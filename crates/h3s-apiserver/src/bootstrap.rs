//! Native agent enrollment. This API is the only owner of enrollment registry
//! records; the supervisor and kubelet never receive registry access.
use crate::{key, stored, Api, Failure, Result};
use axum::{
    body::{to_bytes, Body},
    http::Request,
    response::{IntoResponse, Response},
    Json,
};
use h3s_api::{JoinRequest, JoinResponse};
use h3s_auth::bootstrap::{digest, matches, valid_password};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

pub struct Bootstrap {
    pki: Arc<h3s_certs::ClusterPki>,
    token_hash: [u8; 32],
    slots: tokio::sync::Semaphore,
    pub kubelet_client: Arc<rustls::ClientConfig>,
}
impl Bootstrap {
    pub fn new(pki: Arc<h3s_certs::ClusterPki>, token: &str) -> h3s_certs::Result<Self> {
        let identity = pki.issue_client(h3s_api::KUBELET_CLIENT_ID, None)?;
        let kubelet_client = Arc::new(pki.client_config(Some(&identity))?);
        Ok(Self {
            pki,
            token_hash: digest(token),
            slots: tokio::sync::Semaphore::new(4),
            kubelet_client,
        })
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    version: u32,
    password_hash: [u8; 32],
}
fn invalid() -> Failure {
    Failure::new(401, "Unauthorized", "invalid agent enrollment credentials")
}

pub async fn join(api: &Api, request: Request<Body>) -> Result<Response> {
    let state = api
        .bootstrap
        .as_ref()
        .ok_or_else(|| Failure::new(404, "NotFound", "agent enrollment is not configured"))?;
    if request.method() != "POST" {
        return Err(Failure::new(
            405,
            "MethodNotAllowed",
            "agent enrollment requires POST",
        ));
    }
    if request.uri().query().is_some() {
        return Err(Failure::new(
            400,
            "BadRequest",
            "enrollment does not accept query parameters",
        ));
    }
    let mut headers = request.headers().get_all("authorization").iter();
    let token = headers
        .next()
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or_else(invalid)?;
    if headers.next().is_some() || !matches(&state.token_hash, token) {
        return Err(invalid());
    }
    if request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        != Some("application/json")
    {
        return Err(Failure::new(
            415,
            "UnsupportedMediaType",
            "enrollment requires application/json",
        ));
    }
    let _permit = state
        .slots
        .try_acquire()
        .map_err(|_| Failure::new(429, "TooManyRequests", "enrollment capacity reached"))?;
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(request.into_body(), 16 * 1024),
    )
    .await
    .map_err(|_| Failure::new(408, "Timeout", "enrollment body timed out"))?
    .map_err(|_| {
        Failure::new(
            413,
            "RequestEntityTooLarge",
            "enrollment body exceeds limit",
        )
    })?;
    let joined: JoinRequest = serde_json::from_slice(&bytes)
        .map_err(|_| Failure::new(400, "BadRequest", "invalid enrollment request"))?;
    if !h3s_api::valid_node_name(&joined.node_name)
        || !valid_password(&joined.password)
        || joined.csr_pem.len() > 8192
    {
        return Err(Failure::new(
            400,
            "BadRequest",
            "invalid node name, password format or CSR size",
        ));
    }
    let _guard = api.admission_writes.lock().await;
    let registry_key = key(format!(
        "/registry/h3s-node-identities/{}",
        joined.node_name
    ))?;
    let previous = api.store.get(&registry_key).await?;
    if let Some(previous) = &previous {
        let record: Registration = serde_json::from_slice(&previous.value)
            .map_err(|_| Failure::new(500, "InternalError", "invalid enrollment record"))?;
        if record.version != 1 {
            return Err(Failure::new(
                500,
                "InternalError",
                "unsupported enrollment record",
            ));
        }
        if !matches(&record.password_hash, &joined.password) {
            return Err(invalid());
        }
    } else if api
        .store
        .get(&key(format!("/registry/nodes/{}", joined.node_name))?)
        .await?
        .is_some()
    {
        return Err(Failure::new(
            409,
            "Conflict",
            "node exists without an enrollment record; administrator recovery is required",
        ));
    }
    let pki = state.pki.clone();
    let node = joined.node_name;
    let csr = joined.csr_pem;
    let certificate_pem = tokio::task::spawn_blocking(move || pki.sign_node_csr(&node, &csr))
        .await
        .map_err(|_| Failure::new(500, "InternalError", "enrollment signer unavailable"))?
        .map_err(|_| Failure::new(400, "BadRequest", "invalid node CSR"))?;
    if previous.is_none() {
        let record = serde_json::to_value(Registration {
            version: 1,
            password_hash: digest(&joined.password),
        })?;
        api.store.create(stored(registry_key, &record)?).await?;
    }
    Ok((
        if previous.is_none() {
            axum::http::StatusCode::CREATED
        } else {
            axum::http::StatusCode::OK
        },
        [("cache-control", "no-store")],
        Json(JoinResponse { certificate_pem }),
    )
        .into_response())
}

/// Only an existing native node may request its own dedicated serving identity.
pub async fn serving(api: &Api, user: &h3s_auth::User, request: Request<Body>) -> Result<Response> {
    let node = user
        .node_name()
        .filter(|n| h3s_api::valid_node_name(n))
        .ok_or_else(|| {
            Failure::new(
                403,
                "Forbidden",
                "kubelet certificate requires a native node identity",
            )
        })?;
    let state = api
        .bootstrap
        .as_ref()
        .ok_or_else(|| Failure::new(404, "NotFound", "native bootstrap is not configured"))?;
    if request.method() != "POST" || request.uri().query().is_some() {
        return Err(Failure::new(
            400,
            "BadRequest",
            "kubelet certificate requires POST without query",
        ));
    }
    if request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        != Some("application/json")
    {
        return Err(Failure::new(415, "UnsupportedMediaType", "JSON required"));
    }
    let _slot = state.slots.try_acquire().map_err(|_| {
        Failure::new(
            429,
            "TooManyRequests",
            "certificate signer capacity reached",
        )
    })?;
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(request.into_body(), 16 * 1024),
    )
    .await
    .map_err(|_| Failure::new(408, "Timeout", "certificate body timed out"))?
    .map_err(|_| {
        Failure::new(
            413,
            "RequestEntityTooLarge",
            "certificate body exceeds limit",
        )
    })?;
    let csr: h3s_api::ServingRequest = serde_json::from_slice(&bytes)
        .map_err(|_| Failure::new(400, "BadRequest", "invalid serving certificate request"))?;
    if csr.csr_pem.len() > 8192 {
        return Err(Failure::new(400, "BadRequest", "CSR exceeds limit"));
    }
    let _guard = api.admission_writes.lock().await;
    for path in [
        format!("/registry/nodes/{node}"),
        format!("/registry/h3s-node-identities/{node}"),
    ] {
        if api.store.get(&key(path)?).await?.is_none() {
            return Err(Failure::new(
                403,
                "Forbidden",
                "registered native node required",
            ));
        }
    }
    let pki = state.pki.clone();
    let node = node.to_owned();
    let certificate_pem =
        tokio::task::spawn_blocking(move || pki.sign_kubelet_csr(&node, &csr.csr_pem))
            .await
            .map_err(|_| Failure::new(500, "InternalError", "signer unavailable"))?
            .map_err(|_| Failure::new(400, "BadRequest", "invalid kubelet CSR"))?;
    Ok((
        [("cache-control", "no-store")],
        Json(h3s_api::ServingResponse { certificate_pem }),
    )
        .into_response())
}
