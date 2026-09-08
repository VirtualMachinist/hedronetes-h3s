mod common;
use common::Server;
use http_body_util::BodyExt;
use serde_json::{json, Value};

fn pod(name: &str) -> Value {
    json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":name},"spec":{"containers":[{"name":"web","image":"example.invalid/web:v1"}]}})
}
fn deployment(kind: &str, name: &str) -> Value {
    json!({"apiVersion":"apps/v1","kind":kind,"metadata":{"name":name},"spec":{"selector":{"matchLabels":{"app":"web"}},"template":{"metadata":{"labels":{"app":"web"}},"spec":{"containers":[{"name":"web","image":"example.invalid/web:v1"}]}}}})
}
fn service(name: &str) -> Value {
    json!({"apiVersion":"v1","kind":"Service","metadata":{"name":name},"spec":{"selector":{"app":"web"},"ports":[{"port":80}]}})
}

#[tokio::test]
async fn stock_kubectl_protobuf_deployment_applies_empty_scalar_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("api-smoke").await;
    let response = s
        .raw_bytes(
            s.admin(),
            "POST",
            "/apis/apps/v1/namespaces/api-smoke/deployments",
            include_bytes!("fixtures/kubectl-deployment.pb").to_vec(),
            &[("Content-Type", "application/vnd.kubernetes.protobuf")],
        )
        .await;
    let code = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let object: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(code, 201, "{object}");
    assert_eq!(object["spec"]["replicas"], 2);
    assert_eq!(object["spec"]["strategy"]["type"], "RollingUpdate");
    let spec = &object["spec"]["template"]["spec"];
    assert_eq!(spec["restartPolicy"], "Always");
    assert_eq!(spec["dnsPolicy"], "ClusterFirst");
    assert_eq!(spec["schedulerName"], "default-scheduler");
    assert_eq!(spec["containers"][0]["imagePullPolicy"], "IfNotPresent");
}

