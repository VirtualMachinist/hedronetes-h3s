//! API-only workload ownership, rollout and background collection.
mod deployment;
mod endpoints;
mod replicaset;
use crate::Error;
use chrono::{DateTime, Utc};
pub use deployment::{deployment_once, run_deployment_controller};
pub use endpoints::{
    endpoint_gc_once, endpoints_once, run_endpoint_controller, ENDPOINT_CONTROLLER_ID,
};
use k8s_openapi::api::{
    apps::v1::{Deployment, ReplicaSet},
    core::v1::Pod,
};
use kube::{
    api::{DeleteParams, ListParams, PostParams, Preconditions, PropagationPolicy},
    Api, Client, Resource, ResourceExt,
};
pub use replicaset::{replicaset_once, run_replicaset_controller};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use std::{fmt::Debug, time::Duration};
pub const DEPLOYMENT_CONTROLLER_ID: &str = "system:h3s:deployment-controller";
pub const REPLICASET_CONTROLLER_ID: &str = "system:h3s:replicaset-controller";
pub const WORKLOAD_GC_ID: &str = "system:h3s:workload-gc";
const LIMIT: u32 = 4096;
const BURST: usize = 8;
fn now() -> DateTime<Utc> {
    std::time::SystemTime::now().into()
}
fn timestamp() -> String {
    now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
fn values(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}
fn text<'a>(v: &'a Value, k: &str) -> Result<&'a str, Error> {
    v[k].as_str()
        .filter(|s| !s.is_empty())
        .ok_or(Error::Invalid("workload identity is incomplete"))
}
fn identity(v: &Value) -> Result<(&str, &str, &str), Error> {
    Ok((
        text(&v["metadata"], "namespace")?,
        text(&v["metadata"], "name")?,
        text(&v["metadata"], "uid")?,
    ))
}
fn active(p: &Value) -> bool {
    p["metadata"]["deletionTimestamp"].is_null()
        && !matches!(p["status"]["phase"].as_str(), Some("Succeeded" | "Failed"))
}
fn ready(p: &Value) -> bool {
    active(p)
        && values(&p["status"]["conditions"]).any(|c| c["type"] == "Ready" && c["status"] == "True")
}
fn available(p: &Value, seconds: i64) -> bool {
    ready(p)
        && (seconds == 0
            || values(&p["status"]["conditions"])
                .find(|c| c["type"] == "Ready" && c["status"] == "True")
                .and_then(|c| c["lastTransitionTime"].as_str())
                .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
                .is_some_and(|t| now().signed_duration_since(t).num_seconds() >= seconds))
}
fn controller(v: &Value) -> Option<&Value> {
    let mut owners = values(&v["metadata"]["ownerReferences"]).filter(|o| o["controller"] == true);
    let first = owners.next()?;
    if owners.next().is_some() {
        None
    } else {
        Some(first)
    }
}
fn owned(child: &Value, parent: &Value) -> bool {
    controller(child).is_some_and(|o| {
        o["uid"] == parent["metadata"]["uid"]
            && o["name"] == parent["metadata"]["name"]
            && o["kind"] == parent["kind"]
            && o["apiVersion"] == parent["apiVersion"]
    }) && child["metadata"]["namespace"] == parent["metadata"]["namespace"]
}
fn owner(parent: &Value) -> Value {
    json!({"apiVersion":parent["apiVersion"],"kind":parent["kind"],"name":parent["metadata"]["name"],"uid":parent["metadata"]["uid"],"controller":true,"blockOwnerDeletion":true})
}
fn matches(selector: &Value, labels: &Value) -> bool {
    let label_count = selector["matchLabels"].as_object().map_or(0, |m| m.len());
    let expr_count = values(&selector["matchExpressions"]).count();
    label_count + expr_count > 0
        && selector["matchLabels"]
            .as_object()
            .is_none_or(|m| m.iter().all(|(k, v)| labels[k] == *v))
        && values(&selector["matchExpressions"]).all(|r| {
            let Some(key) = r["key"].as_str().filter(|k| !k.is_empty()) else {
                return false;
            };
            let present = labels.get(key).is_some();
            let vals: Vec<_> = values(&r["values"]).collect();
            match r["operator"].as_str() {
                Some("In") => !vals.is_empty() && present && vals.contains(&&labels[key]),
                Some("NotIn") => !vals.is_empty() && (!present || !vals.contains(&&labels[key])),
                Some("Exists") => vals.is_empty() && present,
                Some("DoesNotExist") => vals.is_empty() && !present,
                _ => false,
            }
        })
}
fn replicas(v: &Value) -> Result<i64, Error> {
    v["spec"]["replicas"]
        .as_i64()
        .filter(|n| (0..=i64::from(LIMIT)).contains(n))
        .ok_or(Error::Invalid(
            "replica count exceeds the current bounded controller range",
        ))
}
fn minimum_ready(v: &Value) -> Result<i64, Error> {
    v["spec"]["minReadySeconds"]
        .as_i64()
        .unwrap_or(0)
        .try_into()
        .map(|n: u32| i64::from(n))
        .map_err(|_| Error::Invalid("invalid minReadySeconds"))
}
fn template_metadata(template: &Value) -> Result<Value, Error> {
    if template["metadata"].as_object().is_some_and(|m| {
        m.iter()
            .any(|(k, v)| !v.is_null() && !matches!(k.as_str(), "labels" | "annotations"))
    }) {
        return Err(Error::Invalid("unsupported Pod template metadata"));
    }
    Ok(
        json!({"labels":template["metadata"]["labels"],"annotations":template["metadata"]["annotations"]}),
    )
}
fn condition(
    old: &Value,
    kind: &str,
    status: &str,
    reason: &str,
    message: &str,
    update: bool,
    deployment: bool,
) -> Value {
    let previous = values(&old["conditions"]).find(|c| c["type"] == kind);
    let time = timestamp();
    let transition = previous
        .filter(|c| c["status"] == status)
        .and_then(|c| c["lastTransitionTime"].as_str())
        .unwrap_or(&time);
    let mut c = json!({"type":kind,"status":status,"reason":reason,"message":message,"lastTransitionTime":transition});
    if deployment {
        c["lastUpdateTime"] = previous
            .filter(|c| !update && c["status"] == status && c["reason"] == reason)
            .and_then(|c| c["lastUpdateTime"].as_str())
            .unwrap_or(&time)
            .into();
    }
    c
}
async fn list<K>(api: &Api<K>) -> Result<Vec<Value>, Error>
where
    K: Clone + Debug + DeserializeOwned + Serialize,
{
    let list = api.list(&ListParams::default().limit(LIMIT)).await?;
    if list.items.len() > LIMIT as usize
        || list
            .metadata
            .continue_
            .as_deref()
            .is_some_and(|c| !c.is_empty())
    {
        return Err(Error::Invalid(
            "workload controller requires complete bounded LIST",
        ));
    }
    list.items
        .iter()
        .map(|v| serde_json::to_value(v).map_err(Into::into))
        .collect()
}
async fn live<K>(api: &Api<K>, object: &Value) -> Result<bool, Error>
where
    K: Clone + Debug + DeserializeOwned + Resource<DynamicType = ()>,
{
    let (_, name, uid) = identity(object)?;
    Ok(api.get_opt(name).await?.is_some_and(|p| {
        p.uid().as_deref() == Some(uid)
            && p.meta().deletion_timestamp.is_none()
            && p.meta().generation == object["metadata"]["generation"].as_i64()
    }))
}
async fn status<K>(api: &Api<K>, object: &Value, desired: Value) -> Result<(), Error>
where
    K: Clone + Debug + DeserializeOwned,
{
    if object["status"] == desired {
        return Ok(());
    }
    let mut value = object.clone();
    value["status"] = desired;
    match api
        .replace_status(
            text(&value["metadata"], "name")?,
            &PostParams::default(),
            serde_json::to_vec(&value)?,
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if matches!(e.code, 404 | 409) => Ok(()),
        Err(e) => Err(e.into()),
    }
}
async fn replace<K>(api: &Api<K>, object: &Value) -> Result<(), Error>
where
    K: Clone + Debug + DeserializeOwned + Serialize,
{
    let value: K = serde_json::from_value(object.clone())?;
    api.replace(
        text(&object["metadata"], "name")?,
        &PostParams::default(),
        &value,
    )
    .await?;
    Ok(())
}
async fn delete<K>(api: &Api<K>, object: &Value) -> Result<bool, Error>
where
    K: Clone + Debug + DeserializeOwned,
{
    let dp = DeleteParams {
        propagation_policy: Some(PropagationPolicy::Background),
        preconditions: Some(Preconditions {
            uid: Some(text(&object["metadata"], "uid")?.into()),
            resource_version: Some(text(&object["metadata"], "resourceVersion")?.into()),
        }),
        ..Default::default()
    };
    match api.delete(text(&object["metadata"], "name")?, &dp).await {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(e)) if matches!(e.code, 404 | 409) => Ok(false),
        Err(e) => Err(e.into()),
    }
}
fn failure(error: &Error) -> String {
    match error {
        Error::Api(kube::Error::Api(e)) => {
            format!("workload API operation failed with status {}", e.code)
        }
        Error::Invalid(m) => (*m).into(),
        _ => "workload API operation did not complete".into(),
    }
}

