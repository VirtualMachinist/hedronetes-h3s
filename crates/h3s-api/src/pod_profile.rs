//! The one Pod subset the native runtime executes. The API server defaults
//! and admits Pods against it and refuses workload templates that leave it;
//! the kubelet validates with it and translates to CRI from it. Nothing else
//! may name what a Pod can or cannot do at runtime.
use serde_json::{json, Value};
use std::collections::BTreeSet;

/// A Pod field or value the runtime does not execute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unsupported(pub &'static str);
impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for Unsupported {}
pub type Result<T> = std::result::Result<T, Unsupported>;

/// The `restricted-v1` profile: one non-root container set, ConfigMap and
/// Secret volumes, readiness probes, and nothing the node cannot honour yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PodRuntimeProfile;

/// The identity a container runs with. CRI translation uses exactly this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContainerExecution {
    pub run_as_user: i64,
    pub run_as_group: i64,
    pub read_only_root_filesystem: bool,
}

const SPEC_FIELDS: &[&str] = &[
    "nodeName",
    "containers",
    "volumes",
    "securityContext",
    "restartPolicy",
    "terminationGracePeriodSeconds",
    "dnsPolicy",
    "dnsConfig",
    "hostname",
    "serviceAccount",
    "serviceAccountName",
    "automountServiceAccountToken",
    "enableServiceLinks",
    "schedulerName",
    "nodeSelector",
    "tolerations",
    "affinity",
    "priority",
    "priorityClassName",
    "preemptionPolicy",
    "imagePullSecrets",
];
const CONTAINER_FIELDS: &[&str] = &[
    "name",
    "image",
    "imagePullPolicy",
    "command",
    "args",
    "workingDir",
    "env",
    "envFrom",
    "ports",
    "resources",
    "securityContext",
    "volumeMounts",
    "readinessProbe",
    "terminationMessagePath",
    "terminationMessagePolicy",
];
const MAX_UID: i64 = u32::MAX as i64;

impl PodRuntimeProfile {
    pub const NAME: &'static str = "restricted-v1";

    /// API-side defaults that match what the node executes. Until
    /// TokenRequest and bearer authentication exist, a projected token would
    /// only be refused with 401, so neither it nor Service links are enabled.
    pub fn defaults(&self, spec: &mut Value) {
        if let Some(spec) = spec.as_object_mut() {
            for field in ["automountServiceAccountToken", "enableServiceLinks"] {
                if !spec.get(field).is_some_and(Value::is_boolean) {
                    spec.insert(field.into(), json!(false));
                }
            }
        }
    }

    /// Whether the runtime can execute this Pod spec exactly as written.
    pub fn check(&self, spec: &Value) -> Result<()> {
        crate::pod_profile_gen::check(self, spec)
    }

    /// The identity a container executes with, after every security check
    /// the runtime relies on. Validation and CRI translation share this.
    pub fn container(&self, spec: &Value, c: &Value) -> Result<ContainerExecution> {
        let sc = &c["securityContext"];
        let ps = &spec["securityContext"];
        let uid = sc["runAsUser"].as_i64().or(ps["runAsUser"].as_i64());
        let Some(uid) = uid.filter(|u| (1..=MAX_UID).contains(u)) else {
            return Err(Unsupported("runtime requires an explicit non-root UID"));
        };
        let gid = sc["runAsGroup"].as_i64().or(ps["runAsGroup"].as_i64());
        if gid.is_some_and(|g| !(1..=MAX_UID).contains(&g)) {
            return Err(Unsupported("runtime requires a non-root GID"));
        }
        if sc["allowPrivilegeEscalation"] != false {
            return Err(Unsupported(
                "runtime requires allowPrivilegeEscalation=false",
            ));
        }
        fields(&sc["capabilities"], &["add", "drop"])?;
        if sc["capabilities"]["add"]
            .as_array()
            .is_some_and(|a| !a.is_empty())
            || !sc["capabilities"]["drop"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v == "ALL"))
        {
            return Err(Unsupported("runtime requires all capabilities dropped"));
        }
        let seccomp = if sc["seccompProfile"].is_null() {
            &ps["seccompProfile"]
        } else {
            &sc["seccompProfile"]
        };
        if seccomp["type"] != "RuntimeDefault" || fields(seccomp, &["type"]).is_err() {
            return Err(Unsupported("runtime-default seccomp is required"));
        }
        if !sc["readOnlyRootFilesystem"].is_null() && !sc["readOnlyRootFilesystem"].is_boolean() {
            return Err(Unsupported("readOnlyRootFilesystem must be a boolean"));
        }
        Ok(ContainerExecution {
            run_as_user: uid,
            // youki/CRI treat a missing GID as 0; that fails OCI create for a
            // non-root UID, so the UID doubles as the GID.
            run_as_group: gid.unwrap_or(uid),
            read_only_root_filesystem: sc["readOnlyRootFilesystem"].as_bool().unwrap_or(false),
        })
    }

}

