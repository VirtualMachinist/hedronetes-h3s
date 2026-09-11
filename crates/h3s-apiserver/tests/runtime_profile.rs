//! The runtime profile is the API's admission boundary: what the node cannot
//! execute is refused at create and never persisted, in every namespace.
mod common;
use common::Server;
use serde_json::{json, Value};

fn legal(name: &str) -> Value {
    json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":name},"spec":{
        "securityContext":{"runAsNonRoot":true,"runAsUser":65534,"seccompProfile":{"type":"RuntimeDefault"}},
        "containers":[{"name":"web","image":"example.invalid/web:v1","securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]
    }})
}
fn workload(kind: &str, name: &str, spec: Value) -> Value {
    json!({"apiVersion":"apps/v1","kind":kind,"metadata":{"name":name},"spec":{"selector":{"matchLabels":{"app":name}},"template":{"metadata":{"labels":{"app":name}},"spec":spec}}})
}
fn set(value: &mut Value, pointer: &str, v: Value) {
    let (parent, key) = pointer.rsplit_once('/').unwrap();
    value
        .pointer_mut(parent)
        .unwrap()
        .as_object_mut()
        .unwrap()
        .insert(key.into(), v);
}
fn refusals() -> Vec<(&'static str, Value, &'static str)> {
    vec![
        (
            "/spec/automountServiceAccountToken",
            json!(true),
            "token projection",
        ),
        (
            "/spec/enableServiceLinks",
            json!(true),
            "service environment",
        ),
        (
            "/spec/initContainers",
            json!([{"name":"init","image":"example.invalid/init:v1","securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]),
            "unsupported execution field",
        ),
        (
            "/spec/imagePullSecrets",
            json!([{"name":"registry"}]),
            "image authentication",
        ),
        (
            "/spec/securityContext/runAsUser",
            Value::Null,
            "explicit non-root UID",
        ),
        (
            "/spec/terminationGracePeriodSeconds",
            json!(31),
            "termination grace",
        ),
        (
            "/spec/volumes",
            json!([{"name":"scratch","emptyDir":{}}]),
            "volume source",
        ),
        (
            "/spec/containers/0/livenessProbe",
            json!({"httpGet":{"path":"/","port":80}}),
            "unsupported execution field",
        ),
        (
            "/spec/containers/0/securityContext/seccompProfile",
            json!({"type":"Localhost","localhostProfile":"p.json"}),
            "seccomp",
        ),
    ]
}

#[tokio::test]
async fn pods_default_into_the_runtime_profile() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let (code, created) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/team-a/pods",
            legal("web"),
        )
        .await;
    assert_eq!(code, 201, "{created}");
    let spec = &created["spec"];
    assert_eq!(spec["automountServiceAccountToken"], false);
    assert_eq!(spec["enableServiceLinks"], false);
    assert_eq!(spec["dnsPolicy"], "ClusterFirst");
    assert_eq!(spec["terminationGracePeriodSeconds"], 30);
    assert!(spec.get("volumes").is_none(), "{created}");
    assert!(
        spec["containers"][0].get("volumeMounts").is_none(),
        "{created}"
    );
}

#[tokio::test]
async fn pods_outside_the_profile_are_refused_regardless_of_pod_security_level() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    // kube-system runs privileged Pod Security; the profile still applies.
    for ns in ["team-a", "kube-system"] {
        for (i, (pointer, value, needle)) in refusals().into_iter().enumerate() {
            let name = format!("refused-{i}");
            let mut pod = legal(&name);
            set(&mut pod, pointer, value);
            let (code, failure) = s
                .json(
                    s.admin(),
                    "POST",
                    &format!("/api/v1/namespaces/{ns}/pods"),
                    pod,
                )
                .await;
            assert_eq!(code, 422, "{ns} {pointer}: {failure}");
            let message = failure["message"].as_str().unwrap();
            assert!(
                message.contains("restricted-v1") && message.contains(needle),
                "{ns} {pointer}: {message}"
            );
            assert_eq!(
                s.json(
                    s.admin(),
                    "GET",
                    &format!("/api/v1/namespaces/{ns}/pods/{name}"),
                    json!({})
                )
                .await
                .0,
                404
            );
        }
    }
}

#[tokio::test]
async fn workload_templates_outside_the_profile_are_refused_at_create_and_update() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    for (kind, plural) in [("Deployment", "deployments"), ("ReplicaSet", "replicasets")] {
        let base = format!("/apis/apps/v1/namespaces/team-a/{plural}");
        for (i, (pointer, value, needle)) in refusals().into_iter().enumerate() {
            let mut template = legal("template");
            set(&mut template, pointer, value);
            let name = format!("bad-{i}");
            let (code, failure) = s
                .json(
                    s.admin(),
                    "POST",
                    &base,
                    workload(kind, &name, template["spec"].clone()),
                )
                .await;
            assert_eq!(code, 422, "{kind} {pointer}: {failure}");
            let message = failure["message"].as_str().unwrap();
            assert!(
                message.contains("template")
                    && message.contains("restricted-v1")
                    && message.contains(needle),
                "{kind} {pointer}: {message}"
            );
        }
        let (_, list) = s.json(s.admin(), "GET", &base, json!({})).await;
        assert_eq!(list["items"], json!([]), "{kind}");
        let (code, created) = s
            .json(
                s.admin(),
                "POST",
                &base,
                workload(kind, "good", legal("t")["spec"].clone()),
            )
            .await;
        assert_eq!(code, 201, "{created}");
        let template = &created["spec"]["template"]["spec"];
        assert_eq!(template["automountServiceAccountToken"], false);
        assert_eq!(template["enableServiceLinks"], false);
        let mut update = created.clone();
        set(
            &mut update,
            "/spec/template/spec/initContainers",
            json!([{"name":"init","image":"example.invalid/init:v1","securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]),
        );
        let (code, failure) = s
            .json(s.admin(), "PUT", &format!("{base}/good"), update)
            .await;
        assert_eq!(code, 422, "{kind}: {failure}");
        assert_eq!(
            s.json(s.admin(), "GET", &format!("{base}/good"), json!({}))
                .await
                .1,
            created
        );
    }
}
