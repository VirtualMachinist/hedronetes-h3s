mod common;
use common::Server;
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn system_namespaces_cannot_be_deleted() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    for name in ["default", "kube-system", "kube-public", "kube-node-lease"] {
        let (code, body) = s
            .json(
                s.admin(),
                "DELETE",
                &format!("/api/v1/namespaces/{name}"),
                json!({}),
            )
            .await;
        assert_eq!(code, 403, "{name}: {body}");
    }
}

#[tokio::test]
async fn namespace_delete_marks_terminating_until_controller_collects() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-del").await;
    let (code, cm) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/team-del/configmaps",
            json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"keep"},"data":{"k":"v"}}),
        )
        .await;
    assert_eq!(code, 201, "{cm}");
    let (code, deleted) = s
        .json(
            s.admin(),
            "DELETE",
            "/api/v1/namespaces/team-del",
            json!({}),
        )
        .await;
    assert_eq!(code, 200, "{deleted}");
    assert_eq!(deleted["status"]["phase"], "Terminating");
    assert!(
        deleted["metadata"]["deletionTimestamp"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "{deleted}"
    );
    assert_eq!(deleted["metadata"]["finalizers"], json!(["kubernetes"]));
    let (code, still) = s
        .json(s.admin(), "GET", "/api/v1/namespaces/team-del", json!({}))
        .await;
    assert_eq!(code, 200, "{still}");
    let (code, cm) = s
        .json(
            s.admin(),
            "GET",
            "/api/v1/namespaces/team-del/configmaps/keep",
            json!({}),
        )
        .await;
    assert_eq!(code, 200, "{cm}");
}

#[tokio::test]
async fn namespace_controller_deletes_children_and_removes_namespace() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start_with_controllers(dir.path()).await;
    s.namespace("team-gc").await;
    let (code, cm) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/team-gc/configmaps",
            json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"gone"},"data":{"k":"v"}}),
        )
        .await;
    assert_eq!(code, 201, "{cm}");
    let (code, deleted) = s
        .json(s.admin(), "DELETE", "/api/v1/namespaces/team-gc", json!({}))
        .await;
    assert_eq!(code, 200, "{deleted}");
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let (code, _) = s
                .json(s.admin(), "GET", "/api/v1/namespaces/team-gc", json!({}))
                .await;
            if code == 404 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("namespace controller did not finish deletion");
    let (code, cm) = s
        .json(
            s.admin(),
            "GET",
            "/api/v1/namespaces/team-gc/configmaps/gone",
            json!({}),
        )
        .await;
    assert_eq!(code, 404, "{cm}");
}
