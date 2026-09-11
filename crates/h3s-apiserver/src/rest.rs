//! Resource requests: resolve the target and verb, authorize, then serve a
//! read, Pod I/O, or hand the mutation to the write pipeline.
use crate::{
    authz, bad, http::Query, pod_io, read, resources::Target, selectors::Selection, write, Api,
    Failure, Result,
};
use axum::{body::Body, http::Request, response::Response};
use h3s_auth::User;

pub(crate) async fn execute(
    api: &Api,
    user: &User,
    path: &str,
    query: Query,
    request: Request<Body>,
) -> Result<Response> {
    let target = Target::parse(path).ok_or_else(|| {
        Failure::new(
            404,
            "NotFound",
            "resource or subresource is not implemented",
        )
    })?;
    let watch = query.get("watch").is_some_and(|v| v == "true" || v == "1");
    let verb = verb(request.method().as_str(), &target, watch)?;
    let selection = Selection::parse(
        query.get("labelSelector").unwrap_or(""),
        query.get("fieldSelector").unwrap_or(""),
        target.resource.kind,
    )?
    .with_name(target.name.as_deref());
    let grant = authz::resource(api, user, &target, verb, selection).await?;
    if query.has("sendInitialEvents") && verb != "watch" {
        return Err(bad("sendInitialEvents requires watch=true"));
    }
    if target.resource.namespaced
        && target.namespace.is_none()
        && (target.name.is_some() || !matches!(verb, "list" | "watch"))
    {
        return Err(bad("namespaced writes and named reads require a namespace"));
    }
    match (target.subresource, verb) {
        (Some("log"), _) => pod_io::logs(api, &target, &query.params).await,
        (Some("exec"), _) => {
            pod_io::exec(api, &target, &query.params, &query.commands, request).await
        }
        (_, "watch") => {
            read::watch(
                api,
                &target,
                &query.params,
                grant.selection,
                grant.read_guard,
            )
            .await
        }
        (_, "list") => {
            read::list(
                api,
                &target,
                &query.params,
                grant.selection,
                grant.read_guard,
            )
            .await
        }
        (_, "get") => read::get(api, &target).await,
        _ => write::execute(api, user, &target, verb, &query, request).await,
    }
}

/// The Kubernetes verb for a method and target, checked against what each
/// subresource supports.
fn verb(method: &str, target: &Target, watch: bool) -> Result<&'static str> {
    let verb = if target.subresource == Some("exec")
        && matches!(method, "GET" | "POST")
        && target.name.is_some()
        && !watch
    {
        "create"
    } else {
        match (method, target.name.is_some(), watch) {
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
        }
    };
    let (supported, message) = match target.subresource {
        Some("status") => (
            matches!(verb, "get" | "update" | "patch"),
            "status supports get, update and patch",
        ),
        Some("binding") => (verb == "create", "binding supports create"),
        Some("log") => (verb == "get", "log supports get"),
        Some("exec") => (verb == "create", "exec supports create"),
        _ => (true, ""),
    };
    if !supported {
        return Err(Failure::new(405, "MethodNotAllowed", message));
    }
    Ok(verb)
}