/// ConfigMap and Secret volumes only; returns the declared volume names.
/// Readiness only: exec, HTTP without a host, or a TCP port.
pub fn probe(v: &Value) -> Result<()> {
    for field in [
        "initialDelaySeconds",
        "periodSeconds",
        "timeoutSeconds",
        "successThreshold",
        "failureThreshold",
    ] {
        if !v[field].is_null()
            && !v[field]
                .as_i64()
                .is_some_and(|n| n >= if field == "initialDelaySeconds" { 0 } else { 1 })
        {
            return Err(Unsupported("invalid readiness timing or threshold"));
        }
    }
    fields(
        v,
        &[
            "exec",
            "httpGet",
            "tcpSocket",
            "initialDelaySeconds",
            "periodSeconds",
            "timeoutSeconds",
            "successThreshold",
            "failureThreshold",
        ],
    )?;
    if ["exec", "httpGet", "tcpSocket"]
        .iter()
        .filter(|k| !v[**k].is_null())
        .count()
        != 1
    {
        return Err(Unsupported("invalid readiness probe"));
    }
    fields(&v["exec"], &["command"])?;
    fields(&v["httpGet"], &["path", "port", "scheme"])?;
    fields(&v["tcpSocket"], &["port"])?;
    if v["httpGet"]["scheme"].as_str().is_some_and(|s| s != "HTTP") {
        return Err(Unsupported("only HTTP readiness is supported"));
    }
    if v["httpGet"]["path"]
        .as_str()
        .is_some_and(|s| !s.starts_with('/') || s.starts_with("//"))
    {
        return Err(Unsupported("invalid readiness path"));
    }
    Ok(())
}