#[tokio::test]
async fn workload_resources_discovery_scope_and_generated_names() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    for (path, value) in [
        ("/api/v1/namespaces/team-a/pods", pod("web")),
        (
            "/api/v1/nodes",
            json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"worker"},"spec":{"podCIDR":"10.42.1.0/24"}}),
        ),
        (
            "/api/v1/namespaces/team-a/serviceaccounts",
            json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"operator"}}),
        ),
        (
            "/apis/apps/v1/namespaces/team-a/deployments",
            deployment("Deployment", "web"),
        ),
        (
            "/apis/apps/v1/namespaces/team-a/replicasets",
            deployment("ReplicaSet", "web"),
        ),
        (
            "/apis/discovery.k8s.io/v1/namespaces/team-a/endpointslices",
            json!({"apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice","metadata":{"name":"web"},"addressType":"IPv4","endpoints":[{"addresses":["10.42.1.5"]}],"ports":[{"port":80}]}),
        ),
        (
            "/apis/coordination.k8s.io/v1/namespaces/team-a/leases",
            json!({"apiVersion":"coordination.k8s.io/v1","kind":"Lease","metadata":{"name":"worker"},"spec":{"holderIdentity":"worker","leaseDurationSeconds":40}}),
        ),
        (
            "/apis/rbac.authorization.k8s.io/v1/clusterroles",
            json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole","metadata":{"name":"system:controller:test"},"rules":[]}),
        ),
    ] {
        let (code, created) = s.json(s.admin(), "POST", path, value).await;
        assert_eq!(code, 201, "{path}: {created}");
        let named = format!("{path}/{}", created["metadata"]["name"].as_str().unwrap());
        assert_eq!(s.json(s.admin(), "GET", &named, json!({})).await.1, created);
        if named.contains(':') {
            assert_eq!(
                s.json(s.admin(), "GET", &named.replace(':', "%3A"), json!({}))
                    .await
                    .1,
                created
            );
        }
    }
    let (_, groups) = s.json(s.admin(), "GET", "/apis", json!({})).await;
    for group in [
        "apps",
        "discovery.k8s.io",
        "coordination.k8s.io",
        "rbac.authorization.k8s.io",
    ] {
        assert!(groups["groups"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == group));
        assert_eq!(
            s.json(s.admin(), "GET", &format!("/apis/{group}/v1"), json!({}))
                .await
                .0,
            200
        );
    }
    let (_, discovery) = s.json(s.admin(), "GET", "/api/v1", json!({})).await;
    assert!(discovery["resources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v["name"] == "pods/status" && v["verbs"] == json!(["get", "update", "patch"])));
    // A resource literally named status must not be mistaken for a subresource.
    s.configmap("status", "value").await;
    assert_eq!(
        s.json(
            s.admin(),
            "GET",
            "/api/v1/namespaces/team-a/configmaps/status",
            json!({})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        s.json(
            s.admin(),
            "GET",
            "/api/v1/namespaces/team-a/configmaps/status/status",
            json!({})
        )
        .await
        .0,
        404
    );
    let mut generated = pod("unused");
    generated["metadata"] = json!({"generateName":"web-"});
    let (code, created) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/team-a/pods",
            generated,
        )
        .await;
    assert_eq!(code, 201, "{created}");
    assert!(created["metadata"]["name"]
        .as_str()
        .unwrap()
        .starts_with("web-"));
    assert_eq!(
        s.json(s.admin(), "GET", "/api/v1/pods/web?watch=true", json!({}))
            .await
            .0,
        400
    );
    for encoded in ["web%2fstatus", "web%5cstatus", "%2e%2e", "bad%00name"] {
        assert_eq!(
            s.json(
                s.admin(),
                "GET",
                &format!("/api/v1/namespaces/team-a/pods/{encoded}"),
                json!({})
            )
            .await
            .0,
            400
        );
    }
}

#[tokio::test]
async fn spec_status_and_generation_have_separate_write_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let path = "/apis/apps/v1/namespaces/team-a/deployments";
    let mut submitted = deployment("Deployment", "web");
    submitted["metadata"]["generation"] = json!(999);
    submitted["status"] = json!({"availableReplicas":999});
    let (code, original) = s.json(s.admin(), "POST", path, submitted).await;
    assert_eq!(code, 201, "{original}");
    assert_eq!(original["metadata"]["generation"], 1);
    assert!(original["status"]["availableReplicas"].is_null());
    assert_eq!(original["spec"]["replicas"], 1);
    assert_eq!(
        original["spec"]["template"]["spec"]["dnsPolicy"],
        "ClusterFirst"
    );
    let named = format!("{path}/web");
    let (code, changed)=s.patch(s.admin(),&named,"application/merge-patch+json",json!({"spec":{"replicas":2},"status":{"availableReplicas":2},"metadata":{"generation":500}})).await;
    assert_eq!(code, 200, "{changed}");
    assert_eq!(changed["metadata"]["generation"], 2);
    assert!(changed["status"]["availableReplicas"].is_null());
    let (code, reported)=s.patch(s.admin(),&format!("{named}/status"),"application/merge-patch+json",json!({"status":{"availableReplicas":2,"observedGeneration":2},"spec":{"replicas":100},"metadata":{"labels":{"forged":"yes"},"generation":1000}})).await;
    assert_eq!(code, 200, "{reported}");
    assert_eq!(reported["status"]["availableReplicas"], 2);
    assert_eq!(reported["spec"], changed["spec"]);
    assert_eq!(
        reported["metadata"]["labels"],
        changed["metadata"]["labels"]
    );
    assert_eq!(reported["metadata"]["generation"], 2);
    assert_eq!(
        s.json(s.admin(), "PUT", &format!("{named}/status"), changed)
            .await
            .0,
        409
    );
    assert_eq!(
        s.json(s.admin(), "DELETE", &format!("{named}/status"), json!({}))
            .await
            .0,
        405
    );
    assert_eq!(
        s.json(
            s.admin(),
            "GET",
            &format!("{named}/status?watch=true"),
            json!({})
        )
        .await
        .0,
        405
    );
    let mut existing = reported.clone();
    existing["metadata"]["labels"] = json!({"added":"label"});
    let (_, relabeled) = s.json(s.admin(), "PUT", &named, existing).await;
    assert_eq!(relabeled["metadata"]["generation"], 2);
    assert_eq!(relabeled["status"], reported["status"]);
}