/// Only Pod -> ReplicaSet and ReplicaSet -> Deployment single-controller ownership.
/// Re-read the actual owner; errors never authorize collection.
pub async fn gc_once(client: Client) -> Result<usize, Error> {
    let sets = list(&Api::<ReplicaSet>::all(client.clone())).await?;
    let pods = list(&Api::<Pod>::all(client.clone())).await?;
    let mut deleted = 0;
    for (objects, kind) in [(&sets, "Deployment"), (&pods, "ReplicaSet")] {
        for object in objects {
            // Additional owner references require a general GC graph; preserve them.
            if values(&object["metadata"]["ownerReferences"]).count() != 1 {
                continue;
            }
            let Some(owner) =
                controller(object).filter(|o| o["apiVersion"] == "apps/v1" && o["kind"] == kind)
            else {
                continue;
            };
            let (ns, _, _) = identity(object)?;
            let name = text(owner, "name")?;
            let uid = text(owner, "uid")?;
            let exists = if kind == "Deployment" {
                Api::<Deployment>::namespaced(client.clone(), ns)
                    .get_opt(name)
                    .await?
                    .is_some_and(|p| p.uid().as_deref() == Some(uid))
            } else {
                Api::<ReplicaSet>::namespaced(client.clone(), ns)
                    .get_opt(name)
                    .await?
                    .is_some_and(|p| p.uid().as_deref() == Some(uid))
            };
            if !exists {
                let removed = if kind == "Deployment" {
                    delete(&Api::<ReplicaSet>::namespaced(client.clone(), ns), object).await?
                } else {
                    delete(&Api::<Pod>::namespaced(client.clone(), ns), object).await?
                };
                deleted += usize::from(removed);
            }
        }
    }
    Ok(deleted)
}
pub async fn run_workload_gc(client: Client) -> Result<(), Error> {
    loop {
        match tokio::time::timeout(Duration::from_secs(60), gc_once(client.clone())).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => eprintln!("workload GC: {}", failure(&e)),
            Err(_) => eprintln!("workload GC: cycle deadline exceeded"),
        };
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
