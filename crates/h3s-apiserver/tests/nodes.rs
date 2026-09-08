mod common;
use common::Server;
use http_body_util::BodyExt;
use serde_json::{json, Value};

fn worker(s: &Server, name: &str) -> rustls::ClientConfig {
    let id = s
        .pki
        .issue_client(&format!("system:node:{name}"), Some("system:nodes"))
        .unwrap();
    s.pki.client_config(Some(&id)).unwrap()
}
fn node(name: &str) -> Value {
    json!({"apiVersion":"v1","kind":"Node","metadata":{"name":name,"labels":{"kubernetes.io/hostname":name,"kubernetes.io/os":"linux","kubernetes.io/arch":"arm64"}}})
}
fn pod(name: &str, node: &str) -> Value {
    json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":name},"spec":{"nodeName":node,"automountServiceAccountToken":false,"securityContext":{"runAsNonRoot":true,"seccompProfile":{"type":"RuntimeDefault"}},"containers":[{"name":"web","image":"example.invalid/web:v1","securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]}})
}
async fn broad_node_role(s: &Server) {
    for (resource, value) in [
        (
            "clusterroles",
            json!({"kind":"ClusterRole","metadata":{"name":"test-overbroad-node"},"rules":[{"apiGroups":["*"],"resources":["*"],"verbs":["*"]}]}),
        ),
        (
            "clusterrolebindings",
            json!({"kind":"ClusterRoleBinding","metadata":{"name":"test-overbroad-node"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"ClusterRole","name":"test-overbroad-node"},"subjects":[{"apiGroup":"rbac.authorization.k8s.io","kind":"Group","name":"system:nodes"}]}),
        ),
    ] {
        let mut value = value;
        value["apiVersion"] = "rbac.authorization.k8s.io/v1".into();
        assert_eq!(
            s.json(
                s.admin(),
                "POST",
                &format!("/apis/rbac.authorization.k8s.io/v1/{resource}"),
                value
            )
            .await
            .0,
            201
        );
    }
}

#[tokio::test]
async fn node_registers_own_identity_status_and_lease_without_rbac_grants() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    assert_eq!(
        s.json(worker(&s, "a"), "GET", "/api/v1/nodes/a", json!({}))
            .await
            .0,
        404
    );
    let (code, created) = s
        .json(worker(&s, "a"), "POST", "/api/v1/nodes", node("a"))
        .await;
    assert_eq!(code, 201, "{created}");
    assert_eq!(created["status"], json!({}));
    assert_eq!(s.patch(worker(&s,"a"), "/api/v1/nodes/a", "application/merge-patch+json", json!({"spec":{"podCIDR":"","podCIDRs":[],"taints":[]},"metadata":{"finalizers":[],"ownerReferences":[]}})).await.0, 200);
    assert_eq!(
        s.json(worker(&s, "a"), "POST", "/api/v1/nodes", node("b"))
            .await
            .0,
        403
    );
    let (_, created) = s
        .json(worker(&s, "a"), "GET", "/api/v1/nodes/a", json!({}))
        .await;
    let (code, status) = s.patch(worker(&s,"a"), "/api/v1/nodes/a/status", "application/merge-patch+json", json!({"spec":{"podCIDR":"10.42.99.0/24"},"status":{"conditions":[{"type":"Ready","status":"False","reason":"RuntimeNotReady","message":"runtime not started"}]}})).await;
    assert_eq!(code, 200, "{status}");
    assert_eq!(status["spec"], created["spec"]);
    assert_eq!(status["metadata"]["labels"], created["metadata"]["labels"]);
    assert_eq!(status["status"]["conditions"][0]["status"], "False");
    for path in [
        "/api/v1/nodes",
        "/api/v1/nodes/b",
        "/api/v1/nodes?fieldSelector=metadata.name%3Db",
    ] {
        assert_eq!(
            s.json(worker(&s, "a"), "GET", path, json!({})).await.0,
            403,
            "{path}"
        );
    }
    let (code, list) = s
        .json(
            worker(&s, "a"),
            "GET",
            "/api/v1/nodes?fieldSelector=metadata.name%3Da",
            json!({}),
        )
        .await;
    assert_eq!(code, 200, "{list}");
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    let lease = json!({"apiVersion":"coordination.k8s.io/v1","kind":"Lease","metadata":{"name":"a"},"spec":{"holderIdentity":"a","leaseDurationSeconds":40}});
    let base = "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases";
    assert_eq!(
        s.json(worker(&s, "a"), "POST", base, lease.clone()).await.0,
        201
    );
    let mut bad = lease.clone();
    bad["metadata"]["name"] = "b".into();
    assert_eq!(s.json(worker(&s, "a"), "POST", base, bad).await.0, 403);
    assert_eq!(
        s.json(
            worker(&s, "a"),
            "POST",
            &base.replace("kube-node-lease", "default"),
            lease
        )
        .await
        .0,
        403
    );
    assert_eq!(
        s.patch(
            worker(&s, "a"),
            &format!("{base}/a"),
            "application/merge-patch+json",
            json!({"spec":{"leaseDurationSeconds":45}})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        s.json(worker(&s, "a"), "DELETE", &format!("{base}/a"), json!({}))
            .await
            .0,
        200
    );
    // Identity requires both the node username and verified node group.
    for (name, group) in [("system:node:a", None), ("a", Some("system:nodes"))] {
        let id = s.pki.issue_client(name, group).unwrap();
        assert_eq!(
            s.json(
                s.pki.client_config(Some(&id)).unwrap(),
                "GET",
                "/api/v1/nodes/a",
                json!({})
            )
            .await
            .0,
            403
        );
    }
    let uid = created["metadata"]["uid"].clone();
    drop(s);
    let s = Server::start(dir.path()).await;
    let (code, reopened) = s
        .json(worker(&s, "a"), "GET", "/api/v1/nodes/a", json!({}))
        .await;
    assert_eq!(code, 200);
    assert_eq!(reopened["metadata"]["uid"], uid);
    assert_eq!(reopened["status"], status["status"]);
}

