//! Authenticated Kubernetes API foundation. All registry access belongs here;
//! future controllers and nodes must use this API rather than write its store.
mod admission;
mod patch;
mod resources;
mod selectors;
mod serviceaccounts;
mod services;
mod strategy;
mod transport;
mod wire;
use axum::{
    body::{to_bytes, Body, Bytes},
    extract::State,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json, Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use futures_util::StreamExt;
use h3s_auth::{Rbac, Request as AuthRequest, ResourceRequest};
use h3s_storage::{EventKind, ListSelect, Storage, StoreKey, StoredObject, WatchSelect};
use resources::{Target, RESOURCES};
use selectors::Selection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, convert::Infallible, sync::Arc, time::Duration};
pub use transport::serve;
use transport::Peer;

pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
#[derive(Clone)]
pub struct Api {
    store: Arc<dyn Storage>,
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
fn stored(key: StoreKey, value: &Value) -> Result<StoredObject> {
    Ok(StoredObject {
        key,
        value: serde_json::to_vec(value)?,
        revision: 0,
    })
}
fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap()
}

impl Api {
    pub async fn new(store: Arc<dyn Storage>) -> std::result::Result<Self, h3s_storage::Error> {
        let api = Self {
            store,
            admission_writes: Arc::new(tokio::sync::Mutex::new(())),
        };
        for namespace in ["default", "kube-system", "kube-public", "kube-node-lease"] {
            api.bootstrap(format!("/registry/namespaces/{namespace}"),json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":namespace},"status":{"phase":"Active"}})).await?;
        }
        api.bootstrap("/registry/clusterroles/h3s-discovery".into(),json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole","metadata":{"name":"h3s-discovery"},"rules":[{"verbs":["get"],"nonResourceURLs":["/api","/api/*","/apis","/apis/*"]}]})).await?;
        api.bootstrap("/registry/clusterrolebindings/h3s-discovery".into(),json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRoleBinding","metadata":{"name":"h3s-discovery"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"ClusterRole","name":"h3s-discovery"},"subjects":[{"kind":"Group","apiGroup":"rbac.authorization.k8s.io","name":"system:authenticated"}]})).await?;
        api.bootstrap("/registry/clusterroles/h3s-namespace-controller".into(), json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole","metadata":{"name":"h3s-namespace-controller"},"rules":[
            {"apiGroups":[""],"resources":["namespaces"],"verbs":["get","list","watch"]},
            {"apiGroups":[""],"resources":["serviceaccounts"],"verbs":["get","list","watch"],"resourceNames":["default"]},
            {"apiGroups":[""],"resources":["configmaps"],"verbs":["get","list","watch","update"],"resourceNames":["kube-root-ca.crt"]},
            {"apiGroups":[""],"resources":["serviceaccounts","configmaps"],"verbs":["create"]}
        ]})).await?;
        api.bootstrap("/registry/clusterrolebindings/h3s-namespace-controller".into(), json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRoleBinding","metadata":{"name":"h3s-namespace-controller"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"ClusterRole","name":"h3s-namespace-controller"},"subjects":[{"kind":"User","apiGroup":"rbac.authorization.k8s.io","name":"system:h3s:namespace-controller"}]})).await?;
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
    pub fn router(self) -> Router {
        Router::new().fallback(handle).with_state(Arc::new(self))
    }
    async fn rbac(&self) -> Result<Rbac> {
        let mut r = Rbac::default();
        let mut snapshot = None;
        for kind in [
            "roles",
            "rolebindings",
            "clusterroles",
            "clusterrolebindings",
        ] {
            let mut sel = ListSelect::new(format!("/registry/{kind}/"));
            sel.at_revision = snapshot;
            loop {
                let page = self.store.list(sel.clone()).await?;
                snapshot.get_or_insert(page.revision);
                for obj in page.items {
                    match kind {
                        "roles" => r.roles.push(serde_json::from_slice(&obj.value)?),
                        "rolebindings" => r.role_bindings.push(serde_json::from_slice(&obj.value)?),
                        "clusterroles" => r.cluster_roles.push(serde_json::from_slice(&obj.value)?),
                        _ => r
                            .cluster_role_bindings
                            .push(serde_json::from_slice(&obj.value)?),
                    }
                }
                let Some(next) = page.next_after else {
                    break;
                };
                sel.at_revision = Some(page.revision);
                sel.start_after = Some(next);
            }
        }
        Ok(r)
    }
}

async fn handle(
    State(api): State<Arc<Api>>,
    Extension(peer): Extension<Peer>,
    request: Request<Body>,
) -> Response {
    match dispatch(api, peer, request).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}
async fn dispatch(api: Arc<Api>, peer: Peer, request: Request<Body>) -> Result<Response> {
    // Decode each segment once, including colon-bearing RBAC names. Encoded
    // separators may not turn a name into a different resource/subresource.
    let segments: Result<Vec<_>> = request
        .uri()
        .path()
        .split('/')
        .map(|segment| {
            let decoded = percent_encoding::percent_decode_str(segment)
                .decode_utf8()
                .map_err(|_| bad("path must be UTF-8"))?;
            if decoded.contains(['/', '\\', '\0']) || matches!(decoded.as_ref(), "." | "..") {
                return Err(bad("invalid path segment"));
            }
            Ok(decoded.into_owned())
        })
        .collect();
    let path = segments?.join("/");
    let method = request.method().as_str().to_owned();
    if method == "GET" && ["/livez", "/readyz", "/version"].contains(&path.as_str()) {
        return Ok(if path == "/version" {
            Json(json!({"major":"1","minor":"34","gitVersion":"v1.34.0+h3s","platform":format!("{}/{}",std::env::consts::OS,match std::env::consts::ARCH{"aarch64"=>"arm64","x86_64"=>"amd64",other=>other})})).into_response()
        } else {
            "ok\n".into_response()
        });
    }
    if request
        .headers()
        .keys()
        .any(|h| h.as_str().starts_with("impersonate-"))
    {
        return Err(Failure::new(
            403,
            "Forbidden",
            "impersonation is not enabled",
        ));
    }
    let user = peer.0.ok_or_else(|| {
        Failure::new(
            401,
            "Unauthorized",
            "a trusted client certificate is required",
        )
    })?;
    // Invalid bearer credentials must not silently fall back to another identity.
    if request.headers().contains_key("authorization") {
        return Err(Failure::new(
            401,
            "Unauthorized",
            "bearer authentication is not yet implemented",
        ));
    }
    let query: Vec<(String, String)> =
        serde_urlencoded::from_str(request.uri().query().unwrap_or(""))
            .map_err(|_| bad("invalid query"))?;
    let mut q = BTreeMap::new();
    for (k, v) in query {
        if q.insert(k, v).is_some() {
            return Err(bad("duplicate query parameter"));
        }
    }
    if let Some(discovery) = discovery(&path) {
        if method != "GET" {
            return Err(Failure::new(
                405,
                "MethodNotAllowed",
                "discovery is read-only",
            ));
        }
        if !api.rbac().await?.allows(
            &user,
            &AuthRequest::NonResource {
                verb: "get",
                path: &path,
            },
        ) {
            return Err(Failure::new(403, "Forbidden", "discovery access denied"));
        }
        return Ok(Json(discovery).into_response());
    }
    let target = Target::parse(&path).ok_or_else(|| {
        Failure::new(
            404,
            "NotFound",
            "resource or subresource is not implemented",
        )
    })?;
    let watch = q.get("watch").is_some_and(|v| v == "true" || v == "1");
    let verb = match (method.as_str(), target.name.is_some(), watch) {
        ("GET", _, true) => "watch",
        ("GET", true, _) => "get",
        ("GET", false, _) => "list",
        ("POST", true, _) if target.subresource == Some("binding") => "create",
        ("POST", false, _) => "create",
        ("PUT", true, _) => "update",
        ("PATCH", true, _) => "patch",
        ("DELETE", true, _) => "delete",
        _ => {
            return Err(Failure::new(
                405,
                "MethodNotAllowed",
                "verb is not implemented for this resource",
            ))
        }
    };
    if target.subresource == Some("status") && !matches!(verb, "get" | "update" | "patch") {
        return Err(Failure::new(
            405,
            "MethodNotAllowed",
            "status supports get, update and patch",
        ));
    }
    if target.subresource == Some("binding") && verb != "create" {
        return Err(Failure::new(
            405,
            "MethodNotAllowed",
            "binding supports create",
        ));
    }
    let selection = Selection::parse(
        q.get("labelSelector").map(String::as_str).unwrap_or(""),
        q.get("fieldSelector").map(String::as_str).unwrap_or(""),
        target.resource.kind,
    )?
    .with_name(target.name.as_deref());
    let attrs = AuthRequest::Resource(ResourceRequest {
        verb,
        group: target.resource.group,
        resource: target.resource.plural,
        subresource: target.subresource,
        namespace: target.namespace.as_deref(),
        name: target.name.as_deref().or_else(|| {
            matches!(verb, "list" | "watch")
                .then(|| selection.exact_name())
                .flatten()
        }),
    });
    if !api.rbac().await?.allows(&user, &attrs) {
        return Err(Failure::new(
            403,
            "Forbidden",
            format!(
                "user {} cannot {verb} {}",
                user.name, target.resource.plural
            ),
        ));
    }
    if target.resource.group == "rbac.authorization.k8s.io"
        && ["create", "update", "patch", "delete"].contains(&verb)
        && !user.is_superuser()
    {
        return Err(Failure::new(403,"Forbidden","RBAC mutations require the bootstrap administrator until escalation checks are implemented"));
    }
    if q.contains_key("dryRun") {
        return Err(bad("dryRun is not yet implemented"));
    }
    if target.resource.namespaced
        && target.namespace.is_none()
        && (target.name.is_some() || !matches!(verb, "list" | "watch"))
    {
        return Err(bad("namespaced writes and named reads require a namespace"));
    }
    if verb == "watch" {
        return watch_response(&api, &target, &q, selection).await;
    }
    if verb == "list" {
        return list_response(&api, &target, &q, selection).await;
    }
    if target.resource.namespaced && target.namespace.is_none() {
        return Err(bad("namespaced writes and named reads require a namespace"));
    }
    let prefix = target.prefix();
    if verb == "get" {
        let k = key(format!("{prefix}{}", target.name.as_ref().unwrap()))?;
        let obj = api
            .store
            .get(&k)
            .await?
            .ok_or_else(|| Failure::new(404, "NotFound", "object not found"))?;
        return Ok(Json(object(obj)?).into_response());
    }
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    let bytes = tokio::time::timeout(
        Duration::from_secs(30),
        to_bytes(request.into_body(), 2 * 1024 * 1024),
    )
    .await
    .map_err(|_| Failure::new(408, "Timeout", "request body timed out"))?
    .map_err(|_| {
        Failure::new(
            413,
            "RequestEntityTooLarge",
            "request body exceeds limit or failed to read",
        )
    })?;
    // Serialize admission with namespace policy/lifecycle changes and Service
    // allocation through commit in this single-server API.
    let _admission_guard = api.admission_writes.lock().await;
    let mut value = if verb == "patch" {
        let k = key(format!("{prefix}{}", target.name.as_ref().unwrap()))?;
        let current = api
            .store
            .get(&k)
            .await?
            .ok_or_else(|| Failure::new(404, "NotFound", "object not found"))?;
        let revision = current.revision.to_string();
        let mut patched = patch::apply(object(current)?, &bytes, &content_type)?;
        if !patched.is_object() || !patched["metadata"].is_object() {
            return Err(Failure::new(
                422,
                "Invalid",
                "patch must preserve object metadata",
            ));
        }
        if !patched["metadata"]["resourceVersion"].is_null()
            && !patched["metadata"]["resourceVersion"].is_string()
        {
            return Err(Failure::new(
                422,
                "Invalid",
                "resourceVersion must be a string",
            ));
        }
        if patched["metadata"]["resourceVersion"]
            .as_str()
            .is_some_and(|rv| rv != revision)
        {
            return Err(Failure::new(
                409,
                "Conflict",
                "patch resourceVersion precondition failed",
            ));
        }
        patched["metadata"]["resourceVersion"] = revision.into();
        patched
    } else {
        wire::decode(&bytes, &content_type)?
    };
    if target.subresource == Some("binding") {
        return bind(&api, &target, value).await;
    }
    if verb == "delete" {
        let k = key(format!("{prefix}{}", target.name.as_ref().unwrap()))?;
        let current = api
            .store
            .get(&k)
            .await?
            .ok_or_else(|| Failure::new(404, "NotFound", "object not found"))?;
        let obj = object(current.clone())?;
        for (field, actual) in [
            ("uid", obj["metadata"]["uid"].as_str().unwrap_or("")),
            (
                "resourceVersion",
                obj["metadata"]["resourceVersion"].as_str().unwrap_or(""),
            ),
        ] {
            if value["preconditions"][field]
                .as_str()
                .is_some_and(|v| v != actual)
            {
                return Err(Failure::new(409, "Conflict", "delete precondition failed"));
            }
        }
        if obj["metadata"]["finalizers"]
            .as_array()
            .is_some_and(|v| !v.is_empty())
        {
            return Err(Failure::new(
                409,
                "Conflict",
                "object has pending finalizers",
            ));
        }
        if target.resource.kind == "Namespace" {
            return Err(Failure::new(
                405,
                "MethodNotAllowed",
                "namespace deletion controller is not implemented",
            ));
        }
        api.store.delete(&k, current.revision).await?;
        return Ok(
            Json(json!({"apiVersion":"v1","kind":"Status","status":"Success","code":200}))
                .into_response(),
        );
    }
    if value["apiVersion"].as_str() != Some(&target.resource.api_version())
        || value["kind"].as_str() != Some(target.resource.kind)
    {
        return Err(bad("apiVersion/kind does not match the endpoint"));
    }
    if verb == "create" && value["metadata"]["name"].as_str().is_none_or(str::is_empty) {
        if let Some(prefix) = value["metadata"]["generateName"].as_str() {
            let suffix = uuid::Uuid::new_v4().simple().to_string();
            let generated = format!("{prefix}{}", &suffix[..8]);
            value["metadata"]["name"] = generated.into();
        }
    }
    let name = value["metadata"]["name"]
        .as_str()
        .ok_or_else(|| bad("metadata.name is required"))?
        .to_owned();
    if !target.resource.valid_name(&name) || target.name.as_ref().is_some_and(|n| n != &name) {
        return Err(bad("invalid or mismatched metadata.name"));
    }
    let mut namespace = None;
    match &target.namespace {
        Some(ns) => {
            if value["metadata"]["namespace"]
                .as_str()
                .is_some_and(|n| !n.is_empty() && n != ns)
            {
                return Err(bad("metadata.namespace mismatch"));
            }
            value["metadata"]["namespace"] = ns.clone().into();
            let stored = api
                .store
                .get(&key(format!("/registry/namespaces/{ns}"))?)
                .await?
                .ok_or_else(|| Failure::new(404, "NotFound", "namespace not found"))?;
            let current = object(stored)?;
            admission::lifecycle(&current, verb == "create")?;
            namespace = Some(current);
        }
        None => {
            if value["metadata"]["namespace"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            {
                return Err(bad("cluster-scoped resource cannot set namespace"));
            }
            value["metadata"]
                .as_object_mut()
                .expect("named object metadata")
                .remove("namespace");
        }
    }
    let k = key(format!("{prefix}{name}"))?;
    let mut old_value = None;
    let expected = if matches!(verb, "update" | "patch") {
        let rv = value["metadata"]["resourceVersion"]
            .as_str()
            .ok_or_else(|| bad("update requires metadata.resourceVersion"))?
            .parse::<u64>()
            .map_err(|_| bad("invalid resourceVersion"))?;
        let old = api
            .store
            .get(&k)
            .await?
            .ok_or_else(|| Failure::new(404, "NotFound", "object not found"))?;
        let old = object(old)?;
        if value["metadata"]["uid"]
            .as_str()
            .is_some_and(|uid| Some(uid) != old["metadata"]["uid"].as_str())
        {
            return Err(Failure::new(409, "Conflict", "UID is immutable"));
        }
        for f in ["uid", "creationTimestamp"] {
            value["metadata"][f] = old["metadata"][f].clone();
        }
        old_value = Some(old);
        Some(rv)
    } else {
        if value["metadata"]["resourceVersion"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
        {
            return Err(bad("create cannot set resourceVersion"));
        }
        value["metadata"]["uid"] = uuid::Uuid::new_v4().to_string().into();
        value["metadata"]["creationTimestamp"] = now().into();
        if target.resource.kind == "Namespace" {
            value["status"] = json!({"phase":"Active"});
        }
        None
    };
    if target.resource.kind == "Secret" {
        if let Some(strings) = value.as_object_mut().and_then(|v| v.remove("stringData")) {
            let strings = strings
                .as_object()
                .ok_or_else(|| bad("stringData must be a string map"))?;
            if value.get("data").is_none() {
                value["data"] = json!({});
            }
            if !value["data"].is_object() {
                return Err(bad("data must be a string map"));
            }
            for (k, v) in strings {
                value["data"][k] = base64::engine::general_purpose::STANDARD
                    .encode(
                        v.as_str()
                            .ok_or_else(|| bad("stringData values must be strings"))?,
                    )
                    .into();
            }
        }
    }
    let mut value = strategy::prepare(
        target.resource,
        value,
        old_value.as_ref(),
        target.subresource.is_some(),
    )?;
    if target.resource.kind == "Pod" && verb == "create" {
        serviceaccounts::admit(
            &api.store,
            target.namespace.as_deref().expect("Pod namespace"),
            &mut value,
        )
        .await?;
        value = target.resource.normalize(value)?;
    }
    if target.subresource.is_none() {
        match target.resource.kind {
            "Namespace" => admission::namespace(&value)?,
            "Pod" => admission::pod(namespace.as_ref().expect("namespaced Pod"), &value)?,
            _ => {}
        }
    }
    if target.resource.kind == "Service" && target.subresource.is_none() {
        services::assign(&api.store, &mut value, old_value.as_ref()).await?;
    }
    patch::check_size(&value)?;
    let obj = stored(k, &value)?;
    let result = match expected {
        Some(rv) => api.store.update(obj, rv).await?,
        None => api.store.create(obj).await?,
    };
    Ok((
        if expected.is_some() {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        },
        Json(object(result)?),
    )
        .into_response())
}
async fn bind(api: &Api, target: &Target, value: Value) -> Result<Response> {
    if value["kind"] != "Binding" || value["apiVersion"] != "v1" {
        return Err(bad("binding requires v1 Binding"));
    }
    let binding: k8s_openapi::api::core::v1::Binding = serde_json::from_value(value)?;
    if binding.metadata.name.as_ref() != target.name.as_ref()
        || binding
            .metadata
            .namespace
            .as_ref()
            .is_some_and(|ns| Some(ns) != target.namespace.as_ref())
    {
        return Err(bad("binding metadata does not match endpoint"));
    }
    if binding.target.kind.as_deref() != Some("Node")
        || binding
            .target
            .api_version
            .as_deref()
            .is_some_and(|v| v != "v1")
    {
        return Err(bad("binding target must be a v1 Node"));
    }
    let node = binding
        .target
        .name
        .as_deref()
        .filter(|n| resources::valid_name(n))
        .ok_or_else(|| bad("binding target node name is required"))?;
    if api
        .store
        .get(&key(format!("/registry/nodes/{node}"))?)
        .await?
        .is_none()
    {
        return Err(Failure::new(
            404,
            "NotFound",
            "binding target node not found",
        ));
    }
    let k = key(format!(
        "{}{}",
        target.prefix(),
        target.name.as_ref().unwrap()
    ))?;
    let stored_pod = api
        .store
        .get(&k)
        .await?
        .ok_or_else(|| Failure::new(404, "NotFound", "Pod not found"))?;
    let revision = stored_pod.revision;
    let mut pod = object(stored_pod)?;
    if pod["spec"]["nodeName"]
        .as_str()
        .is_some_and(|n| !n.is_empty())
        || !pod["metadata"]["deletionTimestamp"].is_null()
    {
        return Err(Failure::new(
            409,
            "Conflict",
            "Pod is already bound or terminating",
        ));
    }
    if binding
        .metadata
        .uid
        .as_deref()
        .is_some_and(|uid| Some(uid) != pod["metadata"]["uid"].as_str())
        || binding
            .metadata
            .resource_version
            .as_deref()
            .is_some_and(|rv| rv != revision.to_string())
    {
        return Err(Failure::new(409, "Conflict", "binding precondition failed"));
    }
    pod["spec"]["nodeName"] = node.into();
    let generation = pod["metadata"]["generation"].as_i64().unwrap_or(1);
    pod["metadata"]["generation"] = generation
        .checked_add(1)
        .ok_or_else(|| Failure::new(422, "Invalid", "generation overflow"))?
        .into();
    api.store.update(stored(k, &pod)?, revision).await?;
    Ok((
        StatusCode::CREATED,
        Json(json!({"apiVersion":"v1","kind":"Status","status":"Success","code":201})),
    )
        .into_response())
}
fn discovery(path: &str) -> Option<Value> {
    if path == "/api" {
        return Some(
            json!({"apiVersion":"v1","kind":"APIVersions","versions":["v1"],"serverAddressByClientCIDRs":[]}),
        );
    }
    let groups: std::collections::BTreeSet<_> = RESOURCES
        .iter()
        .map(|r| r.group)
        .filter(|g| !g.is_empty())
        .collect();
    let group = |name: &str| json!({"name":name,"versions":[{"groupVersion":format!("{name}/v1"),"version":"v1"}],"preferredVersion":{"groupVersion":format!("{name}/v1"),"version":"v1"}});
    if path == "/apis" {
        return Some(
            json!({"apiVersion":"v1","kind":"APIGroupList","groups":groups.iter().map(|g| group(g)).collect::<Vec<_>>()}),
        );
    }
    for name in &groups {
        if path == format!("/apis/{name}") {
            let mut result = group(name);
            result["apiVersion"] = "v1".into();
            result["kind"] = "APIGroup".into();
            return Some(result);
        }
    }
    let gv = if path == "/api/v1" {
        "v1"
    } else {
        path.strip_prefix("/apis/")?
    };
    let resources: Vec<_> = RESOURCES.iter().filter(|r| r.api_version() == gv).collect();
    if resources.is_empty() {
        return None;
    }
    let mut entries = vec![];
    for resource in resources {
        let mut verbs = vec!["get", "list", "watch", "create", "update", "patch"];
        if resource.kind != "Namespace" {
            verbs.push("delete");
        }
        entries.push(json!({"name":resource.plural,"singularName":resource.kind.to_ascii_lowercase(),"namespaced":resource.namespaced,"kind":resource.kind,"verbs":verbs}));
        if resource.kind == "Pod" {
            entries.push(json!({"name":"pods/binding","singularName":"","namespaced":true,"kind":"Binding","verbs":["create"]}));
        }
        if resource.has_status() {
            entries.push(json!({"name":format!("{}/status",resource.plural),"singularName":"","namespaced":resource.namespaced,"kind":resource.kind,"verbs":["get","update","patch"]}));
        }
    }
    Some(json!({"apiVersion":"v1","kind":"APIResourceList","groupVersion":gv,"resources":entries}))
}
#[derive(Serialize, Deserialize)]
struct Continue {
    revision: u64,
    key: String,
    prefix: String,
    labels: String,
    fields: String,
}
fn number(q: &BTreeMap<String, String>, name: &str) -> Result<Option<u64>> {
    q.get(name)
        .map(|v| v.parse().map_err(|_| bad("invalid numeric query value")))
        .transpose()
}
async fn list_response(
    api: &Api,
    target: &Target,
    q: &BTreeMap<String, String>,
    selection: Selection,
) -> Result<Response> {
    let mut sel = ListSelect::new(target.prefix());
    sel.at_revision = number(q, "resourceVersion")?;
    let limit = number(q, "limit")?
        .filter(|l| *l > 0)
        .unwrap_or(256)
        .min(4096) as usize;
    let labels = q.get("labelSelector").cloned().unwrap_or_default();
    let fields = q.get("fieldSelector").cloned().unwrap_or_default();
    if let Some(token) = q.get("continue").filter(|s| !s.is_empty()) {
        let c: Continue = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(token)
                .map_err(|_| bad("invalid continue token"))?,
        )?;
        if c.prefix != target.prefix() || c.labels != labels || c.fields != fields {
            return Err(bad("continue token scope or selector mismatch"));
        }
        if sel
            .at_revision
            .is_some_and(|rv| rv != 0 && rv != c.revision)
        {
            return Err(bad("continue token resourceVersion mismatch"));
        }
        sel.at_revision = Some(c.revision);
        sel.start_after = Some(key(c.key)?);
    }
    let page = api.store.list(sel.clone()).await?;
    let revision = page.revision;
    sel.at_revision = Some(revision);
    let store = api.store.clone();
    let prefix = target.prefix();
    let start = format!(
        "{{\"apiVersion\":{},\"kind\":{},\"items\":[",
        json!(target.resource.api_version()),
        json!(format!("{}List", target.resource.kind))
    );
    // Continuation is known after filtering. Emit metadata after the streamed
    // items; JSON object member order is immaterial to Kubernetes clients.
    let stream = async_stream::try_stream! {
        yield Bytes::from(start);
        let mut page=page;let mut returned=0;let mut scanned=0;let mut next=None;
        'scan: loop {
            let count=page.items.len();
            for(i,obj)in page.items.into_iter().enumerate(){
                scanned+=1;let cursor=obj.key.clone();
                let value=object(obj).map_err(|_|std::io::Error::other("invalid stored list object"))?;
                if selection.matches(&value){
                    if returned>0{yield Bytes::from_static(b",");}
                    yield Bytes::from(serde_json::to_vec(&value).map_err(std::io::Error::other)?);returned+=1;
                }
                if returned>=limit||scanned>=4096{
                    if i+1<count||page.next_after.is_some(){next=Some(cursor);}break 'scan;
                }
            }
            let Some(cursor)=page.next_after else{break;};sel.start_after=Some(cursor);
            page=store.list(sel.clone()).await.map_err(|_|std::io::Error::other("registry snapshot unavailable during list"))?;
        }
        let token=next.map(|k|URL_SAFE_NO_PAD.encode(serde_json::to_vec(&Continue{revision,key:k.as_str().into(),prefix,labels,fields}).expect("continuation JSON"))).unwrap_or_default();
        yield Bytes::from(format!("],\"metadata\":{}}}",json!({"resourceVersion":revision.to_string(),"continue":token})));
    };
    let stream: std::pin::Pin<Box<dyn futures_util::Stream<Item = std::io::Result<Bytes>> + Send>> =
        Box::pin(stream);
    Ok((
        [("content-type", "application/json")],
        Body::from_stream(stream),
    )
        .into_response())
}
async fn watch_response(
    api: &Api,
    target: &Target,
    q: &BTreeMap<String, String>,
    selection: Selection,
) -> Result<Response> {
    let mut stream = api
        .store
        .watch(WatchSelect::new(
            target.prefix(),
            number(q, "resourceVersion")?,
        ))
        .await?;
    let timeout = Duration::from_secs(number(q, "timeoutSeconds")?.unwrap_or(300).clamp(1, 600));
    let deadline = tokio::time::Instant::now() + timeout;
    let kind = target.resource.kind;
    let version = target.resource.api_version();
    let bookmarks = q.get("allowWatchBookmarks").is_some_and(|v| v == "true");
    let out = async_stream::stream! {
        while let Ok(Some(event))=tokio::time::timeout_at(deadline,stream.next()).await{
            let wire=match event{
                Err(e)=>{yield Ok::<Bytes,Infallible>(Bytes::from(format!("{}\n",json!({"type":"ERROR","object":Failure::from(e).value()}))));break;},
                Ok(event)if event.kind==EventKind::Bookmark=>{
                    if !bookmarks{continue;}
                    json!({"type":"BOOKMARK","object":{"apiVersion":version,"kind":kind,"metadata":{"resourceVersion":event.revision.to_string()}}})
                },
                Ok(event)=>{
                    let converted=(||->Result<_>{Ok((event.object.map(object).transpose()?,event.previous.map(object).transpose()?))})();
                    let (current,previous)=match converted{Ok(pair)=>pair,Err(e)=>{yield Ok(Bytes::from(format!("{}\n",json!({"type":"ERROR","object":e.value()}))));break;}};
                    let before=previous.as_ref().is_some_and(|v|selection.matches(v));
                    let after=event.kind!=EventKind::Deleted&&current.as_ref().is_some_and(|v|selection.matches(v));
                    let (typ,mut value)=match(before,after){
                        (false,true)=>("ADDED",current.unwrap()),(true,true)=>("MODIFIED",current.unwrap()),
                        (true,false)=>("DELETED",previous.unwrap()),(false,false)=>continue,
                    };
                    value["metadata"]["resourceVersion"]=event.revision.to_string().into();
                    json!({"type":typ,"object":value})
                }
            };
            yield Ok::<Bytes,Infallible>(Bytes::from(format!("{wire}\n")));
        }
    };
    Ok((
        [("content-type", "application/json")],
        Body::from_stream(out),
    )
        .into_response())
}
