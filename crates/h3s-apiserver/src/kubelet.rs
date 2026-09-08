//! Authorization and verified application TLS precede private kubelet requests.
use crate::{key, Api, Failure, Result};
use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
};
use h3s_auth::{Request as AuthRequest, ResourceRequest, User};
use http_body_util::Empty;
use hyper_util::rt::TokioIo;
use std::time::Duration;

pub fn route(path: &str) -> Option<(&str, &str)> {
    let (node, route) = path.strip_prefix("/api/v1/nodes/")?.split_once("/proxy/")?;
    (!node.is_empty() && !node.contains('/')).then_some((node, route))
}
struct Driver(tokio::task::JoinHandle<()>);
impl Drop for Driver {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn gateway() -> Failure {
    Failure::new(502, "BadGateway", "verified kubelet request failed")
}

pub async fn proxy(
    api: &Api,
    user: &User,
    node: &str,
    route: &str,
    request: Request<Body>,
) -> Result<Response> {
    let auth = AuthRequest::Resource(ResourceRequest {
        verb: "get",
        group: "",
        resource: "nodes",
        subresource: Some("proxy"),
        namespace: None,
        name: Some(node),
    });
    if !api.rbac().await?.allows(user, &auth) {
        return Err(Failure::new(403, "Forbidden", "node proxy access denied"));
    }
    if request.method() != "GET" {
        return Err(Failure::new(
            405,
            "MethodNotAllowed",
            "kubelet health proxy supports GET",
        ));
    }
    let query: Vec<(String, String)> =
        serde_urlencoded::from_str(request.uri().query().unwrap_or(""))
            .map_err(|_| Failure::new(400, "BadRequest", "invalid proxy query"))?;
    // Stock kubectl adds its request timeout even to --raw requests. Accept
    // that client hint, but keep our own 15-second cap and never forward it.
    if query.len() > 1 || query.iter().any(|(key, _)| key != "timeout") {
        return Err(Failure::new(
            400,
            "BadRequest",
            "kubelet health proxy accepts only the client timeout hint",
        ));
    }
    if !matches!(route, "healthz" | "readyz") {
        return Err(Failure::new(
            404,
            "NotFound",
            "kubelet endpoint is not implemented",
        ));
    }
    forward(api, node, "GET", route, &[], 4096).await
}

/// Node-authenticated kubelet call using the API's kubelet client, not the
/// caller's identity. Used for pods/log and pods/exec after those RBAC checks.
pub async fn forward(
    api: &Api,
    node: &str,
    method: &str,
    route: &str,
    query: &[(String, String)],
    max_bytes: usize,
) -> Result<Response> {
    if !h3s_api::valid_node_name(node)
        || api
            .store
            .get(&key(format!("/registry/nodes/{node}"))?)
            .await?
            .is_none()
    {
        return Err(Failure::new(404, "NotFound", "node does not exist"));
    }
    let state = api.bootstrap.as_ref().ok_or_else(gateway)?;
    let path = if query.is_empty() {
        format!("/{route}")
    } else {
        format!(
            "/{route}?{}",
            serde_urlencoded::to_string(query).map_err(|_| bad_query())?
        )
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        let stream = api
            .supervisor
            .open_kubelet(node)
            .await
            .map_err(|_| gateway())?;
        let tls = tokio_rustls::TlsConnector::from(state.kubelet_client.clone())
            .connect(h3s_certs::kubelet_server_name(node), stream)
            .await
            .map_err(|_| gateway())?;
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .map_err(|_| gateway())?;
        let _driver = Driver(tokio::spawn(async move {
            let _ = connection.await;
        }));
        // Never forward caller Authorization, identity, cookies, Host or arbitrary paths.
        let outgoing = Request::builder()
            .method(method)
            .uri(&path)
            .header("host", "kubelet")
            .header("connection", "close")
            .body(Empty::<axum::body::Bytes>::new())
            .expect("fixed request");
        let response = sender.send_request(outgoing).await.map_err(|_| gateway())?;
        let status = response.status();
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("text/plain; charset=utf-8")
            .to_owned();
        let bytes = to_bytes(Body::new(response.into_body()), max_bytes)
            .await
            .map_err(|_| gateway())?;
        if status == StatusCode::OK {
            return Ok((
                status,
                [
                    ("content-type", content_type.as_str()),
                    ("cache-control", "no-store"),
                ],
                bytes,
            )
                .into_response());
        }
        if matches!(route, "healthz" | "readyz") && status == StatusCode::SERVICE_UNAVAILABLE {
            return Ok((
                status,
                [
                    ("content-type", "text/plain; charset=utf-8"),
                    ("cache-control", "no-store"),
                ],
                bytes,
            )
                .into_response());
        }
        let message = String::from_utf8_lossy(&bytes).trim().to_string();
        let (code, reason) = match status.as_u16() {
            400 => (400, "BadRequest"),
            404 => (404, "NotFound"),
            405 => (405, "MethodNotAllowed"),
            409 => (409, "Conflict"),
            501 => (501, "NotImplemented"),
            503 => (503, "ServiceUnavailable"),
            _ => return Err(gateway()),
        };
        Err(Failure::new(
            code,
            reason,
            if message.is_empty() {
                "kubelet request failed".into()
            } else {
                message
            },
        ))
    })
    .await
    .map_err(|_| Failure::new(504, "Timeout", "kubelet request timed out"))?
}
fn bad_query() -> Failure {
    Failure::new(400, "BadRequest", "invalid kubelet query")
}
