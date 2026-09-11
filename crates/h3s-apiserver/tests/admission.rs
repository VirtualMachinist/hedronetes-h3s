mod common;
use common::Server;
use serde_json::{json, Value};

fn pod(name: &str) -> Value {
    json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":name},"spec":{
        "securityContext":{"runAsNonRoot":true,"runAsUser":65534,"seccompProfile":{"type":"RuntimeDefault"}},
        "containers":[{"name":"web","image":"example.invalid/web:v1","securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]
    }})
}
fn overlay(base: &mut Value, patch: Value) {
    if let (Some(base), Some(patch)) = (base.as_object_mut(), patch.as_object()) {
        for (key, value) in patch {
            overlay(base.entry(key).or_insert(Value::Null), value.clone())
        }
    } else {
        *base = patch
    }
}
async fn create(s: &Server, ns: &str, value: Value) -> (u16, Value) {
    s.json(
        s.admin(),
        "POST",
        &format!("/api/v1/namespaces/{ns}/pods"),
        value,
    )
    .await
}

#[tokio::test]
async fn restricted_policy_covers_pod_fields_and_leaves_no_denied_objects() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("secure").await;
    let patches = [
        json!({"hostNetwork":true}),
        json!({"hostPID":true}),
        json!({"hostIPC":true}),
        json!({"os":{"name":"windows"}}),
        json!({"volumes":[{"name":"host","hostPath":{"path":"/"}}]}),
        json!({"volumes":[{"name":"disk","nfs":{"server":"example.invalid","path":"/"}}]}),
        json!({"securityContext":{"runAsUser":0}}),
        json!({"securityContext":{"runAsNonRoot":false}}),
        json!({"securityContext":{"seccompProfile":{"type":"Unconfined"}}}),
        json!({"securityContext":{"appArmorProfile":{"type":"Unconfined"}}}),
        json!({"securityContext":{"seLinuxOptions":{"user":"system_u"}}}),
        json!({"securityContext":{"seLinuxOptions":{"role":"system_r"}}}),
        json!({"securityContext":{"seLinuxOptions":{"type":"unconfined_t"}}}),
        json!({"securityContext":{"windowsOptions":{"hostProcess":true}}}),
        json!({"securityContext":{"sysctls":[{"name":"kernel.core_pattern","value":"/tmp/core"}]}}),
    ];
    for (i, patch) in patches.into_iter().enumerate() {
        let name = format!("denied-{i}");
        let mut value = pod(&name);
        overlay(&mut value["spec"], patch);
        let (code, response) = create(&s, "secure", value).await;
        assert_eq!(code, 403, "case {i}: {response}");
        assert!(response["message"]
            .as_str()
            .unwrap()
            .contains("PodSecurity restricted:v1.34"));
        assert_eq!(
            s.json(
                s.admin(),
                "GET",
                &format!("/api/v1/namespaces/secure/pods/{name}"),
                json!({})
            )
            .await
            .0,
            404
        );
    }
    let mut value = pod("annotations");
    value["metadata"]["annotations"] =
        json!({"container.apparmor.security.beta.kubernetes.io/web":"unconfined"});
    assert_eq!(create(&s, "secure", value).await.0, 403);
    let (code, accepted) = create(&s, "secure", pod("accepted")).await;
    assert_eq!(code, 201, "{accepted}");
    assert_eq!(accepted["status"]["phase"], "Pending");
}

#[tokio::test]
async fn restricted_policy_checks_every_container_class_and_security_override() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("secure").await;
    let patches = [
        json!({"securityContext":{"privileged":true}}),
        json!({"securityContext":{"allowPrivilegeEscalation":true}}),
        json!({"securityContext":{"allowPrivilegeEscalation":null}}),
        json!({"securityContext":{"capabilities":{"drop":[]}}}),
        json!({"securityContext":{"capabilities":{"add":["NET_ADMIN"]}}}),
        json!({"securityContext":{"runAsUser":0}}),
        json!({"securityContext":{"runAsNonRoot":false}}),
        json!({"securityContext":{"seccompProfile":{"type":"Unconfined"}}}),
        json!({"securityContext":{"appArmorProfile":{"type":"Unconfined"}}}),
        json!({"securityContext":{"seLinuxOptions":{"type":"unconfined_t"}}}),
        json!({"securityContext":{"procMount":"Unmasked"}}),
        json!({"securityContext":{"windowsOptions":{"hostProcess":true}}}),
        json!({"ports":[{"containerPort":8080,"hostPort":8080}]}),
        json!({"livenessProbe":{"httpGet":{"path":"/","port":80,"host":"127.0.0.1"}}}),
        json!({"readinessProbe":{"tcpSocket":{"port":80,"host":"127.0.0.1"}}}),
        json!({"startupProbe":{"httpGet":{"path":"/","port":80,"host":"127.0.0.1"}}}),
        json!({"lifecycle":{"postStart":{"httpGet":{"path":"/","port":80,"host":"127.0.0.1"}}}}),
        json!({"lifecycle":{"preStop":{"tcpSocket":{"port":80,"host":"127.0.0.1"}}}}),
    ];
    for field in ["containers", "initContainers", "ephemeralContainers"] {
        for (i, patch) in patches.iter().enumerate() {
            let mut value = pod(&format!("denied-{i}"));
            let mut container = value["spec"]["containers"][0].clone();
            container["name"] = json!("check");
            overlay(&mut container, patch.clone());
            value["spec"][field] = json!([container]);
            let (code, response) = create(&s, "secure", value).await;
            assert_eq!(code, 403, "{field} case {i}: {response}");
        }
    }
}

