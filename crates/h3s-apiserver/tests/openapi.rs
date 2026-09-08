mod common;
use common::Server;
use serde_json::json;

#[tokio::test]
async fn openapi_v2_describes_core_objects_helm_validates() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    let (code, doc) = s.json(s.admin(), "GET", "/openapi/v2", json!({})).await;
    assert_eq!(code, 200, "{doc}");
    assert_eq!(doc["swagger"], "2.0");
    for kind in ["ConfigMap", "Secret", "Namespace"] {
        let key = format!("io.k8s.api.core.v1.{kind}");
        assert_eq!(
            doc["definitions"][&key]["x-kubernetes-group-version-kind"][0]["kind"], kind,
            "{key}: {doc}"
        );
    }
    let (code, _) = s
        .json(s.admin(), "GET", "/openapi/v2?timeout=32s", json!({}))
        .await;
    assert_eq!(code, 200);
}
