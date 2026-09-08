mod common;
use common::Server;
use kube::Client;
use serde_json::{json, Value};
async fn client(s: &Server, id: &str) -> Client {
    let id = s.pki.issue_client(id, None).unwrap();
    h3s_controllers::client_from_kubeconfig(&s.pki.kubeconfig(&s.endpoint(), &id).unwrap())
        .await
        .unwrap()
}
async fn request(s: &Server, method: &str, path: &str, value: Value) -> Value {
    let (code, v) = s.json(s.admin(), method, path, value).await;
    assert!(matches!(code, 200 | 201), "{method} {path}: {code} {v}");
    v
}
async fn objects(s: &Server, plural: &str) -> Vec<Value> {
    let prefix = if plural == "pods" {
        "/api/v1"
    } else {
        "/apis/apps/v1"
    };
    request(
        s,
        "GET",
        &format!("{prefix}/namespaces/default/{plural}"),
        json!({}),
    )
    .await["items"]
        .as_array()
        .unwrap()
        .clone()
}
fn spec() -> Value {
    json!({"automountServiceAccountToken":false,"enableServiceLinks":false,"dnsPolicy":"Default","securityContext":{"runAsNonRoot":true,"seccompProfile":{"type":"RuntimeDefault"}},"containers":[{"name":"web","image":"example.invalid/web:v1","securityContext":{"runAsUser":65534,"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]})
}
async fn create(s: &Server) -> Value {
    request(s,"POST","/apis/apps/v1/namespaces/default/deployments",json!({"apiVersion":"apps/v1","kind":"Deployment","metadata":{"name":"web"},"spec":{"replicas":2,"selector":{"matchLabels":{"app":"web"}},"strategy":{"type":"RollingUpdate","rollingUpdate":{"maxSurge":1,"maxUnavailable":0}},"template":{"metadata":{"labels":{"app":"web"}},"spec":spec()}}})).await
}
async fn tick(s: &Server, dep: &Client, rs: &Client, gc: &Client) {
    h3s_controllers::deployment_once(dep.clone(), "default", "web")
        .await
        .unwrap();
    for set in objects(s, "replicasets").await {
        h3s_controllers::replicaset_once(
            rs.clone(),
            "default",
            set["metadata"]["name"].as_str().unwrap(),
        )
        .await
        .unwrap();
    }
    h3s_controllers::gc_once(gc.clone()).await.unwrap();
    h3s_controllers::deployment_once(dep.clone(), "default", "web")
        .await
        .unwrap();
}
async fn mark_ready(s: &Server, mut pod: Value) {
    let name = pod["metadata"]["name"].as_str().unwrap().to_owned();
    pod["status"] = json!({"phase":"Running","conditions":[{"type":"Ready","status":"True","lastTransitionTime":"2020-01-01T00:00:00Z"}]});
    request(
        s,
        "PUT",
        &format!("/api/v1/namespaces/default/pods/{name}/status"),
        pod,
    )
    .await;
}
async fn remove(s: &Server, plural: &str, p: &Value) {
    let prefix = if plural == "pods" {
        "/api/v1"
    } else {
        "/apis/apps/v1"
    };
    request(
        s,
        "DELETE",
        &format!(
            "{prefix}/namespaces/default/{plural}/{}",
            p["metadata"]["name"].as_str().unwrap()
        ),
        json!({"preconditions":{"uid":p["metadata"]["uid"]},"propagationPolicy":"Background"}),
    )
    .await;
}
#[tokio::test]
async fn real_api_workload_rollout_scale_replacement_restart_and_collection() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    create(&s).await;
    let dep = client(&s, h3s_controllers::DEPLOYMENT_CONTROLLER_ID).await;
    let rs = client(&s, h3s_controllers::REPLICASET_CONTROLLER_ID).await;
    let gc = client(&s, h3s_controllers::WORKLOAD_GC_ID).await;
    for _ in 0..4 {
        tick(&s, &dep, &rs, &gc).await;
    }
    let pods = objects(&s, "pods").await;
    assert_eq!(pods.len(), 2);
    assert!(pods
        .iter()
        .all(|p| p["metadata"]["ownerReferences"][0]["kind"] == "ReplicaSet"));
    for p in pods {
        mark_ready(&s, p).await;
    }
    tick(&s, &dep, &rs, &gc).await;
    let d = request(
        &s,
        "GET",
        "/apis/apps/v1/namespaces/default/deployments/web",
        json!({}),
    )
    .await;
    assert_eq!(d["status"]["availableReplicas"], 2);
    assert_eq!(d["status"]["updatedReplicas"], 2);
    let victim = objects(&s, "pods").await[0].clone();
    remove(&s, "pods", &victim).await;
    tick(&s, &dep, &rs, &gc).await;
    let pods = objects(&s, "pods").await;
    assert_eq!(pods.len(), 2);
    assert!(pods
        .iter()
        .all(|p| p["metadata"]["uid"] != victim["metadata"]["uid"]));
    for p in pods {
        mark_ready(&s, p).await;
    }
    tick(&s, &dep, &rs, &gc).await;
    let (code, _) = s
        .patch(
            s.admin(),
            "/apis/apps/v1/namespaces/default/deployments/web",
            "application/merge-patch+json",
            json!({"spec":{"replicas":3}}),
        )
        .await;
    assert_eq!(code, 200);
    for _ in 0..3 {
        tick(&s, &dep, &rs, &gc).await;
    }
    assert_eq!(objects(&s, "pods").await.len(), 3);
    for p in objects(&s, "pods").await {
        mark_ready(&s, p).await;
    }
    tick(&s, &dep, &rs, &gc).await;
    // Restart API and use fresh controller clients; existing owned Pods must be adopted unchanged.
    let ids: Vec<_> = objects(&s, "pods")
        .await
        .iter()
        .map(|p| p["metadata"]["uid"].clone())
        .collect();
    drop(s);
    let s = Server::start(dir.path()).await;
    let dep = client(&s, h3s_controllers::DEPLOYMENT_CONTROLLER_ID).await;
    let rs = client(&s, h3s_controllers::REPLICASET_CONTROLLER_ID).await;
    let gc = client(&s, h3s_controllers::WORKLOAD_GC_ID).await;
    tick(&s, &dep, &rs, &gc).await;
    assert!(objects(&s, "pods")
        .await
        .iter()
        .all(|p| ids.contains(&p["metadata"]["uid"])));
    assert_eq!(
        s.patch(
            s.admin(),
            "/apis/apps/v1/namespaces/default/deployments/web",
            "application/merge-patch+json",
            json!({"spec":{"template":{"metadata":{"annotations":{"rollout":"two"}}}}})
        )
        .await
        .0,
        200
    );
    for _ in 0..5 {
        tick(&s, &dep, &rs, &gc).await;
    }
    let pods = objects(&s, "pods").await;
    assert_eq!(pods.len(), 4);
    assert_eq!(
        pods.iter()
            .filter(|p| ids.contains(&p["metadata"]["uid"]))
            .count(),
        3,
        "ready old replicas must be retained while surge Pod is not ready"
    );
    for _ in 0..12 {
        for p in objects(&s, "pods").await {
            mark_ready(&s, p).await;
        }
        tick(&s, &dep, &rs, &gc).await;
        assert!(objects(&s, "pods").await.len() <= 4);
    }
    let pods = objects(&s, "pods").await;
    assert_eq!(pods.len(), 3);
    assert!(pods
        .iter()
        .all(|p| p["metadata"]["annotations"]["rollout"] == "two"));
    let d = request(
        &s,
        "GET",
        "/apis/apps/v1/namespaces/default/deployments/web",
        json!({}),
    )
    .await;
    assert_eq!(d["status"]["availableReplicas"], 3);
    assert_eq!(objects(&s, "replicasets").await.len(), 2);
    assert_eq!(
        s.patch(
            s.admin(),
            "/apis/apps/v1/namespaces/default/deployments/web",
            "application/merge-patch+json",
            json!({"spec":{"replicas":1}})
        )
        .await
        .0,
        200
    );
    for _ in 0..4 {
        tick(&s, &dep, &rs, &gc).await;
    }
    assert_eq!(objects(&s, "pods").await.len(), 1);
    // Preserve an unrelated same-label Pod owned by another controller.
    let foreign=request(&s,"POST","/api/v1/namespaces/default/pods",json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"foreign","labels":{"app":"web"},"ownerReferences":[{"apiVersion":"apps/v1","kind":"StatefulSet","name":"other","uid":"other-uid","controller":true}]},"spec":spec()})).await;
    remove(&s, "deployments", &d).await;
    for _ in 0..3 {
        h3s_controllers::gc_once(gc.clone()).await.unwrap();
    }
    assert!(objects(&s, "replicasets").await.is_empty());
    let pods = objects(&s, "pods").await;
    assert_eq!(pods.len(), 1);
    assert_eq!(pods[0]["metadata"]["uid"], foreign["metadata"]["uid"]);
}
#[tokio::test]
async fn controller_rbac_and_unsupported_delete_policy_are_explicit() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    let d = create(&s).await;
    for name in [
        h3s_controllers::DEPLOYMENT_CONTROLLER_ID,
        h3s_controllers::REPLICASET_CONTROLLER_ID,
        h3s_controllers::WORKLOAD_GC_ID,
    ] {
        let id = s.pki.issue_client(name, None).unwrap();
        for (method, path, value) in [
            ("GET", "/api/v1/namespaces/default/secrets", json!({})),
            (
                "POST",
                "/api/v1/nodes",
                json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"forbidden"}}),
            ),
            (
                "DELETE",
                "/apis/apps/v1/namespaces/default/deployments/web",
                json!({}),
            ),
            (
                "PUT",
                "/apis/apps/v1/namespaces/default/deployments/web",
                d.clone(),
            ),
        ] {
            assert_eq!(
                s.json(s.pki.client_config(Some(&id)).unwrap(), method, path, value)
                    .await
                    .0,
                403,
                "{name} {method} {path}"
            );
        }
    }
    for policy in ["Orphan", "Foreground"] {
        assert_eq!(
            s.json(
                s.admin(),
                "DELETE",
                "/apis/apps/v1/namespaces/default/deployments/web",
                json!({"propagationPolicy":policy})
            )
            .await
            .0,
            422
        );
    }
    assert_eq!(
        request(
            &s,
            "GET",
            "/apis/apps/v1/namespaces/default/deployments/web",
            json!({})
        )
        .await["metadata"]["uid"],
        d["metadata"]["uid"]
    );
}

