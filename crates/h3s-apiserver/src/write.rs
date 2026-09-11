//! The write pipeline: decode → concurrency → prepare → admit → persist.
//! Delete and Pod binding decode their own option objects and commit under
//! the same admission lock, but never run the resource strategy.
use crate::{
    admission, bad, http,
    http::Query,
    key, named, node_cidrs, nodes, now, object, patch,
    resources::{self, Target},
    serviceaccounts, services, stored, strategy, wire, Api, Failure, Result,
};
use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use h3s_auth::User;
use h3s_storage::{StoreKey, StoredObject};
use serde_json::{json, Value};

pub(crate) async fn execute(
    api: &Api,
    user: &User,
    target: &Target,
    verb: &str,
    query: &Query,
    request: Request<Body>,
) -> Result<Response> {
    // dryRun only means something to a mutation; reads ignore it like upstream.
    if query.has("dryRun") {
        return Err(bad("dryRun is not yet implemented"));
    }
    let (content_type, bytes) = http::body(request).await?;
    // Serialize admission with namespace policy/lifecycle changes and Service
    // allocation through commit in this single-server API.
    let _admission_guard = api.admission_writes.lock().await;
    let write = Write {
        api,
        user,
        target,
        verb,
    };
    if target.subresource == Some("binding") {
        if user.node_name().is_some() {
            return Err(Failure::new(403, "Forbidden", "nodes may not bind Pods"));
        }
        return bind(api, target, wire::decode(&bytes, &content_type)?).await;
    }
    if verb == "delete" {
        return write.delete(wire::decode(&bytes, &content_type)?).await;
    }
    let Decoded {
        mut value,
        key,
        namespace,
    } = write.decode(&bytes, &content_type).await?;
    let (expected, old) = write.concurrency(&key, &mut value).await?;
    let mut value = write.prepare(value, old.as_ref())?;
    write
        .admit(&key, &mut value, old.as_ref(), namespace.as_ref())
        .await?;
    write.persist(key, value, expected).await
}

struct Write<'a> {
    api: &'a Api,
    user: &'a User,
    target: &'a Target,
    verb: &'a str,
}
struct Decoded {
    value: Value,
    key: StoreKey,
    /// The enclosing Namespace object of a namespaced write.
    namespace: Option<Value>,
}
async fn fetch(api: &Api, k: &StoreKey, missing: &'static str) -> Result<StoredObject> {
    api.store
        .get(k)
        .await?
        .ok_or_else(|| Failure::new(404, "NotFound", missing))
}
fn success(code: u16) -> Response {
    (
        StatusCode::from_u16(code).expect("known status"),
        Json(json!({"apiVersion":"v1","kind":"Status","status":"Success","code":code})),
    )
        .into_response()
}