#[tokio::test]
async fn node_restrictions_survive_overbroad_rbac_and_patch_forms() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    for label in [
        "node-restriction.kubernetes.io/trusted",
        "corp.node-restriction.kubernetes.io/trusted",
        "node-role.kubernetes.io/control-plane",
        "k8s.io/private",
        "corp.kubernetes.io/private",
    ] {
        let mut n = node("a");
        n["metadata"]["labels"][label] = "true".into();
        assert_eq!(
            s.json(worker(&s, "a"), "POST", "/api/v1/nodes", n).await.0,
            403,
            "{label}"
        );
    }
    let mut n = node("a");
    n["metadata"]["labels"]["node-restriction.kubernetes.io/trusted"] = "true".into();
    n["spec"] =
        json!({"podCIDR":"10.42.0.0/24","taints":[{"key":"maintenance","effect":"NoSchedule"}]});
    assert_eq!(s.json(s.admin(), "POST", "/api/v1/nodes", n).await.0, 201);
    assert_eq!(
        s.json(s.admin(), "POST", "/api/v1/nodes", node("b"))
            .await
            .0,
        201
    );
    broad_node_role(&s).await;
    for (path, patch) in [
        (
            "/api/v1/nodes/a",
            json!({"metadata":{"labels":{"node-restriction.kubernetes.io/trusted":null}}}),
        ),
        ("/api/v1/nodes/a", json!({"spec":{"taints":[]}})),
        (
            "/api/v1/nodes/a",
            json!({"spec":{"podCIDR":"10.42.2.0/24"}}),
        ),
        (
            "/api/v1/nodes/a",
            json!({"metadata":{"finalizers":["node.example/hold"]}}),
        ),
        (
            "/api/v1/nodes/b/status",
            json!({"status":{"conditions":[]}}),
        ),
        (
            "/api/v1/nodes/b",
            json!({"metadata":{"labels":{"custom":"value"}}}),
        ),
    ] {
        let result = s
            .patch(worker(&s, "a"), path, "application/merge-patch+json", patch)
            .await;
        assert_eq!(result.0, 403, "{path}: {}", result.1);
    }
    assert_eq!(s.patch(worker(&s,"a"), "/api/v1/nodes/a", "application/json-patch+json", json!([{"op":"remove","path":"/metadata/labels/node-restriction.kubernetes.io~1trusted"}])).await.0, 403);
    assert_eq!(
        s.json(worker(&s, "a"), "DELETE", "/api/v1/nodes/a", json!({}))
            .await
            .0,
        403
    );
    let (code, allowed) = s.patch(worker(&s,"a"), "/api/v1/nodes/a", "application/merge-patch+json", json!({"metadata":{"labels":{"example.org/rack":"r1","node.kubernetes.io/instance-type":"studio","topology.kubernetes.io/zone":"local"}}})).await;
    assert_eq!(code, 200, "{allowed}");
    assert_eq!(
        allowed["metadata"]["labels"]["node-restriction.kubernetes.io/trusted"],
        "true"
    );
    let mut replacement = allowed.clone();
    replacement["spec"]["taints"] = json!([]);
    assert_eq!(
        s.json(worker(&s, "a"), "PUT", "/api/v1/nodes/a", replacement)
            .await
            .0,
        403
    );
}

