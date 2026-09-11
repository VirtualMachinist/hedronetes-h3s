//! The HTTP surface that is not a Kubernetes resource: path and query
//! parsing, bounded request bodies, health, discovery, OpenAPI and the node
//! CIDR ledger.
use crate::{authz, bad, node_cidrs, openapi, resources::RESOURCES, Api, Failure, Result};
use axum::{
    body::{to_bytes, Body, Bytes},
    http::{HeaderValue, Request, Uri},
    response::{IntoResponse, Response},
    Json,
};
use h3s_auth::User;
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Duration};

/// Decode each segment once, including colon-bearing RBAC names. Encoded
/// separators may not turn a name into a different resource/subresource.
pub(crate) fn path(uri: &Uri) -> Result<String> {
    let segments: Result<Vec<_>> = uri
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
    Ok(segments?.join("/"))
}

pub(crate) fn health(method: &str, path: &str) -> Option<Response> {
    if method != "GET" || !["/livez", "/readyz", "/version"].contains(&path) {
        return None;
    }
    Some(if path == "/version" {
        Json(json!({"major":"1","minor":"34","gitVersion":"v1.34.0+h3s","platform":format!("{}/{}",std::env::consts::OS,match std::env::consts::ARCH{"aarch64"=>"arm64","x86_64"=>"amd64",other=>other})})).into_response()
    } else {
        "ok\n".into_response()
    })
}

/// Query parameters; only `command` (exec) may repeat and keeps its order.
pub(crate) struct Query {
    pub params: BTreeMap<String, String>,
    pub commands: Vec<String>,
}
impl Query {
    pub fn parse(uri: &Uri) -> Result<Self> {
        let pairs: Vec<(String, String)> = serde_urlencoded::from_str(uri.query().unwrap_or(""))
            .map_err(|_| bad("invalid query"))?;
        let mut params = BTreeMap::new();
        let mut commands = Vec::new();
        for (k, v) in pairs {
            if k == "command" {
                commands.push(v);
                continue;
            }
            if params.insert(k, v).is_some() {
                return Err(bad("duplicate query parameter"));
            }
        }
        Ok(Self { params, commands })
    }
    pub fn get(&self, name: &str) -> Option<&str> {
        self.params.get(name).map(String::as_str)
    }
    pub fn has(&self, name: &str) -> bool {
        self.params.contains_key(name)
    }
}

/// Content type and the bounded body of a write.
pub(crate) async fn body(request: Request<Body>) -> Result<(String, Bytes)> {
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
    Ok((content_type, bytes))
}

fn read_only(method: &str, what: &'static str) -> Result<()> {
    if method != "GET" {
        return Err(Failure::new(405, "MethodNotAllowed", what));
    }
    Ok(())
}

/// Node CIDR ledger, discovery and OpenAPI. `None` means the path is a
/// resource request.
pub(crate) async fn non_resource(
    api: &Api,
    user: &User,
    method: &str,
    path: &str,
    query: &Query,
    accept: Option<&HeaderValue>,
) -> Result<Option<Response>> {
    if path == h3s_api::network::NODE_CIDR_PATH {
        read_only(method, "node CIDR ledger is read-only")?;
        // client-go appends its transport deadline as `timeout`, including to
        // kubectl --raw requests. It does not change snapshot semantics.
        if query.params.keys().any(|key| key != "timeout") {
            return Err(bad(
                "node CIDR snapshot accepts only the client timeout parameter",
            ));
        }
        authz::non_resource(api, user, path, "node CIDR snapshot access denied").await?;
        let configured = api.node_cidrs.as_ref().ok_or_else(|| {
            Failure::new(
                503,
                "ServiceUnavailable",
                "node CIDR allocation is not configured",
            )
        })?;
        let snapshot = node_cidrs::snapshot(&api.store, configured).await?;
        return Ok(Some(Json(snapshot).into_response()));
    }
    if let Some(discovery) = discovery(path) {
        read_only(method, "discovery is read-only")?;
        authz::non_resource(api, user, path, "discovery access denied").await?;
        return Ok(Some(Json(discovery).into_response()));
    }
    if path == "/openapi/v2" || path == "/swagger.json" {
        read_only(method, "OpenAPI is read-only")?;
        if query
            .params
            .keys()
            .any(|key| !matches!(key.as_str(), "timeout" | "timeoutSeconds"))
        {
            return Err(bad("OpenAPI accepts only the client timeout parameter"));
        }
        authz::non_resource(api, user, "/openapi/v2", "OpenAPI access denied").await?;
        return Ok(Some(openapi::v2(accept)));
    }
    Ok(None)
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
        let mut entry = json!({"name":resource.plural,"singularName":resource.kind.to_ascii_lowercase(),"namespaced":resource.namespaced,"kind":resource.kind,"verbs":verbs,"shortNames":[]});
        if resource.kind == "Namespace" {
            entry["shortNames"] = json!(["ns"]);
        }
        entries.push(entry);
        if resource.kind == "Pod" {
            entries.push(json!({"name":"pods/binding","singularName":"","namespaced":true,"kind":"Binding","verbs":["create"]}));
            entries.push(json!({"name":"pods/log","singularName":"","namespaced":true,"kind":"Pod","verbs":["get"]}));
            entries.push(json!({"name":"pods/exec","singularName":"","namespaced":true,"kind":"Pod","verbs":["create"]}));
        }
        if resource.has_status() {
            entries.push(json!({"name":format!("{}/status",resource.plural),"singularName":"","namespaced":resource.namespaced,"kind":resource.kind,"verbs":["get","update","patch"]}));
        }
    }
    Some(json!({"apiVersion":"v1","kind":"APIResourceList","groupVersion":gv,"resources":entries}))
}
