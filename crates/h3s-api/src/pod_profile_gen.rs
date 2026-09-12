//! Generated `PodRuntimeProfile::check` from hedron-ncl overlay
//! `k8s-1.34-h3s-0.9.1` (hedronetes-h3s @ 38a2f1b). Plain Rust — no nickel-lang-core.

use serde_json::Value;
use std::collections::BTreeSet;

use crate::pod_profile::{
    array, fields, mode, paths, probe, relative, safe_component, text, PodRuntimeProfile, Result,
    Unsupported,
};

pub const CONTRACT_SET: &str = "k8s-1.34-h3s-0.9.1";

const SPEC_FIELDS: &[&str] = &[
    "nodeName", "containers", "volumes", "securityContext", "restartPolicy",
    "terminationGracePeriodSeconds", "dnsPolicy", "dnsConfig", "hostname",
    "serviceAccount", "serviceAccountName", "automountServiceAccountToken",
    "enableServiceLinks", "schedulerName", "nodeSelector", "tolerations", "affinity",
    "priority", "priorityClassName", "preemptionPolicy", "imagePullSecrets",
];
const CONTAINER_FIELDS: &[&str] = &[
    "name", "image", "imagePullPolicy", "command", "args", "workingDir", "env", "envFrom",
    "ports", "resources", "securityContext", "volumeMounts", "readinessProbe",
    "terminationMessagePath", "terminationMessagePolicy",
];

pub(crate) fn check(profile: &PodRuntimeProfile, spec: &Value) -> Result<()> {
        if !spec.is_object() {
            return Err(Unsupported("Pod spec is required"));
        }
        fields(spec, SPEC_FIELDS)?;
        if spec["restartPolicy"]
            .as_str()
            .is_some_and(|s| !matches!(s, "Always" | "OnFailure" | "Never"))
        {
            return Err(Unsupported("invalid restart policy"));
        }
        if spec["terminationGracePeriodSeconds"]
            .as_i64()
            .is_some_and(|v| !(0..=30).contains(&v))
        {
            return Err(Unsupported(
                "termination grace outside supported 0-30 seconds",
            ));
        }
        if spec["automountServiceAccountToken"] != false {
            return Err(Unsupported(
                "service-account token projection is not implemented",
            ));
        }
        if spec["enableServiceLinks"] != false {
            return Err(Unsupported(
                "service environment injection is not implemented",
            ));
        }
        if spec["imagePullSecrets"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
        {
            return Err(Unsupported(
                "private image authentication is not implemented",
            ));
        }
        if !matches!(
            spec["dnsPolicy"].as_str(),
            None | Some("Default" | "None" | "ClusterFirst")
        ) {
            return Err(Unsupported("unsupported DNS policy"));
        }
        fields(
            &spec["securityContext"],
            &["runAsUser", "runAsGroup", "runAsNonRoot", "seccompProfile"],
        )?;
        if spec["hostname"]
            .as_str()
            .is_some_and(|v| !safe_component(v) || v.len() > 63)
        {
            return Err(Unsupported("invalid Pod hostname"));
        }
        let volumes = volumes(spec)?;
        let containers = spec["containers"]
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= 32)
            .ok_or(Unsupported("invalid container count"))?;
        let mut names = BTreeSet::new();
        for c in containers {
            let name = text(c, "name")?;
            if !safe_component(name) || !names.insert(name) {
                return Err(Unsupported("invalid or duplicate container name"));
            }
            container_check(profile, spec, c, &volumes)?;
        }
        Ok(())
    }