#[tokio::test]
async fn restricted_inheritance_and_allowed_overrides_work() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("secure").await;
    // Pod Security inheritance: the container states its own identity and
    // seccomp profile; the Pod level carries only a group.
    let mut value = pod("per-container");
    value["spec"]["securityContext"] = json!({"runAsGroup":1000});
    let context = &mut value["spec"]["containers"][0]["securityContext"];
    context["runAsNonRoot"] = json!(true);
    context["runAsUser"] = json!(1000);
    context["seccompProfile"] = json!({"type":"RuntimeDefault"});
    value["spec"]["volumes"] = json!([{"name":"config","configMap":{"name":"config"}}]);
    let (code, response) = create(&s, "secure", value.clone()).await;
    assert_eq!(code, 201, "{response}");
    // Restricted allows these; the node cannot execute them, so the runtime
    // profile refuses them after policy has had its say.
    for (i, (pointer, patch)) in [
        ("/spec/securityContext", json!({"sysctls":[{"name":"net.ipv4.tcp_rmem","value":"4096 131072 6291456"}]})),
        ("/spec/containers/0/securityContext/seccompProfile", json!({"type":"Localhost","localhostProfile":"profiles/workload.json"})),
        ("/spec/containers/0/securityContext/capabilities/add", json!(["NET_BIND_SERVICE"])),
        ("/spec/containers/0/securityContext/seLinuxOptions", json!({"type":"container_engine_t"})),
        ("/spec/volumes", json!([{"name":"tmp","emptyDir":{}}])),
        ("/spec/initContainers", json!([{"name":"init","image":"example.invalid/init:v1","securityContext":{"runAsNonRoot":true,"runAsUser":1000,"seccompProfile":{"type":"RuntimeDefault"},"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}])),
    ]
    .into_iter()
    .enumerate()
    {
        let mut unsupported = value.clone();
        unsupported["metadata"]["name"] = json!(format!("unsupported-{i}"));
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        unsupported
            .pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(key.into(), patch);
        let (code, response) = create(&s, "secure", unsupported).await;
        assert_eq!(code, 422, "{pointer}: {response}");
        assert!(
            response["message"].as_str().unwrap().contains("runtime profile"),
            "{pointer}: {response}"
        );
    }
    value["metadata"]["name"] = json!("missing-seccomp");
    value["spec"]["containers"][0]["securityContext"]["seccompProfile"] = Value::Null;
    assert_eq!(create(&s, "secure", value.clone()).await.0, 403);
    value["metadata"]["name"] = json!("missing-nonroot");
    value["spec"]["containers"][0]["securityContext"]["seccompProfile"] =
        json!({"type":"RuntimeDefault"});
    value["spec"]["containers"][0]["securityContext"]["runAsNonRoot"] = Value::Null;
    assert_eq!(create(&s, "secure", value).await.0, 403);
}

#[tokio::test]
async fn namespace_policy_is_configurable_persistent_and_applies_to_admin() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("secure").await;
    // Inside the runtime profile (explicit UID), outside restricted Pod
    // Security (no runAsNonRoot): only the namespace policy objects.
    let mut insecure = pod("insecure");
    insecure["spec"]["securityContext"]["runAsNonRoot"] = Value::Null;
    assert_eq!(create(&s, "secure", insecure.clone()).await.0, 403);
    assert_eq!(create(&s, "default", insecure.clone()).await.0, 403);
    assert_eq!(create(&s, "kube-system", insecure.clone()).await.0, 201);
    let ns = "/api/v1/namespaces/secure";
    for (key, value) in [
        ("enforce", "unknown"),
        ("enforce-version", "v1.33"),
        ("warn", "restricted"),
    ] {
        let mut labels = json!({});
        labels[format!("pod-security.kubernetes.io/{key}")] = json!(value);
        let (code, response) = s
            .patch(
                s.admin(),
                ns,
                "application/merge-patch+json",
                json!({"metadata":{"labels":labels}}),
            )
            .await;
        assert_eq!(code, 422, "{response}");
    }
    let labels = json!({"pod-security.kubernetes.io/enforce":"baseline","pod-security.kubernetes.io/enforce-version":"v1.34"});
    assert_eq!(
        s.patch(
            s.admin(),
            ns,
            "application/merge-patch+json",
            json!({"metadata":{"labels":labels}})
        )
        .await
        .0,
        200
    );
    assert_eq!(create(&s, "secure", insecure.clone()).await.0, 201);
    insecure["metadata"]["name"] = json!("privileged");
    insecure["spec"]["containers"][0]["securityContext"] = json!({"privileged":true});
    assert_eq!(create(&s, "secure", insecure.clone()).await.0, 403);
    drop(s);
    let s = Server::start(dir.path()).await;
    assert_eq!(create(&s, "secure", insecure.clone()).await.0, 403);
    assert_eq!(
        s.patch(
            s.admin(),
            ns,
            "application/merge-patch+json",
            json!({"metadata":{"labels":{"pod-security.kubernetes.io/enforce":"privileged"}}})
        )
        .await
        .0,
        200
    );
    // Privileged Pod Security no longer objects, but the node cannot run a
    // privileged container: the runtime profile refuses it instead.
    let (code, refused) = create(&s, "secure", insecure).await;
    assert_eq!(code, 422, "{refused}");
    assert!(
        refused["message"]
            .as_str()
            .unwrap()
            .contains("runtime profile"),
        "{refused}"
    );
}

#[tokio::test]
async fn pod_update_and_patch_cannot_bypass_policy_and_status_remains_isolated() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("secure").await;
    let (_, original) = create(&s, "secure", pod("web")).await;
    let path = "/api/v1/namespaces/secure/pods/web";
    let annotation = json!({"container.apparmor.security.beta.kubernetes.io/web":"unconfined"});
    let mut update = original.clone();
    update["metadata"]["annotations"] = annotation.clone();
    assert_eq!(s.json(s.admin(), "PUT", path, update).await.0, 403);
    assert_eq!(
        s.patch(
            s.admin(),
            path,
            "application/merge-patch+json",
            json!({"metadata":{"annotations":annotation}})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        s.patch(
            s.admin(),
            path,
            "application/json-patch+json",
            json!([{"op":"add","path":"/metadata/annotations","value":annotation}])
        )
        .await
        .0,
        403
    );
    assert_eq!(s.json(s.admin(), "GET", path, json!({})).await.1, original);
    let (code, reported) = s
        .patch(
            s.admin(),
            &format!("{path}/status"),
            "application/merge-patch+json",
            json!({"metadata":{"annotations":annotation},"status":{"phase":"Running"}}),
        )
        .await;
    assert_eq!(code, 200, "{reported}");
    assert!(reported["metadata"]["annotations"].is_null());
}

#[tokio::test]
async fn terminating_namespace_rejects_new_content_but_allows_cleanup_and_status() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("closing").await;
    let (_, original) = create(&s, "closing", pod("web")).await;
    let (code, response) = s
        .patch(
            s.admin(),
            "/api/v1/namespaces/closing/status",
            "application/merge-patch+json",
            json!({"status":{"phase":"Terminating"}}),
        )
        .await;
    assert_eq!(code, 200, "{response}");
    assert_eq!(create(&s, "closing", pod("new")).await.0, 403);
    assert_eq!(
        s.json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/closing/configmaps",
            json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"new","namespace":""}})
        )
        .await
        .0,
        403
    );
    assert_eq!(create(&s, "missing", pod("new")).await.0, 404);
    let path = "/api/v1/namespaces/closing/pods/web";
    assert_eq!(
        s.patch(
            s.admin(),
            &format!("{path}/status"),
            "application/merge-patch+json",
            json!({"status":{"phase":"Succeeded"}})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        s.json(
            s.admin(),
            "DELETE",
            path,
            json!({"preconditions":{"uid":original["metadata"]["uid"]}})
        )
        .await
        .0,
        200
    );
    let mut empty = pod("empty");
    empty["metadata"]["namespace"] = json!("");
    assert_eq!(create(&s, "default", empty).await.0, 201);
}
