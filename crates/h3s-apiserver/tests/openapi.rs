mod common;
use common::Server;
use http_body_util::BodyExt;
use prost::Message;
use serde_json::json;

#[derive(Clone, PartialEq, Message)]
struct OpenApiDocument {
    #[prost(string, tag = "1")]
    swagger: String,
}

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

#[tokio::test]
async fn openapi_v2_serves_the_protobuf_stock_helm_requests() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    let response = s
        .raw(
            s.admin(),
            "GET",
            "/openapi/v2",
            json!({}),
            &[(
                "Accept",
                "application/com.github.proto-openapi.spec.v2@v1.0+protobuf",
            )],
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "application/com.github.proto-openapi.spec.v2.v1.0+protobuf"
    );
    assert_eq!(
        response.headers()["vary"]
            .to_str()
            .unwrap()
            .to_ascii_lowercase(),
        "accept"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let document = OpenApiDocument::decode(body).unwrap();
    assert_eq!(document.swagger, "2.0");
}
