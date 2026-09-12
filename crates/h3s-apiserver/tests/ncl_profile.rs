//! NICKEL G4 wire: the runtime profile that admission enforces is the
//! `k8s-1.34-h3s-0.9.1` release overlay generated from `hedron-ncl`, and the
//! status contract around it is unchanged. `examples/supported-pod.yaml`
//! admits; a 0.9 release gap (token projection, service links, PVC) is a
//! 422 runtime refusal; a Pod Security miss is 403 and is decided first.
//! Nothing here evaluates Nickel: this crate consumes generated Rust only.
mod common;
use common::Server;
use h3s_api::pod_profile::PodRuntimeProfile;
use serde_json::{json, Value};

const SUPPORTED_POD: &str = include_str!("../../../examples/supported-pod.yaml");

fn supported_pod(name: &str) -> Value {
    let mut pod: Value =
        serde_yaml::from_str(SUPPORTED_POD).expect("examples/supported-pod.yaml is YAML");
    pod["metadata"]["name"] = json!(name);
    pod
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

async fn create(s: &Server, ns: &str, pod: Value) -> (u16, Value) {
    s.json(
        s.admin(),
        "POST",
        &format!("/api/v1/namespaces/{ns}/pods"),
        pod,
    )
    .await
}

async fn absent(s: &Server, ns: &str, name: &str) {
    let (code, _) = s
        .json(
            s.admin(),
            "GET",
            &format!("/api/v1/namespaces/{ns}/pods/{name}"),
            json!({}),
        )
        .await;
    assert_eq!(code, 404, "{ns}/{name} must not be persisted");
}

#[tokio::test]
async fn supported_pod_example_admits_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let (code, created) = create(&s, "team-a", supported_pod("supported-pod")).await;
    assert_eq!(code, 201, "{created}");
    let spec = &created["spec"];
    assert_eq!(spec["automountServiceAccountToken"], false);
    assert_eq!(spec["enableServiceLinks"], false);
    assert_eq!(spec["dnsPolicy"], "Default");
    let sc = &spec["containers"][0]["securityContext"];
    assert_eq!(sc["runAsNonRoot"], true);
    assert_eq!(sc["runAsUser"], 65534);
    assert_eq!(sc["allowPrivilegeEscalation"], false);
    assert_eq!(sc["capabilities"]["drop"], json!(["ALL"]));
    assert_eq!(sc["seccompProfile"]["type"], "RuntimeDefault");
}

/// The 0.9.1 overlay still says no token projection: until TokenRequest and
/// bearer authentication land (GOAL-V010 spine 3), `automountServiceAccountToken: true`
/// is a runtime refusal, in every namespace, and nothing is persisted.
#[tokio::test]
async fn automount_true_is_still_a_422_runtime_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    for ns in ["team-a", "kube-system"] {
        let mut pod = supported_pod("token");
        set(&mut pod, "/spec/automountServiceAccountToken", json!(true));
        let (code, failure) = create(&s, ns, pod).await;
        assert_eq!(code, 422, "{ns}: {failure}");
        assert_eq!(failure["reason"], "Invalid", "{failure}");
        let message = failure["message"].as_str().unwrap();
        assert!(
            message.contains("restricted-v1")
                && message.contains(PodRuntimeProfile::CONTRACT_SET)
                && message.contains("token projection"),
            "{ns}: {message}"
        );
        absent(&s, ns, "token").await;
    }
}

/// Release gaps that Pod Security `restricted` would allow are still refused
/// by the runtime profile: 422, never 403.
#[tokio::test]
async fn release_gaps_are_runtime_422_not_policy_403() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let gaps: Vec<(&str, &str, Value, &str)> = vec![
        (
            "links",
            "/spec/enableServiceLinks",
            json!(true),
            "service environment",
        ),
        (
            "pvc",
            "/spec/volumes",
            json!([{"name":"data","persistentVolumeClaim":{"claimName":"data"}}]),
            "volume source",
        ),
        (
            "init",
            "/spec/initContainers",
            json!([{"name":"init","image":"registry.k8s.io/pause:3.10","securityContext":{"runAsNonRoot":true,"runAsUser":65534,"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"seccompProfile":{"type":"RuntimeDefault"}}}]),
            "unsupported execution field",
        ),
    ];
    for (name, pointer, value, needle) in gaps {
        let mut pod = supported_pod(name);
        set(&mut pod, pointer, value);
        let (code, failure) = create(&s, "team-a", pod).await;
        assert_eq!(code, 422, "{pointer}: {failure}");
        let message = failure["message"].as_str().unwrap();
        assert!(
            message.contains("restricted-v1") && message.contains(needle),
            "{pointer}: {message}"
        );
        assert!(!message.contains("PodSecurity"), "{pointer}: {message}");
        absent(&s, "team-a", name).await;
    }
}

/// A Pod Security miss is 403 with the policy sentence, and it is decided
/// before the runtime profile: a Pod that misses both is 403, not 422.
#[tokio::test]
async fn pod_security_miss_is_403_and_precedes_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let mut root = supported_pod("root");
    set(
        &mut root,
        "/spec/containers/0/securityContext/runAsNonRoot",
        json!(false),
    );
    let (code, failure) = create(&s, "team-a", root).await;
    assert_eq!(code, 403, "{failure}");
    assert_eq!(failure["reason"], "Forbidden", "{failure}");
    let message = failure["message"].as_str().unwrap();
    assert!(
        message.contains("PodSecurity restricted:v1.34"),
        "{message}"
    );
    assert!(message.contains("must not request root"), "{message}");
    absent(&s, "team-a", "root").await;

    let mut both = supported_pod("both");
    set(
        &mut both,
        "/spec/containers/0/securityContext/allowPrivilegeEscalation",
        json!(true),
    );
    set(&mut both, "/spec/automountServiceAccountToken", json!(true));
    let (code, failure) = create(&s, "team-a", both).await;
    assert_eq!(code, 403, "policy is decided first: {failure}");
    assert!(
        failure["message"].as_str().unwrap().contains("PodSecurity"),
        "{failure}"
    );
    absent(&s, "team-a", "both").await;

    // kube-system runs privileged Pod Security: the same Pod passes policy and
    // meets the runtime profile instead.
    let mut both = supported_pod("both");
    set(
        &mut both,
        "/spec/containers/0/securityContext/allowPrivilegeEscalation",
        json!(true),
    );
    set(&mut both, "/spec/automountServiceAccountToken", json!(true));
    let (code, failure) = create(&s, "kube-system", both).await;
    assert_eq!(code, 422, "{failure}");
    assert!(
        failure["message"]
            .as_str()
            .unwrap()
            .contains("restricted-v1"),
        "{failure}"
    );
}
