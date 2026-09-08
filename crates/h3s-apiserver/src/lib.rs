//! Authenticated Kubernetes API foundation. All registry access belongs here;
//! future controllers and nodes must use this API rather than write its store.
mod resources;
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
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{collections::BTreeMap, convert::Infallible, sync::Arc, time::Duration};
pub use transport::serve;
use transport::Peer;

pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
#[derive(Clone)]
pub struct Api {
    store: Arc<dyn Storage>,
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
        let api = Self { store };
        for namespace in ["default", "kube-system", "kube-public", "kube-node-lease"] {
            api.bootstrap(format!("/registry/namespaces/{namespace}"),json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":namespace},"status":{"phase":"Active"}})).await?;
        }
        api.bootstrap("/registry/clusterroles/h3s-discovery".into(),json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole","metadata":{"name":"h3s-discovery"},"rules":[{"verbs":["get"],"nonResourceURLs":["/api","/api/*","/apis","/apis/*"]}]})).await?;
        api.bootstrap("/registry/clusterrolebindings/h3s-discovery".into(),json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRoleBinding","metadata":{"name":"h3s-discovery"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"ClusterRole","name":"h3s-discovery"},"subjects":[{"kind":"Group","apiGroup":"rbac.authorization.k8s.io","name":"system:authenticated"}]})).await?;
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
    let path = request.uri().path().to_owned();
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
        ("POST", false, _) => "create",
        ("PUT", true, _) => "update",
        ("DELETE", true, _) => "delete",
        _ => {
            return Err(Failure::new(
                405,
                "MethodNotAllowed",
                "verb is not implemented for this resource",
            ))
        }
    };
    let attrs = AuthRequest::Resource(ResourceRequest {
        verb,
        group: target.resource.group,
        resource: target.resource.plural,
        subresource: None,
        namespace: target.namespace.as_deref(),
        name: target.name.as_deref(),
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
        && ["create", "update", "delete"].contains(&verb)
        && !user.is_superuser()
    {
        return Err(Failure::new(403,"Forbidden","RBAC mutations require the bootstrap administrator until escalation checks are implemented"));
    }
    // Reject unsupported selection instead of returning an unfiltered result.
    for field in ["labelSelector", "fieldSelector"] {
        if q.get(field).is_some_and(|s| !s.is_empty()) {
            return Err(bad("selectors are not yet implemented"));
        }
    }
    if q.contains_key("dryRun") {
        return Err(bad("dryRun is not yet implemented"));
    }
    if verb == "watch" {
        return watch_response(&api, &target, &q).await;
    }
    if verb == "list" {
        return list_response(&api, &target, &q).await;
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
    let mut value = wire::decode(&bytes, &content_type)?;
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
    let name = value["metadata"]["name"]
        .as_str()
        .ok_or_else(|| bad("metadata.name is required"))?
        .to_owned();
    if !resources::valid_name(&name) || target.name.as_ref().is_some_and(|n| n != &name) {
        return Err(bad("invalid or mismatched metadata.name"));
    }
    match &target.namespace {
        Some(ns) => {
            if value["metadata"]["namespace"]
                .as_str()
                .is_some_and(|n| n != ns)
            {
                return Err(bad("metadata.namespace mismatch"));
            }
            value["metadata"]["namespace"] = ns.clone().into();
            if api
                .store
                .get(&key(format!("/registry/namespaces/{ns}"))?)
                .await?
                .is_none()
            {
                return Err(Failure::new(404, "NotFound", "namespace not found"));
            }
        }
        None => {
            if value["metadata"]["namespace"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            {
                return Err(bad("cluster-scoped resource cannot set namespace"));
            }
        }
    }
    let k = key(format!("{prefix}{name}"))?;
    let expected = if verb == "update" {
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
        if target.resource.kind == "Namespace" {
            value["status"] = old["status"].clone();
        }
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
    let value = target.resource.normalize(value)?;
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
fn discovery(path: &str) -> Option<Value> {
    if path == "/api" {
        return Some(
            json!({"apiVersion":"v1","kind":"APIVersions","versions":["v1"],"serverAddressByClientCIDRs":[]}),
        );
    }
    let group = json!({"name":"rbac.authorization.k8s.io","versions":[{"groupVersion":"rbac.authorization.k8s.io/v1","version":"v1"}],"preferredVersion":{"groupVersion":"rbac.authorization.k8s.io/v1","version":"v1"}});
    if path == "/apis" {
        return Some(json!({"apiVersion":"v1","kind":"APIGroupList","groups":[group]}));
    }
    if path == "/apis/rbac.authorization.k8s.io" {
        let mut group = group;
        group["apiVersion"] = "v1".into();
        group["kind"] = "APIGroup".into();
        return Some(group);
    }
    let gv = match path {
        "/api/v1" => "v1",
        "/apis/rbac.authorization.k8s.io/v1" => "rbac.authorization.k8s.io/v1",
        _ => return None,
    };
    Some(
        json!({"apiVersion":"v1","kind":"APIResourceList","groupVersion":gv,"resources":RESOURCES.iter().filter(|r|r.api_version()==gv).map(|r|json!({"name":r.plural,"singularName":r.kind.to_ascii_lowercase(),"namespaced":r.namespaced,"kind":r.kind,"verbs":if r.kind=="Namespace"{vec!["get","list","watch","create","update"]}else{vec!["get","list","watch","create","update","delete"]}})).collect::<Vec<_>>()}),
    )
}
#[derive(Serialize, Deserialize)]
struct Continue {
    revision: u64,
    key: String,
    prefix: String,
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
) -> Result<Response> {
    let mut sel = ListSelect::new(target.prefix());
    sel.at_revision = number(q, "resourceVersion")?;
    sel.limit = number(q, "limit")?
        .filter(|l| *l > 0)
        .unwrap_or(256)
        .min(4096) as usize;
    if let Some(token) = q.get("continue").filter(|s| !s.is_empty()) {
        let c: Continue = serde_json::from_slice(
            &URL_SAFE_NO_PAD
                .decode(token)
                .map_err(|_| bad("invalid continue token"))?,
        )?;
        if c.prefix != target.prefix() {
            return Err(bad("continue token scope mismatch"));
        }
        sel.at_revision = Some(c.revision);
        sel.start_after = Some(key(c.key)?);
    }
    let page = api.store.list(sel).await?;
    let next = page
        .next_after
        .map(|k| {
            URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&Continue {
                    revision: page.revision,
                    key: k.as_str().into(),
                    prefix: target.prefix(),
                })
                .unwrap(),
            )
        })
        .unwrap_or_default();
    let start = format!(
        "{{\"apiVersion\":{},\"kind\":{},\"metadata\":{},\"items\":[",
        json!(target.resource.api_version()),
        json!(format!("{}List", target.resource.kind)),
        json!({"resourceVersion":page.revision.to_string(),"continue":next})
    );
    let stream = async_stream::stream! {
        yield Ok::<Bytes,Infallible>(Bytes::from(start));
        for(i,obj)in page.items.into_iter().enumerate(){
            if i>0{yield Ok(Bytes::from_static(b","));}
            match object(obj){Ok(value)=>yield Ok(Bytes::from(serde_json::to_vec(&value).unwrap())),Err(_)=>return,}
        }
        yield Ok(Bytes::from_static(b"]}"));
    };
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
) -> Result<Response> {
    if target.name.is_some() {
        return Err(bad("named watches are not yet implemented"));
    }
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
        while let Ok(Some(event))=tokio::time::timeout_at(deadline,stream.next()).await {
            let (wire,done)=match event{
                Ok(event)=>{
                    let typ=match event.kind{EventKind::Added=>"ADDED",EventKind::Modified=>"MODIFIED",EventKind::Deleted=>"DELETED",EventKind::Bookmark=>{if !bookmarks{continue;}"BOOKMARK"}};
                    let value=match event.object{Some(obj)=>match object(obj){Ok(v)=>v,Err(e)=>{yield Ok::<Bytes,Infallible>(Bytes::from(format!("{}\n",json!({"type":"ERROR","object":e.value()}))));break;}},None=>json!({"apiVersion":version,"kind":kind,"metadata":{"resourceVersion":event.revision.to_string()}})};
                    (json!({"type":typ,"object":value}),false)
                },Err(e)=>(json!({"type":"ERROR","object":Failure::from(e).value()}),true),
            };
            yield Ok::<Bytes,Infallible>(Bytes::from(format!("{wire}\n")));if done{break;}
        }
    };
    Ok((
        [("content-type", "application/json")],
        Body::from_stream(out),
    )
        .into_response())
}