impl Write<'_> {
    /// Wire bytes (or a patch over the stored object) become one normalized
    /// object whose apiVersion, kind, name and namespace match the endpoint.
    /// `Resource::normalize` runs here and nowhere else on the write path.
    async fn decode(&self, bytes: &[u8], content_type: &str) -> Result<Decoded> {
        let target = self.target;
        let value = if self.verb == "patch" {
            self.patched(bytes, content_type).await?
        } else {
            wire::decode(bytes, content_type)?
        };
        if value["apiVersion"].as_str() != Some(&target.resource.api_version())
            || value["kind"].as_str() != Some(target.resource.kind)
        {
            return Err(bad("apiVersion/kind does not match the endpoint"));
        }
        let mut value = target.resource.normalize(value)?;
        if self.verb == "create" && value["metadata"]["name"].as_str().is_none_or(str::is_empty) {
            if let Some(prefix) = value["metadata"]["generateName"].as_str() {
                let suffix = uuid::Uuid::new_v4().simple().to_string();
                value["metadata"]["name"] = format!("{prefix}{}", &suffix[..8]).into();
            }
        }
        let name = value["metadata"]["name"]
            .as_str()
            .ok_or_else(|| bad("metadata.name is required"))?
            .to_owned();
        if !target.resource.valid_name(&name) || target.name.as_ref().is_some_and(|n| n != &name) {
            return Err(bad("invalid or mismatched metadata.name"));
        }
        let namespace = match &target.namespace {
            Some(ns) => {
                if value["metadata"]["namespace"]
                    .as_str()
                    .is_some_and(|n| !n.is_empty() && n != ns)
                {
                    return Err(bad("metadata.namespace mismatch"));
                }
                value["metadata"]["namespace"] = ns.clone().into();
                let k = key(format!("/registry/namespaces/{ns}"))?;
                let current = object(fetch(self.api, &k, "namespace not found").await?)?;
                admission::lifecycle(&current, self.verb == "create")?;
                Some(current)
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
                    .expect("normalized metadata")
                    .remove("namespace");
                None
            }
        };
        Ok(Decoded {
            value,
            key: key(format!("{}{name}", target.prefix()))?,
            namespace,
        })
    }
    /// A patch applies to the stored object. The result must keep metadata
    /// and may only name the revision it was computed against.
    async fn patched(&self, bytes: &[u8], content_type: &str) -> Result<Value> {
        let current = fetch(self.api, &named(self.target)?, "object not found").await?;
        let revision = current.revision.to_string();
        let mut patched =
            patch::apply(self.target.resource, object(current)?, bytes, content_type)?;
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
        Ok(patched)
    }
    /// Update and patch name the revision they observed; create refuses one.
    /// Returns the expected revision and the stored object for updates.
    async fn concurrency(
        &self,
        k: &StoreKey,
        value: &mut Value,
    ) -> Result<(Option<u64>, Option<Value>)> {
        if !matches!(self.verb, "update" | "patch") {
            if value["metadata"]["resourceVersion"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
            {
                return Err(bad("create cannot set resourceVersion"));
            }
            value["metadata"]["uid"] = uuid::Uuid::new_v4().to_string().into();
            value["metadata"]["creationTimestamp"] = now().into();
            return Ok((None, None));
        }
        let old_stored = fetch(self.api, k, "object not found").await?;
        // Only Helm-owned ConfigMaps and Secrets accept unconditional PUT.
        // Other writes must carry the revision they observed.
        let rv = match value["metadata"]["resourceVersion"]
            .as_str()
            .filter(|s| !s.is_empty())
        {
            Some(s) => {
                let rv = s
                    .parse::<u64>()
                    .map_err(|_| bad("invalid resourceVersion"))?;
                if old_stored.revision != rv {
                    return Err(Failure::new(
                        409,
                        "Conflict",
                        "resourceVersion precondition failed",
                    ));
                }
                rv
            }
            None
                if matches!(self.target.resource.kind, "ConfigMap" | "Secret")
                    && strategy::helm_owned(value, self.target.resource.kind) =>
            {
                old_stored.revision
            }
            None => return Err(bad("update requires metadata.resourceVersion")),
        };
        let old = object(old_stored)?;
        if value["metadata"]["uid"]
            .as_str()
            .is_some_and(|uid| Some(uid) != old["metadata"]["uid"].as_str())
        {
            return Err(Failure::new(409, "Conflict", "UID is immutable"));
        }
        for f in ["uid", "creationTimestamp"] {
            value["metadata"][f] = old["metadata"][f].clone();
        }
        Ok((Some(rv), Some(old)))
    }
    /// Kind strategy: defaults, validation and immutability against the
    /// stored object. Secret stringData folds into data first.
    fn prepare(&self, mut value: Value, old: Option<&Value>) -> Result<Value> {
        if self.target.resource.kind == "Secret" {
            if let Some(strings) = value.as_object_mut().and_then(|v| v.remove("stringData")) {
                let strings = strings
                    .as_object()
                    .ok_or_else(|| bad("stringData must be a string map"))?;
                if !value["data"].is_object() {
                    value["data"] = json!({});
                }
                for (k, v) in strings {
                    let plain = v
                        .as_str()
                        .ok_or_else(|| bad("stringData values must be strings"))?;
                    value["data"][k] = STANDARD.encode(plain).into();
                }
            }
        }
        strategy::prepare(
            self.target.resource,
            value,
            old,
            self.target.subresource.is_some(),
        )
    }
    /// Admission after the strategy: ServiceAccount projection, namespace and
    /// Pod policy, Service IP allocation, node restriction, size, node CIDRs.
    async fn admit(
        &self,
        k: &StoreKey,
        value: &mut Value,
        old: Option<&Value>,
        namespace: Option<&Value>,
    ) -> Result<()> {
        let target = self.target;
        if target.resource.kind == "Pod" && self.verb == "create" {
            let ns = target.namespace.as_deref().expect("Pod namespace");
            serviceaccounts::admit(&self.api.store, ns, value).await?;
        }
        if target.subresource.is_none() {
            match target.resource.kind {
                "Namespace" => admission::namespace(value)?,
                "Pod" => admission::pod(namespace.expect("namespaced Pod"), value)?,
                "Service" => services::assign(&self.api.store, value, old).await?,
                _ => {}
            }
        }
        nodes::admit(self.user, target, self.verb, value, old)?;
        patch::check_size(value)?;
        if target.resource.kind == "Node" {
            if let Some(configured) = &self.api.node_cidrs {
                // Do not reserve a subnet for a duplicate create that will fail.
                if old.is_none() && self.api.store.get(k).await?.is_some() {
                    return Err(Failure::new(409, "AlreadyExists", "Node already exists"));
                }
                node_cidrs::admit(&self.api.store, configured, value, old).await?;
            }
        }
        Ok(())
    }
    /// Compare-and-set commit. A Namespace whose last finalizer just cleared
    /// is removed in the same request.
    async fn persist(&self, k: StoreKey, value: Value, expected: Option<u64>) -> Result<Response> {
        let obj = stored(k.clone(), &value)?;
        let result = match expected {
            Some(rv) => self.api.store.update(obj, rv).await?,
            None => self.api.store.create(obj).await?,
        };
        if self.target.resource.kind == "Namespace" && expected.is_some() {
            let current = object(result.clone())?;
            if deletion_requested(&current) && !finalizers_pending(&current) {
                self.api.store.delete(&k, result.revision).await?;
                return Ok(success(200));
            }
        }
        let code = if expected.is_some() {
            StatusCode::OK
        } else {
            StatusCode::CREATED
        };
        Ok((code, Json(object(result)?)).into_response())
    }
    async fn delete(&self, options: Value) -> Result<Response> {
        let k = named(self.target)?;
        let current = fetch(self.api, &k, "object not found").await?;
        let obj = object(current.clone())?;
        nodes::admit(self.user, self.target, self.verb, &obj, Some(&obj))?;
        for (field, actual) in [
            ("uid", obj["metadata"]["uid"].as_str().unwrap_or("")),
            (
                "resourceVersion",
                obj["metadata"]["resourceVersion"].as_str().unwrap_or(""),
            ),
        ] {
            if options["preconditions"][field]
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
        if self.target.resource.kind == "Namespace" {
            return delete_namespace(self.api, k, current, obj).await;
        }
        if options["propagationPolicy"]
            .as_str()
            .is_some_and(|p| p != "Background")
            || options["orphanDependents"] == true
        {
            return Err(Failure::new(422,"Invalid","only background deletion is implemented; foreground and orphan propagation are unsupported"));
        }
        self.api.store.delete(&k, current.revision).await?;
        Ok(success(200))
    }
}

fn deletion_requested(obj: &Value) -> bool {
    obj["metadata"]["deletionTimestamp"]
        .as_str()
        .is_some_and(|s| !s.is_empty())
}
fn finalizers_pending(obj: &Value) -> bool {
    obj["metadata"]["finalizers"]
        .as_array()
        .is_some_and(|a| a.iter().any(|v| v.as_str().is_some_and(|s| !s.is_empty())))
}
const NAMESPACE_FINALIZER: &str = "kubernetes";
const PROTECTED_NAMESPACES: &[&str] = &["default", "kube-system", "kube-public", "kube-node-lease"];
/// Namespace deletion is two-phase: mark Terminating with the `kubernetes`
/// finalizer, then remove once the namespace controller clears it.
async fn delete_namespace(
    api: &Api,
    k: StoreKey,
    current: StoredObject,
    mut obj: Value,
) -> Result<Response> {
    let name = obj["metadata"]["name"].as_str().unwrap_or("");
    if PROTECTED_NAMESPACES.contains(&name) {
        return Err(Failure::new(
            403,
            "Forbidden",
            "this namespace cannot be deleted",
        ));
    }
    if deletion_requested(&obj) && !finalizers_pending(&obj) {
        api.store.delete(&k, current.revision).await?;
        return Ok(success(200));
    }
    if deletion_requested(&obj) {
        return Ok((StatusCode::OK, Json(obj)).into_response());
    }
    obj["metadata"]["deletionTimestamp"] = now().into();
    obj["status"]["phase"] = "Terminating".into();
    let mut finalizers: Vec<String> = obj["metadata"]["finalizers"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().filter(|s| !s.is_empty()).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    if !finalizers.iter().any(|f| f == NAMESPACE_FINALIZER) {
        finalizers.push(NAMESPACE_FINALIZER.into());
    }
    obj["metadata"]["finalizers"] = json!(finalizers);
    let result = api.store.update(stored(k, &obj)?, current.revision).await?;
    Ok((StatusCode::OK, Json(object(result)?)).into_response())
}

/// `pods/{name}/binding`: the scheduler's one-shot assignment of a Pod to a
/// Node. The Pod must be unbound, alive, and match any stated preconditions.
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
    fetch(
        api,
        &key(format!("/registry/nodes/{node}"))?,
        "binding target node not found",
    )
    .await?;
    let k = named(target)?;
    let stored_pod = fetch(api, &k, "Pod not found").await?;
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
    Ok(success(201))
}
