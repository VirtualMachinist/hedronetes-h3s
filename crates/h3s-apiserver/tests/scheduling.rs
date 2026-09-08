mod common;
use common::Server;
use serde_json::{json, Value};
async fn client(s: &Server) -> kube::Client {
    let id = s
        .pki
        .issue_client(h3s_scheduler::SCHEDULER_ID, None)
        .unwrap();
    h3s_controllers::client_from_kubeconfig(&s.pki.kubeconfig(&s.endpoint(), &id).unwrap())
        .await
        .unwrap()
}
async fn node(s: &Server) -> Value {
    let (code,mut node)=s.json(s.admin(),"POST","/api/v1/nodes",json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"worker","labels":{"zone":"a"}}})).await;
    assert_eq!(code, 201, "{node}");
    node["status"] = json!({"allocatable":{"cpu":"1","memory":"128Mi","pods":"2"},"conditions":[{"type":"Ready","status":"True"}]});
    assert_eq!(
        s.json(
            s.admin(),
            "PUT",
            "/api/v1/nodes/worker/status",
            node.clone()
        )
        .await
        .0,
        200
    );
    let(code,lease)=s.json(s.admin(),"POST","/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases",json!({"apiVersion":"coordination.k8s.io/v1","kind":"Lease","metadata":{"name":"worker","ownerReferences":[{"apiVersion":"v1","kind":"Node","name":"worker","uid":node["metadata"]["uid"]}]},"spec":{"holderIdentity":"worker","leaseDurationSeconds":40,"renewTime":time::OffsetDateTime::now_utc().format(&time::format_description::well_known::Rfc3339).unwrap()}})).await;
    assert_eq!(code, 201, "{lease}");
    node
}
async fn pod(s: &Server, name: &str, extra: Value) -> Value {
    let mut p = json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":name},"spec":{"automountServiceAccountToken":false,"securityContext":{"runAsNonRoot":true,"seccompProfile":{"type":"RuntimeDefault"}},"containers":[{"name":"web","image":"example.invalid/web:v1","resources":{"limits":{"cpu":"700m","memory":"64Mi"}},"securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]}});
    for (k, v) in extra.as_object().unwrap() {
        p["spec"][k] = v.clone();
    }
    let (code, p) = s
        .json(s.admin(), "POST", "/api/v1/namespaces/default/pods", p)
        .await;
    assert_eq!(code, 201, "{p}");
    p
}
async fn pods(s: &Server) -> Vec<Value> {
    s.json(
        s.admin(),
        "GET",
        "/api/v1/namespaces/default/pods",
        json!({}),
    )
    .await
    .1["items"]
        .as_array()
        .unwrap()
        .clone()
}
#[tokio::test]
async fn scoped_scheduler_binds_reserves_recovers_and_preserves_foreign_work() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    node(&s).await;
    pod(&s, "first", json!({"nodeSelector":{"zone":"a"}})).await;
    pod(&s, "second", json!({"nodeSelector":{"zone":"a"}})).await;
    pod(&s, "mismatch", json!({"nodeSelector":{"zone":"b"}})).await;
    pod(&s, "foreign", json!({"schedulerName":"other-scheduler"})).await;
    let c = client(&s).await;
    assert_eq!(h3s_scheduler::reconcile_once(c.clone()).await.unwrap(), 1);
    let all = pods(&s).await;
    let bound: Vec<_> = all
        .iter()
        .filter(|p| p["spec"]["nodeName"] == "worker")
        .collect();
    assert_eq!(bound.len(), 1);
    let first = bound[0].clone();
    assert!(first["status"]["conditions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["type"] == "PodScheduled" && c["status"] == "True"));
    assert!(all
        .iter()
        .find(|p| p["metadata"]["name"] == "foreign")
        .unwrap()["status"]["conditions"]
        .is_null());
    assert_eq!(h3s_scheduler::reconcile_once(c).await.unwrap(), 0);
    // New process/client must reconstruct reservations from persisted bound Pods.
    drop(s);
    let s = Server::start(dir.path()).await;
    let c = client(&s).await;
    assert_eq!(h3s_scheduler::reconcile_once(c.clone()).await.unwrap(), 0);
    let path = format!(
        "/api/v1/namespaces/default/pods/{}",
        first["metadata"]["name"].as_str().unwrap()
    );
    assert_eq!(
        s.json(
            s.admin(),
            "DELETE",
            &path,
            json!({"preconditions":{"uid":first["metadata"]["uid"]}})
        )
        .await
        .0,
        200
    );
    assert_eq!(h3s_scheduler::reconcile_once(c.clone()).await.unwrap(), 1);
    let all = pods(&s).await;
    assert_eq!(
        all.iter()
            .filter(|p| p["spec"]["nodeName"] == "worker")
            .count(),
        1
    );
    let id = s
        .pki
        .issue_client(h3s_scheduler::SCHEDULER_ID, None)
        .unwrap();
    for (method, path, value) in [
        ("GET", "/api/v1/namespaces/default/secrets", json!({})),
        (
            "DELETE",
            "/api/v1/namespaces/default/pods/mismatch",
            json!({}),
        ),
        (
            "POST",
            "/api/v1/nodes",
            json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"forbidden"}}),
        ),
        (
            "PUT",
            "/api/v1/namespaces/default/pods/mismatch",
            all.iter()
                .find(|p| p["metadata"]["name"] == "mismatch")
                .unwrap()
                .clone(),
        ),
    ] {
        assert_eq!(
            s.json(s.pki.client_config(Some(&id)).unwrap(), method, path, value)
                .await
                .0,
            403,
            "{method} {path}"
        );
    }
}
