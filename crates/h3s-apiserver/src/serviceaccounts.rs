//! Pod ServiceAccount admission: the account must exist and permit what the
//! Pod references. No token is projected: until TokenRequest and bearer
//! authentication exist the API would only refuse it, and the runtime profile
//! keeps `automountServiceAccountToken` false to match.
use crate::{key, object, resources::RESOURCES, Failure, Result};
use h3s_storage::Storage;
use serde_json::Value;
use std::{collections::BTreeSet, sync::Arc};

fn denied(message: &str) -> Failure {
    Failure::new(403, "Forbidden", message)
}
fn items(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}

pub async fn admit(store: &Arc<dyn Storage>, namespace: &str, pod: &mut Value) -> Result<()> {
    if pod["metadata"]["annotations"]
        .get("kubernetes.io/config.mirror")
        .is_some()
    {
        return Err(denied("mirror Pod admission is not implemented"));
    }
    let spec = &mut pod["spec"];
    let name = spec["serviceAccountName"].as_str().unwrap_or("default");
    let resource = RESOURCES
        .iter()
        .find(|r| r.kind == "ServiceAccount")
        .expect("ServiceAccount resource");
    if !resource.valid_name(name) {
        return Err(Failure::new(422, "Invalid", "invalid serviceAccountName"));
    }
    let account = store
        .get(&key(format!(
            "/registry/serviceaccounts/{namespace}/{name}"
        ))?)
        .await?
        .ok_or_else(|| denied("Pod service account does not exist in its namespace"))?;
    let account = object(account)?;
    if !account["metadata"]["deletionTimestamp"].is_null() {
        return Err(denied("Pod service account is terminating"));
    }
    if items(&spec["imagePullSecrets"]).next().is_none() && !account["imagePullSecrets"].is_null() {
        spec["imagePullSecrets"] = account["imagePullSecrets"].clone();
    }
    let enforce = account["metadata"]["annotations"]["kubernetes.io/enforce-mountable-secrets"]
        .as_str()
        .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("t"));
    if enforce {
        validate_secret_references(spec, &account)?;
    }
    Ok(())
}
fn validate_secret_references(spec: &Value, account: &Value) -> Result<()> {
    let allowed: BTreeSet<_> = items(&account["secrets"])
        .filter_map(|v| v["name"].as_str())
        .collect();
    let pulls: BTreeSet<_> = items(&account["imagePullSecrets"])
        .filter_map(|v| v["name"].as_str())
        .collect();
    let mut refs = Vec::new();
    for volume in items(&spec["volumes"]) {
        if let Some(name) = volume["secret"]["secretName"].as_str() {
            refs.push(name);
        }
        for source in items(&volume["projected"]["sources"]) {
            if let Some(name) = source["secret"]["name"].as_str() {
                refs.push(name);
            }
        }
    }
    for field in ["containers", "initContainers", "ephemeralContainers"] {
        for container in items(&spec[field]) {
            for env in items(&container["env"]) {
                if let Some(name) = env["valueFrom"]["secretKeyRef"]["name"].as_str() {
                    refs.push(name);
                }
            }
            for env in items(&container["envFrom"]) {
                if let Some(name) = env["secretRef"]["name"].as_str() {
                    refs.push(name);
                }
            }
        }
    }
    if refs.iter().any(|name| !allowed.contains(name)) {
        return Err(denied(
            "Pod references a secret not allowed by its service account",
        ));
    }
    if items(&spec["imagePullSecrets"])
        .any(|v| v["name"].as_str().is_none_or(|n| !pulls.contains(n)))
    {
        return Err(denied(
            "Pod references an image pull secret not allowed by its service account",
        ));
    }
    Ok(())
}