#[tokio::test]
async fn status_only_rbac_cannot_change_spec_or_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let path = "/api/v1/namespaces/team-a/pods";
    let (code, p) = s.json(s.admin(), "POST", path, pod("web")).await;
    assert_eq!(code, 201, "{p}");
    assert_eq!(p["status"]["phase"], "Pending");
    let base = "/apis/rbac.authorization.k8s.io/v1/namespaces/team-a";
    let role = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{"name":"reporter"},"rules":[{"apiGroups":[""],"resources":["pods/status"],"verbs":["get","patch","update"]}]});
    let binding = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding","metadata":{"name":"reporter"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":"reporter"},"subjects":[{"apiGroup":"rbac.authorization.k8s.io","kind":"User","name":"reporter"}]});
    assert_eq!(
        s.json(s.admin(), "POST", &format!("{base}/roles"), role)
            .await
            .0,
        201
    );
    assert_eq!(
        s.json(s.admin(), "POST", &format!("{base}/rolebindings"), binding)
            .await
            .0,
        201
    );
    let cert = s.pki.issue_client("reporter", None).unwrap();
    let client = || s.pki.client_config(Some(&cert)).unwrap();
    let named = format!("{path}/web");
    assert_eq!(s.json(client(), "GET", &named, json!({})).await.0, 403);
    assert_eq!(
        s.patch(
            client(),
            &named,
            "application/merge-patch+json",
            json!({"spec":{"nodeName":"other"}})
        )
        .await
        .0,
        403
    );
    let (code,v)=s.patch(client(),&format!("{named}/status"),"application/merge-patch+json",json!({"status":{"phase":"Running"},"spec":{"nodeName":"forged"},"metadata":{"ownerReferences":[{"apiVersion":"v1","kind":"Pod","name":"other","uid":"other"}]}})).await;
    assert_eq!(code, 200, "{v}");
    assert_eq!(v["spec"], p["spec"]);
    assert!(v["metadata"]["ownerReferences"].is_null());
    assert_eq!(v["status"]["phase"], "Running");
}

#[tokio::test]
async fn invalid_workload_specs_and_immutable_updates_do_not_persist() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let path = "/apis/apps/v1/namespaces/team-a/deployments";
    for bad in [
        json!({"replicas":-1}),
        json!({"selector":{"matchLabels":{"app":"other"}}}),
        json!({"selector":{"matchLabels":null}}),
        json!({"template":{"spec":{"containers":[]}}}),
    ] {
        let mut v = deployment("Deployment", "invalid");
        json_patch::merge(&mut v["spec"], &bad);
        assert_eq!(s.json(s.admin(), "POST", path, v).await.0, 422);
    }
    let (_, original) = s
        .json(s.admin(), "POST", path, deployment("Deployment", "web"))
        .await;
    assert_eq!(s.patch(s.admin(),&format!("{path}/web"),"application/merge-patch+json",json!({"spec":{"selector":{"matchLabels":{"app":"new"}},"template":{"metadata":{"labels":{"app":"new"}}}}})).await.0,422);
    assert_eq!(
        s.json(s.admin(), "GET", &format!("{path}/web"), json!({}))
            .await
            .1,
        original
    );
    let pods = "/api/v1/namespaces/team-a/pods";
    let (_, original) = s.json(s.admin(), "POST", pods, pod("web")).await;
    assert_eq!(
        s.patch(
            s.admin(),
            &format!("{pods}/web"),
            "application/merge-patch+json",
            json!({"spec":{"nodeName":"worker"}})
        )
        .await
        .0,
        422
    );
    let (code,changed)=s.patch(s.admin(),&format!("{pods}/web"),"application/json-patch+json",json!([{"op":"replace","path":"/spec/containers/0/image","value":"example.invalid/web:v2"}])).await;
    assert_eq!(code, 200, "{changed}");
    assert_eq!(changed["metadata"]["generation"], 2);
    assert_eq!(changed["metadata"]["uid"], original["metadata"]["uid"]);
}

#[tokio::test]
async fn cluster_ip_allocation_is_unique_durable_and_released_by_delete() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let path = "/api/v1/namespaces/team-a/services";
    let (a, b) = tokio::join!(
        s.json(s.admin(), "POST", path, service("a")),
        s.json(s.admin(), "POST", path, service("b"))
    );
    assert_eq!(a.0, 201, "{}", a.1);
    assert_eq!(b.0, 201, "{}", b.1);
    let ip = a.1["spec"]["clusterIP"].clone();
    assert_ne!(ip, b.1["spec"]["clusterIP"]);
    assert_eq!(a.1["spec"]["clusterIPs"], json!([ip]));
    let mut duplicate = service("duplicate");
    duplicate["spec"]["clusterIP"] = ip.clone();
    assert_eq!(s.json(s.admin(), "POST", path, duplicate).await.0, 422);
    assert_eq!(
        s.patch(
            s.admin(),
            &format!("{path}/a"),
            "application/merge-patch+json",
            json!({"spec":{"clusterIP":"10.43.99.99"}})
        )
        .await
        .0,
        422
    );
    drop(s);
    tokio::task::yield_now().await;
    let s = Server::start(dir.path()).await;
    assert_eq!(
        s.json(s.admin(), "GET", &format!("{path}/a"), json!({}))
            .await
            .1,
        a.1
    );
    let (code, c) = s.json(s.admin(), "POST", path, service("c")).await;
    assert_eq!(code, 201, "{c}");
    assert_ne!(c["spec"]["clusterIP"], ip);
    assert_eq!(
        s.json(s.admin(), "DELETE", &format!("{path}/a"), json!({}))
            .await
            .0,
        200
    );
    let mut requested = service("reused");
    requested["spec"]["clusterIP"] = ip.clone();
    let (code, reused) = s.json(s.admin(), "POST", path, requested).await;
    assert_eq!(code, 201, "{reused}");
    assert_eq!(reused["spec"]["clusterIP"], ip);
    for address in [
        "10.43.0.0",
        "10.43.0.1",
        "10.43.255.255",
        "192.168.1.2",
        "::1",
    ] {
        let mut v = service("invalid");
        v["spec"]["clusterIP"] = json!(address);
        assert_eq!(s.json(s.admin(), "POST", path, v).await.0, 422);
    }
    let mut headless = service("headless");
    headless["spec"]["clusterIP"] = json!("None");
    assert_eq!(s.json(s.admin(), "POST", path, headless).await.0, 201);
}