#[tokio::test]
async fn replicaset_claims_releases_and_reports_real_admission_failure() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    let rsclient = client(&s, h3s_controllers::REPLICASET_CONTROLLER_ID).await;
    let orphan=request(&s,"POST","/api/v1/namespaces/default/pods",json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":"orphan","labels":{"app":"claim"}},"spec":spec()})).await;
    let set=request(&s,"POST","/apis/apps/v1/namespaces/default/replicasets",json!({"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"name":"claim"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"claim"}},"template":{"metadata":{"labels":{"app":"claim"}},"spec":spec()}}})).await;
    for _ in 0..2 {
        h3s_controllers::replicaset_once(rsclient.clone(), "default", "claim")
            .await
            .unwrap();
    }
    let all = objects(&s, "pods").await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0]["metadata"]["uid"], orphan["metadata"]["uid"]);
    assert_eq!(
        all[0]["metadata"]["ownerReferences"][0]["uid"],
        set["metadata"]["uid"]
    );
    assert_eq!(
        s.patch(
            s.admin(),
            "/api/v1/namespaces/default/pods/orphan",
            "application/merge-patch+json",
            json!({"metadata":{"labels":{"app":"released"}}})
        )
        .await
        .0,
        200
    );
    for _ in 0..2 {
        h3s_controllers::replicaset_once(rsclient.clone(), "default", "claim")
            .await
            .unwrap();
    }
    let all = objects(&s, "pods").await;
    assert_eq!(all.len(), 2);
    assert_eq!(
        all.iter()
            .find(|p| p["metadata"]["name"] == "orphan")
            .unwrap()["metadata"]["ownerReferences"],
        json!([])
    );
    let mut unsafe_spec = spec();
    unsafe_spec["containers"][0]["securityContext"]["privileged"] = true.into();
    let set=request(&s,"POST","/apis/apps/v1/namespaces/default/replicasets",json!({"apiVersion":"apps/v1","kind":"ReplicaSet","metadata":{"name":"denied"},"spec":{"replicas":1,"selector":{"matchLabels":{"app":"denied"}},"template":{"metadata":{"labels":{"app":"denied"}},"spec":unsafe_spec}}})).await;
    assert!(
        h3s_controllers::replicaset_once(rsclient.clone(), "default", "denied")
            .await
            .is_err()
    );
    let failed = request(
        &s,
        "GET",
        "/apis/apps/v1/namespaces/default/replicasets/denied",
        json!({}),
    )
    .await;
    assert_eq!(failed["status"]["replicas"], 0);
    assert_eq!(failed["status"]["conditions"][0]["type"], "ReplicaFailure");
    assert!(failed["status"]["conditions"][0]["message"]
        .as_str()
        .unwrap()
        .contains("403"));
    let mut fixed = failed;
    fixed["spec"]["template"]["spec"] = spec();
    request(
        &s,
        "PUT",
        "/apis/apps/v1/namespaces/default/replicasets/denied",
        fixed,
    )
    .await;
    h3s_controllers::replicaset_once(rsclient, "default", "denied")
        .await
        .unwrap();
    let recovered = request(
        &s,
        "GET",
        "/apis/apps/v1/namespaces/default/replicasets/denied",
        json!({}),
    )
    .await;
    assert_eq!(recovered["metadata"]["uid"], set["metadata"]["uid"]);
    assert_eq!(recovered["status"]["replicas"], 1);
    assert!(recovered["status"]["conditions"].is_null());
}
