//! Resource strategies for the M1 workload API. Runtime admission is separate.
use super::{resources::Resource, Failure, Result};
use h3s_api::pod_profile::PodRuntimeProfile;
use serde_json::{json, Value};
use std::collections::BTreeSet;

fn invalid(message: &str) -> Failure {
    Failure::new(422, "Invalid", message)
}
fn default(value: &mut Value, key: &str, fallback: Value) {
    if value.get(key).is_none_or(Value::is_null)
        || (fallback.is_string() && value[key].as_str() == Some(""))
    {
        value[key] = fallback;
    }
}
fn one_of(value: &Value, choices: &[&str], field: &str) -> Result<()> {
    if !value.as_str().is_some_and(|v| choices.contains(&v)) {
        return Err(invalid(field));
    }
    Ok(())
}

/// Helm release drivers rebuild ConfigMaps and Secrets without resourceVersion.
pub(crate) fn helm_owned(value: &Value, kind: &str) -> bool {
    if kind == "Secret" && value.get("type").and_then(Value::as_str) == Some("helm.sh/release.v1") {
        return true;
    }
    if value["metadata"]["labels"]["owner"].as_str() == Some("helm") {
        return true;
    }
    if kind == "Secret" {
        if let Some(name) = value["metadata"]["name"].as_str() {
            if name.starts_with("sh.helm.release.v1.") {
                return true;
            }
        }
    }
    false
}