#[tokio::test]
async fn immutable_configmap_and_secret_data_are_enforced_for_patch() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    for (plural, kind) in [("configmaps", "ConfigMap"), ("secrets", "Secret")] {
        let path = format!("/api/v1/namespaces/team-a/{plural}");
        let value = json!({"apiVersion":"v1","kind":kind,"metadata":{"name":"fixed"},"immutable":true,"data":{"value":"b25l"}});
        let (code, original) = s.json(s.admin(), "POST", &path, value).await;
        assert_eq!(code, 201, "{original}");
        for patch in [json!({"data":{"value":"dHdv"}}), json!({"immutable":false})] {
            assert_eq!(
                s.patch(
                    s.admin(),
                    &format!("{path}/fixed"),
                    "application/merge-patch+json",
                    patch
                )
                .await
                .0,
                422
            );
        }
        assert_eq!(
            s.json(s.admin(), "GET", &format!("{path}/fixed"), json!({}))
                .await
                .1,
            original
        );
    }
}

#[tokio::test]
async fn scheduler_binding_uses_a_distinct_permission_and_one_cas_winner() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let pods = "/api/v1/namespaces/team-a/pods";
    let (_, p) = s.json(s.admin(), "POST", pods, pod("web")).await;
    for node in ["worker-a", "worker-b"] {
        assert_eq!(
            s.json(
                s.admin(),
                "POST",
                "/api/v1/nodes",
                json!({"apiVersion":"v1","kind":"Node","metadata":{"name":node}})
            )
            .await
            .0,
            201
        );
    }
    let base = "/apis/rbac.authorization.k8s.io/v1/namespaces/team-a";
    let role = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{"name":"scheduler"},"rules":[{"apiGroups":[""],"resources":["pods/binding"],"verbs":["create"]}]});
    let binding = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding","metadata":{"name":"scheduler"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":"scheduler"},"subjects":[{"apiGroup":"rbac.authorization.k8s.io","kind":"User","name":"scheduler"}]});
    assert_eq!(
        s.json(s.admin(), "POST", &format!("{base}/roles"), role)
            .await
            .0,
        201
    );
    assert_eq!(
        s.json(s.admin(), "POST", &format!("{base}/rolebindings"), binding)
            .await
            .0,
        201
    );
    let cert = s.pki.issue_client("scheduler", None).unwrap();
    let client = || s.pki.client_config(Some(&cert)).unwrap();
    let named = format!("{pods}/web");
    let endpoint = format!("{named}/binding");
    assert_eq!(s.json(client(), "PUT", &named, p.clone()).await.0, 403);
    let request = |node: &str| json!({"apiVersion":"v1","kind":"Binding","metadata":{"name":"web","uid":p["metadata"]["uid"]},"target":{"kind":"Node","name":node}});
    assert_eq!(
        s.json(client(), "POST", &endpoint, request("missing"))
            .await
            .0,
        404
    );
    let (a, b) = tokio::join!(
        s.json(client(), "POST", &endpoint, request("worker-a")),
        s.json(client(), "POST", &endpoint, request("worker-b"))
    );
    assert!(matches!((a.0, b.0), (201, 409) | (409, 201)), "{a:?} {b:?}");
    let (_, bound) = s.json(s.admin(), "GET", &named, json!({})).await;
    assert_eq!(
        bound["spec"]["nodeName"],
        if a.0 == 201 { "worker-a" } else { "worker-b" }
    );
    assert_eq!(bound["metadata"]["uid"], p["metadata"]["uid"]);
    assert_eq!(
        s.json(client(), "POST", &endpoint, request("worker-a"))
            .await
            .0,
        409
    );
}