fn container_check(profile: &PodRuntimeProfile, spec: &Value, c: &Value, volumes: &[&str]) -> Result<()> {
        fields(c, CONTAINER_FIELDS)?;
        text(c, "image")?;
        if c["terminationMessagePolicy"]
            .as_str()
            .is_some_and(|s| s != "File")
        {
            return Err(Unsupported("termination message policy is unsupported"));
        }
        for port in array(&c["ports"])? {
            fields(port, &["name", "containerPort", "protocol"])?;
        }
        if c["workingDir"]
            .as_str()
            .is_some_and(|s| !s.is_empty() && (!s.starts_with('/') || s.contains('\0')))
        {
            return Err(Unsupported("invalid working directory"));
        }
        fields(
            &c["securityContext"],
            &[
                "runAsUser",
                "runAsGroup",
                "runAsNonRoot",
                "allowPrivilegeEscalation",
                "readOnlyRootFilesystem",
                "capabilities",
                "seccompProfile",
                "procMount",
            ],
        )?;
        if c["securityContext"]["procMount"]
            .as_str()
            .is_some_and(|s| s != "Default")
        {
            return Err(Unsupported("unmasked proc mounts are unsupported"));
        }
        profile.container(spec, c)?;
        fields(&c["resources"], &["requests", "limits"])?;
        for k in ["requests", "limits"] {
            fields(&c["resources"][k], &["cpu", "memory"])?;
        }
        for e in array(&c["env"])? {
            fields(e, &["name", "value", "valueFrom"])?;
            let source = &e["valueFrom"];
            fields(source, &["configMapKeyRef", "secretKeyRef", "fieldRef"])?;
            if !source.is_null()
                && ["configMapKeyRef", "secretKeyRef", "fieldRef"]
                    .iter()
                    .filter(|k| !source[**k].is_null())
                    .count()
                    != 1
            {
                return Err(Unsupported("invalid environment value source"));
            }
            if !source["fieldRef"].is_null() {
                fields(&source["fieldRef"], &["fieldPath", "apiVersion"])?;
                if !matches!(
                    source["fieldRef"]["fieldPath"].as_str(),
                    Some("metadata.name" | "metadata.namespace" | "metadata.uid" | "spec.nodeName")
                ) {
                    return Err(Unsupported("unsupported downward API field"));
                }
            }
        }
        for source in array(&c["envFrom"])? {
            fields(source, &["prefix", "configMapRef", "secretRef"])?;
        }
        let mut destinations = BTreeSet::new();
        for m in array(&c["volumeMounts"])? {
            fields(m, &["name", "mountPath", "readOnly"])?;
            if !volumes.contains(&text(m, "name")?) {
                return Err(Unsupported("mount references an undefined volume"));
            }
            let path = text(m, "mountPath")?;
            if !path.starts_with('/')
                || !relative(&path[1..])
                || ["/proc", "/sys", "/dev"]
                    .iter()
                    .any(|p| path == *p || path.starts_with(&format!("{p}/")))
                || !destinations.insert(path)
                || (!m["readOnly"].is_null() && !m["readOnly"].is_boolean())
            {
                return Err(Unsupported("invalid container mount path or options"));
            }
        }
        // Nested mount destinations obscure a portion of a projected volume.
        paths(destinations.iter().map(|p| &p[1..]))?;
        if !c["readinessProbe"].is_null() {
            probe(&c["readinessProbe"])?;
        }
        Ok(())
    }

fn volumes(spec: &Value) -> Result<Vec<&str>> {
    let volumes = array(&spec["volumes"])?;
    if volumes.len() > 32 {
        return Err(Unsupported("too many Pod volumes"));
    }
    let mut names = Vec::new();
    for v in volumes {
        if fields(v, &["name", "configMap", "secret"]).is_err() {
            return Err(Unsupported(
                "only ConfigMap and Secret volume sources are implemented",
            ));
        }
        let name = text(v, "name")?;
        if !safe_component(name) || name.starts_with('.') || names.contains(&name) {
            return Err(Unsupported("invalid or duplicate volume name"));
        }
        let (source, field) = match (v["configMap"].is_null(), v["secret"].is_null()) {
            (false, true) => (&v["configMap"], "name"),
            (true, false) => (&v["secret"], "secretName"),
            _ => {
                return Err(Unsupported(
                    "exactly one ConfigMap or Secret volume source is required",
                ))
            }
        };
        fields(source, &[field, "items", "defaultMode", "optional"])?;
        if !safe_component(text(source, field)?)
            || (!source["optional"].is_null() && !source["optional"].is_boolean())
        {
            return Err(Unsupported("invalid volume object reference"));
        }
        let default = mode(&source["defaultMode"], 0o644)?;
        let items = array(&source["items"])?;
        if items.len() > 1024 {
            return Err(Unsupported("too many projected files"));
        }
        let mut item_paths = vec![];
        for item in items {
            fields(item, &["key", "path", "mode"])?;
            text(item, "key")?;
            item_paths.push(text(item, "path")?);
            mode(&item["mode"], default)?;
        }
        paths(item_paths.into_iter())?;
        names.push(name);
    }
    Ok(names)
}