/// Copy `key` from `from`, leaving it absent rather than null when unset so
/// the once-normalized object stays canonical without another round-trip.
fn copy(value: &mut Value, key: &str, from: &Value) {
    match from.get(key).filter(|v| !v.is_null()) {
        Some(v) => value[key] = v.clone(),
        None => {
            if let Some(object) = value.as_object_mut() {
                object.remove(key);
            }
        }
    }
}
/// `value` is already normalized by the write pipeline; every default and
/// copy below keeps it typed so no further k8s-openapi round-trip is needed.
pub(crate) fn prepare(
    resource: Resource,
    mut value: Value,
    old: Option<&Value>,
    status: bool,
) -> Result<Value> {
    if status {
        let old = old.expect("status updates require an existing object");
        let rv = value["metadata"]["resourceVersion"].clone();
        // Node status writers (including Flannel) update labels/annotations.
        // Node admission still enforces ownership and administrative labels.
        // Other status resources retain the existing frozen metadata boundary.
        copy(&mut value, "spec", old);
        if resource.kind != "Node" {
            value["metadata"] = old["metadata"].clone();
        }
        value["metadata"]["resourceVersion"] = rv;
    } else if resource.has_status() {
        value["status"] = old
            .and_then(|v| v.get("status").filter(|s| !s.is_null()).cloned())
            .unwrap_or_else(|| match resource.kind {
                "Namespace" => json!({"phase":"Active"}),
                "Pod" => json!({"phase":"Pending"}),
                _ => json!({}),
            });
    }
    if status {
        if resource.kind == "Pod" && !value["status"]["phase"].is_null() {
            one_of(
                &value["status"]["phase"],
                &["Pending", "Running", "Succeeded", "Failed", "Unknown"],
                "invalid Pod status.phase",
            )?;
        }
        return Ok(value);
    }
    match resource.kind {
        "Pod" => pod(&mut value["spec"])?,
        "Deployment" | "ReplicaSet" => {
            let spec = value
                .get_mut("spec")
                .filter(|v| v.is_object())
                .ok_or_else(|| invalid("spec is required"))?;
            default(spec, "replicas", json!(1));
            if spec["replicas"].as_i64().is_none_or(|v| v < 0) {
                return Err(invalid("replicas must be nonnegative"));
            }
            let template = spec
                .get_mut("template")
                .filter(|v| v.is_object())
                .ok_or_else(|| invalid("template is required"))?;
            pod(&mut template["spec"])?;
            if template["spec"]["restartPolicy"] != "Always" {
                return Err(invalid("workload template restartPolicy must be Always"));
            }
            // A template the node cannot execute is refused here, never
            // persisted to fail one replica at a time.
            PodRuntimeProfile.check(&template["spec"]).map_err(|e| {
                invalid(&format!(
                    "template cannot run under the {} runtime profile: {e}",
                    PodRuntimeProfile::NAME
                ))
            })?;
            selector_matches(&spec["selector"], &spec["template"]["metadata"]["labels"])?;
            if resource.kind == "Deployment" {
                default(spec, "revisionHistoryLimit", json!(10));
                default(spec, "progressDeadlineSeconds", json!(600));
                default(spec, "strategy", json!({}));
                default(&mut spec["strategy"], "type", json!("RollingUpdate"));
                one_of(
                    &spec["strategy"]["type"],
                    &["RollingUpdate", "Recreate"],
                    "invalid deployment strategy",
                )?;
                if spec["strategy"]["type"] == "RollingUpdate" {
                    default(&mut spec["strategy"], "rollingUpdate", json!({}));
                    default(
                        &mut spec["strategy"]["rollingUpdate"],
                        "maxSurge",
                        json!("25%"),
                    );
                    default(
                        &mut spec["strategy"]["rollingUpdate"],
                        "maxUnavailable",
                        json!("25%"),
                    );
                } else if spec["strategy"]
                    .get("rollingUpdate")
                    .is_some_and(|v| !v.is_null())
                {
                    return Err(invalid("Recreate cannot set rollingUpdate"));
                }
            }
        }
        "Service" => service(&mut value["spec"])?,
        "Node" => {
            default(&mut value, "spec", json!({}));
            if let Some(cidr) = value["spec"]["podCIDR"].as_str().filter(|s| !s.is_empty()) {
                if cidr.parse::<ipnet::IpNet>().is_err() {
                    return Err(invalid("invalid node podCIDR"));
                }
            }
        }
        "EndpointSlice" => endpoints(&mut value)?,
        "Lease"
            if value["spec"]["leaseDurationSeconds"]
                .as_i64()
                .is_some_and(|v| v <= 0) =>
        {
            return Err(invalid("lease duration must be positive"));
        }
        _ => {}
    }
    if let Some(old) = old {
        if matches!(resource.kind, "Deployment" | "ReplicaSet")
            && value["spec"]["selector"] != old["spec"]["selector"]
        {
            return Err(invalid("workload selector is immutable"));
        }
        if resource.kind == "Pod" {
            // Scheduling uses the binding subresource. Images may be updated;
            // other pod-spec transitions will get explicit admission strategies.
            let mut allowed = old["spec"].clone();
            for field in ["containers", "initContainers"] {
                if let (Some(before), Some(after)) = (
                    allowed.get_mut(field).and_then(Value::as_array_mut),
                    value["spec"][field].as_array(),
                ) {
                    if before.len() == after.len() {
                        for (a, b) in before.iter_mut().zip(after) {
                            a["image"] = b["image"].clone();
                        }
                    }
                }
            }
            if allowed != value["spec"] {
                return Err(invalid(
                    "this Pod update changes immutable spec fields; scheduling requires binding",
                ));
            }
        }
        if resource.kind == "EndpointSlice" && value["addressType"] != old["addressType"] {
            return Err(invalid("addressType is immutable"));
        }
        if matches!(resource.kind, "Secret" | "ConfigMap")
            && old["immutable"] == true
            && (value["immutable"] != true
                || value["data"] != old["data"]
                || value["binaryData"] != old["binaryData"])
        {
            return Err(invalid("immutable object data cannot change"));
        }
        if resource.kind == "Secret" && value["type"] != old["type"] {
            return Err(invalid("Secret type is immutable"));
        }
        value["metadata"]["generation"] = old["metadata"]["generation"].clone();
    } else {
        value["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("generation");
    }
    if resource.has_generation() {
        let changed = old.is_some_and(|old| {
            old["spec"] != value["spec"]
                || (resource.kind == "Deployment"
                    && old["metadata"]["annotations"] != value["metadata"]["annotations"])
        });
        let generation = old
            .and_then(|v| v["metadata"]["generation"].as_i64())
            .unwrap_or(1);
        value["metadata"]["generation"] = json!(generation
            .checked_add(i64::from(changed))
            .ok_or_else(|| invalid("generation overflow"))?);
    }
    Ok(value)
}

fn pod(spec: &mut Value) -> Result<()> {
    if !spec.is_object() {
        return Err(invalid("Pod spec is required"));
    }
    if spec["containers"].as_array().is_none_or(Vec::is_empty) {
        return Err(invalid("at least one container is required"));
    }
    default(spec, "restartPolicy", json!("Always"));
    default(spec, "dnsPolicy", json!("ClusterFirst"));
    default(spec, "schedulerName", json!("default-scheduler"));
    if let Some(alias) = spec["serviceAccount"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
    {
        if spec["serviceAccountName"]
            .as_str()
            .is_some_and(|name| !name.is_empty() && name != alias)
        {
            return Err(invalid("serviceAccount and serviceAccountName must agree"));
        }
        spec["serviceAccountName"] = alias.into();
    }
    default(spec, "serviceAccountName", json!("default"));
    spec["serviceAccount"] = spec["serviceAccountName"].clone();
    default(spec, "terminationGracePeriodSeconds", json!(30));
    PodRuntimeProfile.defaults(spec);
    one_of(
        &spec["restartPolicy"],
        &["Always", "OnFailure", "Never"],
        "invalid restartPolicy",
    )?;
    one_of(
        &spec["dnsPolicy"],
        &["ClusterFirst", "Default", "ClusterFirstWithHostNet", "None"],
        "invalid dnsPolicy",
    )?;
    if spec["terminationGracePeriodSeconds"]
        .as_i64()
        .is_none_or(|v| v < 0)
    {
        return Err(invalid("termination grace must be nonnegative"));
    }
    let mut names = BTreeSet::new();
    for field in ["containers", "initContainers"] {
        if let Some(containers) = spec.get_mut(field).and_then(Value::as_array_mut) {
            for container in containers {
                let name = container["name"].as_str().unwrap_or("");
                if !super::resources::valid_label_name(name) || !names.insert(name.to_owned()) {
                    return Err(invalid("container names must be unique DNS labels"));
                }
                let image = container["image"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| invalid("container image is required"))?;
                let latest = !image.contains('@')
                    && (image.ends_with(":latest")
                        || !image.rsplit('/').next().unwrap().contains(':'));
                default(
                    container,
                    "imagePullPolicy",
                    json!(if latest { "Always" } else { "IfNotPresent" }),
                );
                default(
                    container,
                    "terminationMessagePath",
                    json!("/dev/termination-log"),
                );
                default(container, "terminationMessagePolicy", json!("File"));
                one_of(
                    &container["imagePullPolicy"],
                    &["Always", "IfNotPresent", "Never"],
                    "invalid imagePullPolicy",
                )?;
                if let Some(ports) = container.get_mut("ports").and_then(Value::as_array_mut) {
                    for port in ports {
                        valid_port(&port["containerPort"])?;
                        default(port, "protocol", json!("TCP"));
                        one_of(
                            &port["protocol"],
                            &["TCP", "UDP", "SCTP"],
                            "invalid port protocol",
                        )?;
                    }
                }
            }
        }
    }
    Ok(())
}

fn selector_matches(selector: &Value, labels: &Value) -> Result<()> {
    let mut count = 0;
    if let Some(wanted) = selector["matchLabels"].as_object() {
        count += wanted.len();
        for (key, value) in wanted {
            if labels[key] != *value {
                return Err(invalid("selector does not match template labels"));
            }
        }
    }
    if let Some(expressions) = selector["matchExpressions"].as_array() {
        count += expressions.len();
        for expr in expressions {
            let key = expr["key"].as_str().unwrap_or("");
            if key.is_empty() {
                return Err(invalid("selector expression key is required"));
            }
            let values = expr["values"].as_array().cloned().unwrap_or_default();
            let present = labels.get(key).is_some();
            let matches = match expr["operator"].as_str() {
                Some("In") if !values.is_empty() => present && values.contains(&labels[key]),
                Some("NotIn") if !values.is_empty() => !present || !values.contains(&labels[key]),
                Some("Exists") if values.is_empty() => present,
                Some("DoesNotExist") if values.is_empty() => !present,
                _ => return Err(invalid("invalid selector operator or values")),
            };
            if !matches {
                return Err(invalid("selector does not match template labels"));
            }
        }
    }
    if count == 0 {
        return Err(invalid("workload selector must not be empty"));
    }
    Ok(())
}
fn valid_port(value: &Value) -> Result<()> {
    if value.as_i64().is_none_or(|n| !(1..=65535).contains(&n)) {
        return Err(invalid("port must be in 1..65535"));
    }
    Ok(())
}
fn service(spec: &mut Value) -> Result<()> {
    if !spec.is_object() {
        return Err(invalid("Service spec is required"));
    }
    default(spec, "type", json!("ClusterIP"));
    one_of(
        &spec["type"],
        &["ClusterIP", "ExternalName"],
        "currently supported Service types are ClusterIP and ExternalName",
    )?;
    if spec["type"] == "ExternalName" {
        if !spec["externalName"]
            .as_str()
            .is_some_and(super::resources::valid_name)
        {
            return Err(invalid("externalName must be a DNS name"));
        }
        return Ok(());
    }
    default(spec, "sessionAffinity", json!("None"));
    one_of(
        &spec["sessionAffinity"],
        &["None"],
        "ClientIP affinity is not implemented by the native Service proxy",
    )?;
    if spec["externalIPs"]
        .as_array()
        .is_some_and(|v| !v.is_empty())
        || !spec["trafficDistribution"].is_null()
    {
        return Err(invalid(
            "externalIPs and trafficDistribution are not implemented by the native Service proxy",
        ));
    }
    default(spec, "internalTrafficPolicy", json!("Cluster"));
    one_of(
        &spec["internalTrafficPolicy"],
        &["Cluster", "Local"],
        "invalid internalTrafficPolicy",
    )?;
    let ports = spec
        .get_mut("ports")
        .and_then(Value::as_array_mut)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| invalid("Service ports are required"))?;
    let multiple = ports.len() > 1;
    let mut names = BTreeSet::new();
    for port in ports {
        valid_port(&port["port"])?;
        let target = port["port"].clone();
        if port["targetPort"] == 0 {
            port["targetPort"] = Value::Null;
        }
        default(port, "targetPort", target);
        if port["targetPort"].is_number() {
            valid_port(&port["targetPort"])?;
        } else if !port["targetPort"]
            .as_str()
            .is_some_and(super::resources::valid_label_name)
        {
            return Err(invalid("invalid named targetPort"));
        }
        default(port, "protocol", json!("TCP"));
        one_of(
            &port["protocol"],
            &["TCP", "UDP"],
            "native Service forwarding supports TCP and UDP",
        )?;
        let name = port["name"].as_str().unwrap_or("");
        if (multiple && name.is_empty())
            || (!name.is_empty() && !super::resources::valid_label_name(name))
            || !names.insert(name.to_owned())
        {
            return Err(invalid("Service port names must be valid and unique"));
        }
    }
    Ok(())
}
fn endpoints(value: &mut Value) -> Result<()> {
    one_of(
        &value["addressType"],
        &["IPv4", "IPv6", "FQDN"],
        "invalid EndpointSlice addressType",
    )?;
    let address_type = value["addressType"].as_str().unwrap();
    for endpoint in value["endpoints"].as_array().into_iter().flatten() {
        let addresses = endpoint["addresses"]
            .as_array()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| invalid("endpoint addresses required"))?;
        for address in addresses {
            let address = address.as_str().unwrap_or("");
            let valid = match address_type {
                "IPv4" => address.parse::<std::net::Ipv4Addr>().is_ok(),
                "IPv6" => address.parse::<std::net::Ipv6Addr>().is_ok(),
                _ => super::resources::valid_name(address),
            };
            if !valid {
                return Err(invalid("endpoint address does not match addressType"));
            }
        }
    }
    if let Some(ports) = value.get_mut("ports").and_then(Value::as_array_mut) {
        for port in ports {
            if !port["port"].is_null() {
                valid_port(&port["port"])?;
            }
            default(port, "protocol", json!("TCP"));
            one_of(
                &port["protocol"],
                &["TCP", "UDP", "SCTP"],
                "invalid endpoint protocol",
            )?;
        }
    }
    Ok(())
}