/// A required, non-empty string field.
pub fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(Unsupported("required Pod field missing"))
}
/// Safe as a single path component or identity label.
pub fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && s != "."
        && s != ".."
}
/// An object may only carry the listed members (null members are ignored).
pub fn fields(v: &Value, allowed: &[&str]) -> Result<()> {
    if v.as_object().is_some_and(|m| {
        m.iter()
            .any(|(k, v)| !v.is_null() && !allowed.contains(&k.as_str()))
    }) {
        return Err(Unsupported("Pod contains an unsupported execution field"));
    }
    Ok(())
}
pub(crate) fn array(v: &Value) -> Result<&[Value]> {
    if v.is_null() {
        Ok(&[])
    } else {
        v.as_array()
            .map(Vec::as_slice)
            .ok_or(Unsupported("Pod list field must be an array"))
    }
}
pub(crate) fn mode(v: &Value, default: u32) -> Result<u32> {
    if v.is_null() {
        Ok(default)
    } else {
        v.as_u64()
            .filter(|n| *n <= 0o777)
            .map(|n| n as u32)
            .ok_or(Unsupported("volume file mode must be within 0000-0777"))
    }
}
pub(crate) fn relative(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && !path.contains('\0')
        && path.split('/').count() <= 32
        && path
            .split('/')
            .all(|s| !s.is_empty() && s.len() <= 255 && s != "." && s != "..")
        && !path.starts_with("..")
        && !path.starts_with('/')
}
pub(crate) fn paths<'a>(paths: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = BTreeSet::new();
    for path in paths {
        if !relative(path) || !seen.insert(path) {
            return Err(Unsupported("invalid or duplicate projected file path"));
        }
    }
    for path in &seen {
        let mut prefix = *path;
        while let Some((parent, _)) = prefix.rsplit_once('/') {
            if seen.contains(parent) {
                return Err(Unsupported("projected file conflicts with a directory"));
            }
            prefix = parent;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec() -> Value {
        json!({"nodeName":"worker","automountServiceAccountToken":false,"enableServiceLinks":false,"dnsPolicy":"Default",
            "volumes":[
                {"name":"config","configMap":{"name":"settings","defaultMode":292,"items":[{"key":"mode","path":"nested/mode","mode":256}]}},
                {"name":"secret","secret":{"secretName":"credentials"}}
            ],
            "containers":[{"name":"web","image":"busybox:1.37.0",
                "volumeMounts":[{"name":"config","mountPath":"/etc/project/config"},{"name":"secret","mountPath":"/etc/project/secret","readOnly":false}],
                "securityContext":{"runAsUser":65534,"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"seccompProfile":{"type":"RuntimeDefault"}}}]})
    }
    fn set(spec: &mut Value, pointer: &str, value: Value) {
        let (parent, key) = pointer.rsplit_once('/').unwrap();
        spec.pointer_mut(parent)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(key.into(), value);
    }
    #[test]
    fn defaults_disable_token_projection_and_service_links_unless_stated() {
        let mut spec = json!({"containers":[]});
        PodRuntimeProfile.defaults(&mut spec);
        assert_eq!(spec["automountServiceAccountToken"], false);
        assert_eq!(spec["enableServiceLinks"], false);
        let mut stated = json!({"automountServiceAccountToken":true,"enableServiceLinks":null});
        PodRuntimeProfile.defaults(&mut stated);
        assert_eq!(stated["automountServiceAccountToken"], true);
        assert_eq!(stated["enableServiceLinks"], false);
        let mut scalar = json!("not a spec");
        PodRuntimeProfile.defaults(&mut scalar);
        assert_eq!(scalar, "not a spec");
    }
    #[test]
    fn executable_spec_passes_and_resolves_container_identity() {
        let spec = spec();
        PodRuntimeProfile.check(&spec).unwrap();
        let identity = PodRuntimeProfile
            .container(&spec, &spec["containers"][0])
            .unwrap();
        assert_eq!(identity.run_as_user, 65534);
        assert_eq!(identity.run_as_group, 65534);
        assert!(!identity.read_only_root_filesystem);
        let mut grouped = spec.clone();
        set(
            &mut grouped,
            "/containers/0/securityContext/runAsGroup",
            json!(1000),
        );
        set(
            &mut grouped,
            "/containers/0/securityContext/readOnlyRootFilesystem",
            json!(true),
        );
        let identity = PodRuntimeProfile
            .container(&grouped, &grouped["containers"][0])
            .unwrap();
        assert_eq!(identity.run_as_group, 1000);
        assert!(identity.read_only_root_filesystem);
        let mut inherited = spec;
        set(
            &mut inherited,
            "/containers/0/securityContext/runAsUser",
            Value::Null,
        );
        set(
            &mut inherited,
            "/securityContext",
            json!({"runAsUser":1000,"runAsGroup":2000}),
        );
        let identity = PodRuntimeProfile
            .container(&inherited, &inherited["containers"][0])
            .unwrap();
        assert_eq!((identity.run_as_user, identity.run_as_group), (1000, 2000));
    }
    #[test]
    fn unimplemented_semantics_are_refused() {
        for (pointer, value) in [
            ("/hostNetwork", json!(true)),
            (
                "/initContainers",
                json!([{"name":"init","image":"busybox"}]),
            ),
            ("/automountServiceAccountToken", json!(true)),
            ("/enableServiceLinks", json!(true)),
            ("/imagePullSecrets", json!([{"name":"registry"}])),
            ("/terminationGracePeriodSeconds", json!(31)),
            ("/restartPolicy", json!("Sometimes")),
            ("/dnsPolicy", json!("ClusterFirstWithHostNet")),
            ("/hostname", json!("has_underscore")),
            (
                "/securityContext",
                json!({"sysctls":[{"name":"net.ipv4.tcp_rmem","value":"1"}]}),
            ),
            ("/containers/0/securityContext/privileged", json!(true)),
            ("/containers/0/securityContext/runAsUser", json!(0)),
            ("/containers/0/securityContext/runAsGroup", json!(0)),
            (
                "/containers/0/securityContext/allowPrivilegeEscalation",
                json!(true),
            ),
            (
                "/containers/0/securityContext/seccompProfile",
                json!({"type":"Unconfined"}),
            ),
            (
                "/containers/0/securityContext/seccompProfile",
                json!({"type":"Localhost","localhostProfile":"x.json"}),
            ),
            (
                "/containers/0/securityContext/capabilities/add",
                json!(["NET_BIND_SERVICE"]),
            ),
            ("/containers/0/securityContext/capabilities/drop", json!([])),
            ("/containers/0/securityContext/procMount", json!("Unmasked")),
            (
                "/containers/0/livenessProbe",
                json!({"httpGet":{"port":80}}),
            ),
            (
                "/containers/0/readinessProbe",
                json!({"httpGet":{"port":80,"host":"metadata.internal"}}),
            ),
            (
                "/containers/0/readinessProbe",
                json!({"httpGet":{"port":80},"exec":{"command":["true"]}}),
            ),
            (
                "/containers/0/terminationMessagePolicy",
                json!("FallbackToLogsOnError"),
            ),
            ("/containers/0/workingDir", json!("relative")),
            (
                "/containers/0/env",
                json!([{"name":"X","valueFrom":{"resourceFieldRef":{"resource":"limits.cpu"}}}]),
            ),
            (
                "/containers/0/env",
                json!([{"name":"X","valueFrom":{"fieldRef":{"fieldPath":"status.podIP"}}}]),
            ),
            (
                "/containers/0/envFrom",
                json!([{"secretRef":{"name":"s"},"optional":true}]),
            ),
            (
                "/containers/0/volumeMounts",
                json!([{"name":"missing","mountPath":"/x"}]),
            ),
            (
                "/containers/0/volumeMounts",
                json!([{"name":"config","mountPath":"/x","subPath":"a"}]),
            ),
            (
                "/containers/0/volumeMounts",
                json!([{"name":"config","mountPath":"/etc/a"},{"name":"secret","mountPath":"/etc/a/b"}]),
            ),
            ("/volumes", json!([{"name":"host","hostPath":{"path":"/"}}])),
            ("/volumes", json!([{"name":"scratch","emptyDir":{}}])),
            (
                "/volumes",
                json!([{"name":"both","configMap":{"name":"a"},"secret":{"secretName":"b"}}]),
            ),
            ("/volumes", json!([{"name":"..","configMap":{"name":"a"}}])),
            (
                "/volumes",
                json!([{"name":"a","configMap":{"name":"x"}},{"name":"a","secret":{"secretName":"y"}}]),
            ),
        ] {
            let mut bad = spec();
            set(&mut bad, pointer, value);
            assert!(PodRuntimeProfile.check(&bad).is_err(), "{pointer}");
        }
        for path in [
            "",
            "../escape",
            "/absolute",
            "..data",
            "a/../b",
            "a/./b",
            "a//b",
            "a/",
            "x\0y",
        ] {
            let mut bad = spec();
            bad["volumes"][0]["configMap"]["items"][0]["path"] = json!(path);
            assert!(PodRuntimeProfile.check(&bad).is_err(), "{path:?}");
        }
        for path in [
            "/",
            "/proc",
            "/sys/a",
            "/dev/shm",
            "/etc/../proc",
            "relative",
        ] {
            let mut bad = spec();
            bad["containers"][0]["volumeMounts"][0]["mountPath"] = json!(path);
            assert!(PodRuntimeProfile.check(&bad).is_err(), "{path}");
        }
        for value in [json!(-1), json!(512), json!("0444"), json!(true)] {
            let mut bad = spec();
            bad["volumes"][0]["configMap"]["defaultMode"] = value;
            assert!(PodRuntimeProfile.check(&bad).is_err());
        }
        let mut bad = spec();
        bad["volumes"][0]["configMap"]["items"] =
            json!([{"key":"a","path":"nested"},{"key":"b","path":"nested/mode"}]);
        assert!(PodRuntimeProfile.check(&bad).is_err());
        assert!(PodRuntimeProfile.check(&json!({"containers":[]})).is_err());
        assert!(PodRuntimeProfile.check(&Value::Null).is_err());
        assert_eq!(
            PodRuntimeProfile.check(&json!({"containers":[{"name":"w","image":"i"}]})),
            Err(Unsupported(
                "service-account token projection is not implemented"
            ))
        );
    }
    #[test]
    fn probes_cannot_choose_a_host_or_add_arbitrary_credentials() {
        probe(&json!({"httpGet":{"port":8080,"path":"/ready"}})).unwrap();
        probe(&json!({"tcpSocket":{"port":8080},"periodSeconds":5})).unwrap();
        for p in [
            json!({"httpGet":{"port":8080,"host":"metadata.internal"}}),
            json!({"httpGet":{"port":8080,"httpHeaders":[{"name":"Authorization","value":"secret"}]}}),
            json!({"httpGet":{"port":8080,"path":"//outside"}}),
            json!({"httpGet":{"port":8080,"scheme":"HTTPS"}}),
            json!({"httpGet":{"port":8080},"exec":{"command":["true"]}}),
            json!({"tcpSocket":{"port":8080},"periodSeconds":0}),
        ] {
            assert!(probe(&p).is_err(), "{p}");
        }
    }
}
