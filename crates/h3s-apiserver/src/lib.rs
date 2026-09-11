//! Authenticated Kubernetes API foundation. All registry access belongs here;
//! future controllers and nodes must use this API rather than write its store.
//!
//! Request flow: `handle` → `authn` → non-resource routes (`http`) **or**
//! `rest::execute`, which serves reads from `read` and runs every mutation
//! through `write` (decode → concurrency → prepare → admit → persist).
mod admission;
mod authn;
mod authz;
mod bootstrap;
mod http;
mod kubelet;
mod node_cidrs;
mod nodes;
mod openapi;
mod patch;
mod pod_io;
mod read;
mod resources;
mod rest;
mod seed;
mod selectors;
mod serviceaccounts;
mod services;
mod strategy;
mod supervisor;
mod transport;
mod wire;
mod write;
use axum::{
    body::Body,
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json, Router,
};
use h3s_storage::{Storage, StoreKey, StoredObject};
use resources::Target;
use serde_json::{json, Value};
use std::sync::Arc;
pub use transport::serve;
use transport::Peer;

pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
#[derive(Clone)]
pub struct Api {
    store: Arc<dyn Storage>,
    bootstrap: Option<Arc<bootstrap::Bootstrap>>,
    node_cidrs: Option<h3s_api::network::NodeCidrAllocations>,
    supervisor: Arc<h3s_supervisor::Hub>,
    admission_writes: Arc<tokio::sync::Mutex<()>>,
}
#[derive(Debug)]
struct Failure {
    code: u16,
    reason: &'static str,
    message: String,
}
type Result<T> = std::result::Result<T, Failure>;
impl Failure {
    fn new(code: u16, reason: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            reason,
            message: message.into(),
        }
    }
    fn value(&self) -> Value {
        json!({"apiVersion":"v1","kind":"Status","status":"Failure","reason":self.reason,"message":self.message,"code":self.code})
    }
}
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (StatusCode::from_u16(self.code).unwrap(), Json(self.value())).into_response()
    }
}
impl From<h3s_storage::Error> for Failure {
    fn from(e: h3s_storage::Error) -> Self {
        use h3s_storage::Error::*;
        let (code, reason) = match &e {
            AlreadyExists(_) => (409, "AlreadyExists"),
            NotFound(_) => (404, "NotFound"),
            Conflict { .. } => (409, "Conflict"),
            Compacted { .. } => (410, "Expired"),
            Invalid(_) | FutureRevision { .. } => (400, "BadRequest"),
            _ => (500, "InternalError"),
        };
        Self::new(
            code,
            reason,
            if code == 500 {
                "registry operation failed".into()
            } else {
                e.to_string()
            },
        )
    }
}
impl From<serde_json::Error> for Failure {
    fn from(_: serde_json::Error) -> Self {
        Self::new(422, "Invalid", "invalid Kubernetes JSON object")
    }
}
fn bad(message: &str) -> Failure {
    Failure::new(400, "BadRequest", message)
}
fn object(stored: StoredObject) -> Result<Value> {
    let mut value: Value = serde_json::from_slice(&stored.value)
        .map_err(|_| Failure::new(500, "InternalError", "invalid stored object"))?;
    if !value["metadata"].is_object() {
        return Err(Failure::new(
            500,
            "InternalError",
            "invalid stored metadata",
        ));
    }
    // Older Protobuf writes retained namespace="" on cluster-scoped objects.
    // kube-rs ObjectRef distinguishes Some("") from None; canonicalize reads as
    // well as new writes so existing registry entries work with controllers.
    if value["metadata"]["namespace"].as_str() == Some("") {
        value["metadata"]
            .as_object_mut()
            .expect("checked metadata")
            .remove("namespace");
    }
    value["metadata"]["resourceVersion"] = stored.revision.to_string().into();
    Ok(value)
}
fn key(value: String) -> Result<StoreKey> {
    Ok(StoreKey::new(value)?)
}
/// Registry key of the object a named request addresses.
fn named(target: &Target) -> Result<StoreKey> {
    key(format!(
        "{}{}",
        target.prefix(),
        target.name.as_deref().expect("named target")
    ))
}
fn stored(key: StoreKey, value: &Value) -> Result<StoredObject> {
    Ok(StoredObject {
        key,
        value: serde_json::to_vec(value)?,
        revision: 0,
    })
}
/// RFC 3339 at second precision, the shape Kubernetes stores and clients echo.
fn now() -> String {
    time::OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("zero nanoseconds")
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

impl Api {
    pub async fn new(store: Arc<dyn Storage>) -> std::result::Result<Self, h3s_storage::Error> {
        let api = Self {
            store,
            bootstrap: None,
            node_cidrs: None,
            supervisor: Arc::new(h3s_supervisor::Hub::default()),
            admission_writes: Arc::new(tokio::sync::Mutex::new(())),
        };
        seed::cluster(&api).await?;
        Ok(api)
    }
    async fn bootstrap(
        &self,
        path: String,
        mut value: Value,
    ) -> std::result::Result<(), h3s_storage::Error> {
        value["metadata"]["uid"] = uuid::Uuid::new_v4().to_string().into();
        value["metadata"]["creationTimestamp"] = now().into();
        let obj = StoredObject {
            key: StoreKey::new(path)?,
            value: serde_json::to_vec(&value).expect("known bootstrap JSON"),
            revision: 0,
        };
        match self.store.create(obj).await {
            Ok(_) | Err(h3s_storage::Error::AlreadyExists(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }
    /// Enable durable IPv4 node allocation before serving requests. Existing
    /// reservations and Node allocations are checked before the listener starts.
    pub async fn with_node_cidrs(
        mut self,
        cluster_cidr: &str,
        node_prefix: u8,
    ) -> std::result::Result<Self, String> {
        let configured = h3s_api::network::NodeCidrAllocations::new(cluster_cidr, node_prefix)?;
        let _write = self.admission_writes.lock().await;
        node_cidrs::initialize(&self.store, &configured)
            .await
            .map_err(|e| e.message)?;
        seed::node_cidrs(&self).await.map_err(|e| e.to_string())?;
        self.node_cidrs = Some(configured);
        drop(_write);
        Ok(self)
    }
    pub fn with_bootstrap(
        mut self,
        pki: Arc<h3s_certs::ClusterPki>,
        token: &str,
    ) -> std::result::Result<Self, &'static str> {
        if !h3s_auth::bootstrap::valid_token(token) {
            return Err("join token must be 32-256 printable ASCII bytes");
        }
        self.bootstrap = Some(Arc::new(
            bootstrap::Bootstrap::new(pki, token)
                .map_err(|_| "kubelet client credential setup failed")?,
        ));
        Ok(self)
    }
    /// Internal handle for authorized kubelet requests and transport verification.
    pub fn supervisor(&self) -> Arc<h3s_supervisor::Hub> {
        self.supervisor.clone()
    }
    pub fn router(self) -> Router {
        Router::new().fallback(handle).with_state(Arc::new(self))
    }
}

async fn handle(
    State(api): State<Arc<Api>>,
    Extension(peer): Extension<Peer>,
    request: Request<Body>,
) -> Response {
    match dispatch(&api, peer, request).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}
/// Health and join are open; everything else authenticates first and is then
/// either a non-resource URL or a resource request.
async fn dispatch(api: &Api, peer: Peer, request: Request<Body>) -> Result<Response> {
    let path = http::path(request.uri())?;
    if let Some(response) = http::health(request.method().as_str(), &path) {
        return Ok(response);
    }
    authn::forbid_impersonation(&request)?;
    if path == "/v1-h3s/join" {
        return bootstrap::join(api, request).await;
    }
    let user = authn::authenticate(peer, &request)?;
    if path == "/v1-h3s/serving" {
        return bootstrap::serving(api, &user, request).await;
    }
    if path == "/v1-h3s/connect" {
        return supervisor::connect(api, &user, request).await;
    }
    if let Some((node, route)) = kubelet::route(&path) {
        return kubelet::proxy(api, &user, node, route, request).await;
    }
    let query = http::Query::parse(request.uri())?;
    // Borrow only what the non-resource routes need: the request body is not
    // Sync, so holding `&request` across an await would make the handler !Send.
    let method = request.method().as_str();
    let accept = request.headers().get(axum::http::header::ACCEPT);
    if let Some(response) = http::non_resource(api, &user, method, &path, &query, accept).await? {
        return Ok(response);
    }
    rest::execute(api, &user, &path, query, request).await
}