#[tokio::test]
async fn node_reads_only_assigned_pods_and_typed_namespaced_references() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    s.namespace("team-b").await;
    let mut p = pod("owned", "a");
    p["spec"]["volumes"] = json!([
        {"name":"settings","configMap":{"name":"cm-volume"}},
        {"name":"credentials","secret":{"secretName":"secret-volume"}},
        {"name":"projected","projected":{"sources":[{"configMap":{"name":"cm-project"}},{"secret":{"name":"secret-project"}}]}},
        {"name":"csi","csi":{"driver":"example.org/csi","nodePublishSecretRef":{"name":"secret-csi"}}}
    ]);
    p["spec"]["imagePullSecrets"] = json!([{"name":"secret-pull"}]);
    p["spec"]["containers"][0]["env"] = json!([
        {"name":"CM","valueFrom":{"configMapKeyRef":{"name":"cm-env","key":"value"}}},
        {"name":"SECRET","valueFrom":{"secretKeyRef":{"name":"secret-env","key":"value"}}},
        {"name":"LITERAL","value":"unrelated"}
    ]);
    p["spec"]["containers"][0]["envFrom"] =
        json!([{"configMapRef":{"name":"cm-all"}},{"secretRef":{"name":"secret-all"}}]);
    p["spec"]["initContainers"] = json!([{"name":"init","image":"example.invalid/init:v1","securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}},"envFrom":[{"secretRef":{"name":"secret-init"}}]}]);
    p["metadata"]["annotations"]["unrelated"] = "unrelated".into();
    let (code, owned) = s
        .json(s.admin(), "POST", "/api/v1/namespaces/team-a/pods", p)
        .await;
    assert_eq!(code, 201, "{owned}");
    for p in [pod("foreign", "b"), pod("unscheduled", "")] {
        assert_eq!(
            s.json(s.admin(), "POST", "/api/v1/namespaces/team-a/pods", p)
                .await
                .0,
            201
        );
    }
    for (kind, resource, names) in [
        (
            "ConfigMap",
            "configmaps",
            vec!["cm-volume", "cm-project", "cm-env", "cm-all"],
        ),
        (
            "Secret",
            "secrets",
            vec![
                "secret-volume",
                "secret-project",
                "secret-pull",
                "secret-env",
                "secret-all",
                "secret-init",
                "secret-csi",
            ],
        ),
    ] {
        for name in names.into_iter().chain(["unrelated"]) {
            let value = json!({"apiVersion":"v1","kind":kind,"metadata":{"name":name},"data":{"value":"dGVzdA=="}});
            let base = format!("/api/v1/namespaces/team-a/{resource}");
            assert_eq!(s.json(s.admin(), "POST", &base, value.clone()).await.0, 201);
            assert_eq!(
                s.json(s.admin(), "POST", &base.replace("team-a", "team-b"), value)
                    .await
                    .0,
                201
            );
            let expected = if name == "unrelated" { 403 } else { 200 };
            for path in [
                format!("{base}/{name}"),
                format!("{base}?fieldSelector=metadata.name%3D{name}"),
            ] {
                assert_eq!(
                    s.json(worker(&s, "a"), "GET", &path, json!({})).await.0,
                    expected,
                    "{path}"
                );
                assert_eq!(
                    s.json(worker(&s, "b"), "GET", &path, json!({})).await.0,
                    403
                );
                assert_eq!(
                    s.json(
                        worker(&s, "a"),
                        "GET",
                        &path.replace("team-a", "team-b"),
                        json!({})
                    )
                    .await
                    .0,
                    403
                );
            }
        }
        assert_eq!(
            s.json(
                worker(&s, "a"),
                "GET",
                &format!("/api/v1/namespaces/team-a/{resource}"),
                json!({})
            )
            .await
            .0,
            403
        );
    }
    for path in [
        "/api/v1/pods",
        "/api/v1/namespaces/team-a/pods/foreign",
        "/api/v1/namespaces/team-a/pods/unscheduled",
        "/api/v1/pods?fieldSelector=spec.nodeName%21%3Db",
        "/api/v1/pods?labelSelector=node%3Da",
    ] {
        assert_eq!(
            s.json(worker(&s, "a"), "GET", path, json!({})).await.0,
            403,
            "{path}"
        );
    }
    let (code, list) = s
        .json(
            worker(&s, "a"),
            "GET",
            "/api/v1/pods?fieldSelector=spec.nodeName%3Da&limit=1",
            json!({}),
        )
        .await;
    assert_eq!(code, 200, "{list}");
    assert_eq!(list["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        list["items"][0]["metadata"]["uid"],
        owned["metadata"]["uid"]
    );
    assert_eq!(
        s.json(
            s.admin(),
            "DELETE",
            "/api/v1/namespaces/team-a/pods/owned",
            json!({})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        s.json(
            worker(&s, "a"),
            "GET",
            "/api/v1/namespaces/team-a/secrets/secret-env",
            json!({})
        )
        .await
        .0,
        403
    );
}

#[tokio::test]
async fn pod_status_and_deletion_are_bound_to_persisted_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let base = "/api/v1/namespaces/team-a/pods";
    for p in [
        pod("owned", "a"),
        pod("foreign", "b"),
        pod("unscheduled", ""),
    ] {
        assert_eq!(s.json(s.admin(), "POST", base, p).await.0, 201);
    }
    let (code, changed) = s.patch(worker(&s,"a"), &format!("{base}/owned/status"), "application/merge-patch+json", json!({"spec":{"nodeName":"b"},"metadata":{"labels":{"owner":"spoof"}},"status":{"phase":"Running"}})).await;
    assert_eq!(code, 200, "{changed}");
    assert_eq!(changed["spec"]["nodeName"], "a");
    assert!(changed["metadata"]["labels"].is_null());
    broad_node_role(&s).await;
    for name in ["foreign", "unscheduled"] {
        assert_eq!(
            s.patch(
                worker(&s, "a"),
                &format!("{base}/{name}/status"),
                "application/merge-patch+json",
                json!({"spec":{"nodeName":"a"},"status":{"phase":"Running"}})
            )
            .await
            .0,
            403
        );
        assert_eq!(
            s.json(
                worker(&s, "a"),
                "DELETE",
                &format!("{base}/{name}"),
                json!({})
            )
            .await
            .0,
            403
        );
    }
    assert_eq!(
        s.patch(
            worker(&s, "a"),
            &format!("{base}/owned"),
            "application/json-patch+json",
            json!([{"op":"replace","path":"/spec/containers/0/image","value":"example.invalid/changed:v1"}])
        )
        .await
        .0,
        403
    );
    assert_eq!(s.json(worker(&s,"a"), "POST", &format!("{base}/unscheduled/binding"), json!({"apiVersion":"v1","kind":"Binding","metadata":{"name":"unscheduled"},"target":{"kind":"Node","name":"a"}})).await.0, 403);
    assert_eq!(
        s.json(worker(&s, "a"), "POST", base, pod("injected", "a"))
            .await
            .0,
        403
    );
    assert_eq!(
        s.json(
            worker(&s, "a"),
            "DELETE",
            &format!("{base}/owned"),
            json!({"preconditions":{"uid":"wrong"}})
        )
        .await
        .0,
        409
    );
    assert_eq!(
        s.json(
            worker(&s, "a"),
            "DELETE",
            &format!("{base}/owned"),
            json!({})
        )
        .await
        .0,
        200
    );
}

#[tokio::test]
async fn node_pod_watch_filters_recreated_foreign_assignment() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let base = "/api/v1/namespaces/team-a/pods";
    let (_, list) = s.json(s.admin(), "GET", base, json!({})).await;
    let rv = list["metadata"]["resourceVersion"].as_str().unwrap();
    assert_eq!(
        s.json(s.admin(), "POST", base, pod("owned", "a")).await.0,
        201
    );
    assert_eq!(
        s.json(s.admin(), "POST", base, pod("foreign", "b")).await.0,
        201
    );
    // Name-only read is related at admission. Subsequently recreating that name
    // on another node cannot broaden an already authorized watch.
    let config = worker(&s, "a");
    let watch = s.raw(config, "GET", &format!("{base}?watch=true&fieldSelector=metadata.name%3Downed&resourceVersion={rv}&timeoutSeconds=1"), json!({}), &[]).await;
    assert_eq!(watch.status(), 200);
    assert_eq!(
        s.json(s.admin(), "DELETE", &format!("{base}/owned"), json!({}))
            .await
            .0,
        200
    );
    assert_eq!(
        s.json(s.admin(), "POST", base, pod("owned", "b")).await.0,
        201
    );
    let data = watch.into_body().collect().await.unwrap().to_bytes();
    let events: Vec<Value> = String::from_utf8(data.to_vec())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        events
            .iter()
            .map(|v| v["type"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["ADDED", "DELETED"]
    );
    for event in events {
        assert_eq!(event["object"]["spec"]["nodeName"], "a");
    }
}

#[tokio::test]
async fn node_secret_watch_stops_before_delivering_data_after_relationship_revocation() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let mut p = pod("owned", "a");
    p["spec"]["imagePullSecrets"] = json!([{"name":"pull"}]);
    assert_eq!(
        s.json(s.admin(), "POST", "/api/v1/namespaces/team-a/pods", p)
            .await
            .0,
        201
    );
    let base = "/api/v1/namespaces/team-a/secrets";
    let (_, list) = s.json(s.admin(), "GET", base, json!({})).await;
    let rv = list["metadata"]["resourceVersion"].as_str().unwrap();
    assert_eq!(s.json(s.admin(), "POST", base, json!({"apiVersion":"v1","kind":"Secret","metadata":{"name":"pull"},"data":{"value":"YmVmb3Jl"}})).await.0, 201);
    let response = s.raw(worker(&s,"a"), "GET", &format!("{base}?watch=true&fieldSelector=metadata.name%3Dpull&resourceVersion={rv}&timeoutSeconds=2"), json!({}), &[]).await;
    assert_eq!(response.status(), 200);
    let mut body = response.into_body();
    let first = body.frame().await.unwrap().unwrap().into_data().unwrap();
    assert!(String::from_utf8(first.to_vec()).unwrap().contains("ADDED"));
    assert_eq!(
        s.json(
            s.admin(),
            "DELETE",
            "/api/v1/namespaces/team-a/pods/owned",
            json!({})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        s.patch(
            s.admin(),
            &format!("{base}/pull"),
            "application/merge-patch+json",
            json!({"data":{"value":"YWZ0ZXI="}})
        )
        .await
        .0,
        200
    );
    let rest = body.collect().await.unwrap().to_bytes();
    let text = String::from_utf8(rest.to_vec()).unwrap();
    assert!(!text.contains("YWZ0ZXI="), "revoked data leaked");
    let event: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(event["type"], "ERROR");
    assert_eq!(event["object"]["code"], 403);
}

#[tokio::test]
async fn flannel_status_patches_preserve_conditions_and_enforce_node_boundaries() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    for name in ["network-a", "network-b"] {
        let (code, value) = s
            .json(worker(&s, name), "POST", "/api/v1/nodes", node(name))
            .await;
        assert_eq!(code, 201, "{value}");
    }
    broad_node_role(&s).await;
    let main = "/api/v1/nodes/network-a";
    let status = format!("{main}/status");
    let smp = "application/strategic-merge-patch+json";
    let (code, allocated) = s.patch(s.admin(), main, smp, json!({"spec":{"podCIDR":"10.42.1.0/24","podCIDRs":["10.42.1.0/24"],"taints":[{"key":"test","effect":"NoSchedule"}]},"metadata":{"annotations":{"preserve":"value"},"labels":{"node-restriction.kubernetes.io/trusted":"existing"}}})).await;
    assert_eq!(code, 200, "{allocated}");
    assert_eq!(s.patch(worker(&s,"network-a"), &status, smp, json!({"status":{"conditions":[{"type":"Ready","status":"True","reason":"KubeletReady"},{"type":"NetworkUnavailable","status":"True","reason":"NoRouteCreated"}]}})).await.0, 200);
    // The actual fields and request shape emitted by Flannel's kube subnet
    // manager, plus its NetworkUnavailable condition update. No admin identity.
    let patch = json!({"metadata":{"annotations":{"flannel.alpha.coreos.com/backend-data":"{\"VNI\":1,\"VtepMAC\":\"de:ad:be:ef:00:01\"}","flannel.alpha.coreos.com/backend-type":"vxlan","flannel.alpha.coreos.com/public-ip":"192.168.104.3","flannel.alpha.coreos.com/kube-subnet-manager":"true"}},"status":{"conditions":[{"type":"NetworkUnavailable","status":"False","reason":"FlannelIsUp"}]}});
    let (code, ready) = s.patch(worker(&s, "network-a"), &status, smp, patch).await;
    assert_eq!(code, 200, "{ready}");
    assert_eq!(ready["metadata"]["annotations"]["preserve"], "value");
    assert_eq!(
        ready["metadata"]["annotations"]["flannel.alpha.coreos.com/backend-type"],
        "vxlan"
    );
    assert_eq!(ready["spec"], allocated["spec"]);
    let conditions = ready["status"]["conditions"].as_array().unwrap();
    assert_eq!(conditions.len(), 2);
    assert!(conditions
        .iter()
        .any(|c| c["type"] == "Ready" && c["status"] == "True" && c["reason"] == "KubeletReady"));
    assert!(conditions
        .iter()
        .any(|c| c["type"] == "NetworkUnavailable" && c["status"] == "False"));
    for metadata in [
        json!({"labels":null}),
        json!({"labels":{"node-restriction.kubernetes.io/trusted":"true"}}),
        json!({"labels":{"node-role.kubernetes.io/control-plane":"true"}}),
        json!({"ownerReferences":[{"apiVersion":"v1","kind":"Node","name":"network-b","uid":"foreign","controller":true}]}),
        json!({"finalizers":["test/hold"]}),
        json!({"deletionTimestamp":"2026-09-08T00:00:00Z"}),
        json!({"deletionGracePeriodSeconds":0}),
    ] {
        for kind in [smp, "application/merge-patch+json"] {
            let (code, value) = s
                .patch(
                    worker(&s, "network-a"),
                    &status,
                    kind,
                    json!({"metadata":metadata}),
                )
                .await;
            assert_eq!(code, 403, "{value}");
        }
    }
    assert_eq!(
        s.patch(
            worker(&s, "network-b"),
            &status,
            smp,
            json!({"metadata":{"annotations":{"takeover":"yes"}}})
        )
        .await
        .0,
        403
    );
    assert_eq!(s.patch(worker(&s,"network-a"),&status,smp,json!({"metadata":{"resourceVersion":allocated["metadata"]["resourceVersion"]},"status":{"conditions":[]}})).await.0,409);
    // Desired-state writes through /status are reset, while the same network
    // allocation change through the main endpoint is denied even with broad RBAC.
    assert_eq!(
        s.patch(
            worker(&s, "network-a"),
            main,
            smp,
            json!({"spec":{"podCIDR":"10.42.9.0/24"}})
        )
        .await
        .0,
        403
    );
    let (code, unchanged) = s
        .patch(
            worker(&s, "network-a"),
            &status,
            smp,
            json!({"spec":{"podCIDR":"10.42.9.0/24","taints":[]}}),
        )
        .await;
    assert_eq!(code, 200, "{unchanged}");
    assert_eq!(unchanged["spec"], allocated["spec"]);
    assert_eq!(
        unchanged["metadata"]["annotations"],
        ready["metadata"]["annotations"]
    );
    drop(s);
    let s = Server::start(dir.path()).await;
    let (code, reopened) = s
        .json(worker(&s, "network-a"), "GET", main, json!({}))
        .await;
    assert_eq!(code, 200);
    assert_eq!(reopened, unchanged);
}
