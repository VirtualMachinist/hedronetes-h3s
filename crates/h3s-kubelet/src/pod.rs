//! Kubernetes Pod to CRI translation. Unsupported execution semantics fail closed.
use crate::{invalid, Result};
use h3s_cri::v1::*;
use serde_json::Value;
use std::collections::HashMap;
pub const NODE: &str = "io.hedronetes.node";
pub const UID: &str = "io.hedronetes.pod.uid";
pub const HASH: &str = "io.hedronetes.container.hash";
pub fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("required Pod field missing"))
}
pub fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 253
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && s != "."
        && s != ".."
}
pub fn fields(v: &Value, allowed: &[&str]) -> Result<()> {
    if v.as_object().is_some_and(|m| {
        m.iter()
            .any(|(k, v)| !v.is_null() && !allowed.contains(&k.as_str()))
    }) {
        return Err(invalid("Pod contains an unsupported execution field"));
    }
    Ok(())
}
pub fn labels(node: &str, uid: &str) -> HashMap<String, String> {
    HashMap::from([(NODE.into(), node.into()), (UID.into(), uid.into())])
}
pub fn owned(labels: &HashMap<String, String>, node: &str) -> bool {
    labels.get(NODE).is_some_and(|s| s == node)
        && labels.get(UID).is_some_and(|s| safe_component(s))
}
pub fn validate(p: &Value, node: &str) -> Result<()> {
    for k in ["uid", "name", "namespace"] {
        if !safe_component(text(&p["metadata"], k)?) {
            return Err(invalid("invalid Pod identity"));
        }
    }
    let s = &p["spec"];
    if s["restartPolicy"]
        .as_str()
        .is_some_and(|s| !matches!(s, "Always" | "OnFailure" | "Never"))
    {
        return Err(invalid("invalid restart policy"));
    }
    if s["volumes"].as_array().is_some_and(|v| !v.is_empty()) {
        return Err(invalid("Pod volumes are not implemented yet"));
    }
    if s["terminationGracePeriodSeconds"]
        .as_i64()
        .is_some_and(|v| !(0..=30).contains(&v))
    {
        return Err(invalid("termination grace outside supported 0-30 seconds"));
    }
    if text(s, "nodeName")? != node {
        return Err(invalid("Pod is assigned to another node"));
    }
    fields(
        s,
        &[
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
        ],
    )?;
    if s["automountServiceAccountToken"] != false || s["enableServiceLinks"] != false {
        return Err(invalid(
            "service-account projection and service environment injection are not implemented",
        ));
    }
    if s["imagePullSecrets"]
        .as_array()
        .is_some_and(|a| !a.is_empty())
    {
        return Err(invalid("private image authentication is not implemented"));
    }
    if !matches!(s["dnsPolicy"].as_str(), Some("Default") | Some("None")) {
        return Err(invalid("cluster DNS is not configured"));
    }
    fields(&s["dnsConfig"], &["nameservers", "searches", "options"])?;
    if s["dnsPolicy"] == "Default" && !s["dnsConfig"].is_null() {
        return Err(invalid("DNS overrides require dnsPolicy None"));
    }
    fields(
        &s["securityContext"],
        &["runAsUser", "runAsGroup", "runAsNonRoot", "seccompProfile"],
    )?;
    if s["hostname"]
        .as_str()
        .is_some_and(|v| !safe_component(v) || v.len() > 63)
    {
        return Err(invalid("invalid Pod hostname"));
    }
    let containers = s["containers"]
        .as_array()
        .filter(|v| !v.is_empty() && v.len() <= 32)
        .ok_or_else(|| invalid("invalid container count"))?;
    for c in containers {
        if c["volumeMounts"].as_array().is_some_and(|v| !v.is_empty()) {
            return Err(invalid("Pod volumes are not implemented yet"));
        }
        fields(
            c,
            &[
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
            ],
        )?;
        if !safe_component(text(c, "name")?) {
            return Err(invalid("invalid container name"));
        }
        if c["terminationMessagePolicy"]
            .as_str()
            .is_some_and(|s| s != "File")
        {
            return Err(invalid("termination message policy is unsupported"));
        }
        for port in c["ports"].as_array().into_iter().flatten() {
            fields(port, &["name", "containerPort", "protocol"])?;
        }
        if c["workingDir"]
            .as_str()
            .is_some_and(|s| !s.is_empty() && (!s.starts_with('/') || s.contains('\0')))
        {
            return Err(invalid("invalid working directory"));
        }
        let sc = &c["securityContext"];
        fields(
            sc,
            &[
                "runAsUser",
                "runAsGroup",
                "runAsNonRoot",
                "allowPrivilegeEscalation",
                "readOnlyRootFilesystem",
                "capabilities",
                "seccompProfile",
            ],
        )?;
        let uid = sc["runAsUser"]
            .as_i64()
            .or(s["securityContext"]["runAsUser"].as_i64());
        if !uid.is_some_and(|u| u > 0 && u <= u32::MAX as i64)
            || sc["allowPrivilegeEscalation"] != false
        {
            return Err(invalid(
                "runtime requires an explicit non-root UID and no privilege escalation",
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
            return Err(invalid("runtime requires all capabilities dropped"));
        }
        let sec = if sc["seccompProfile"].is_null() {
            &s["securityContext"]["seccompProfile"]
        } else {
            &sc["seccompProfile"]
        };
        fields(sec, &["type"])?;
        if sec["type"] != "RuntimeDefault" {
            return Err(invalid("runtime-default seccomp is required"));
        }
        fields(&c["resources"], &["requests", "limits"])?;
        for k in ["requests", "limits"] {
            fields(&c["resources"][k], &["cpu", "memory"])?;
        }
        for e in c["env"].as_array().into_iter().flatten() {
            fields(e, &["name", "value", "valueFrom"])?;
        }
        for m in c["volumeMounts"].as_array().into_iter().flatten() {
            fields(m, &["name", "mountPath", "readOnly"])?;
            let path = text(m, "mountPath")?;
            if !path.starts_with('/')
                || path.split('/').any(|p| p == "..")
                || path.contains('\0')
                || matches!(path, "/" | "/proc" | "/sys" | "/dev")
                || path.starts_with("/proc/")
                || path.starts_with("/sys/")
                || path.starts_with("/dev/")
            {
                return Err(invalid("invalid container mount path"));
            }
        }
        if !c["readinessProbe"].is_null() {
            validate_probe(&c["readinessProbe"])?;
        }
    }
    Ok(())
}
pub fn validate_probe(v: &Value) -> Result<()> {
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
        return Err(invalid("invalid readiness probe"));
    }
    fields(&v["exec"], &["command"])?;
    fields(&v["httpGet"], &["path", "port", "scheme"])?;
    fields(&v["tcpSocket"], &["port"])?;
    if v["httpGet"]["scheme"].as_str().is_some_and(|s| s != "HTTP") {
        return Err(invalid("only HTTP readiness is supported"));
    }
    if v["httpGet"]["path"]
        .as_str()
        .is_some_and(|s| !s.starts_with('/') || s.starts_with("//"))
    {
        return Err(invalid("invalid readiness path"));
    }
    Ok(())
}
/// Exact fixed-point quantity conversion; round fractional units up like Kubernetes.
pub fn quantity(s: &str, cpu: bool) -> Result<i64> {
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (n, suffix) = s.split_at(split);
    let (whole, frac) = n.split_once('.').unwrap_or((n, ""));
    if whole.is_empty() || frac.len() > 9 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("unsupported resource quantity"));
    }
    let scale = 10i128.pow(frac.len() as u32);
    let base = whole
        .parse::<i128>()
        .ok()
        .and_then(|x| x.checked_mul(scale))
        .and_then(|x| {
            x.checked_add(if frac.is_empty() {
                0
            } else {
                frac.parse::<i128>().ok()?
            })
        })
        .ok_or_else(|| invalid("invalid resource quantity"))?;
    let (mul, div) = match suffix {
        "" => (1, 1),
        "m" => (1, 1000),
        "u" => (1, 1_000_000),
        "n" => (1, 1_000_000_000),
        "Ki" => (1024, 1),
        "Mi" => (1024 * 1024, 1),
        "Gi" => (1024 * 1024 * 1024, 1),
        "Ti" => (1024i128.pow(4), 1),
        "k" | "K" => (1000, 1),
        "M" => (1_000_000, 1),
        "G" => (1_000_000_000, 1),
        _ => return Err(invalid("unsupported resource quantity suffix")),
    };
    let numerator = base
        .checked_mul(mul)
        .and_then(|x| x.checked_mul(if cpu { 1000 } else { 1 }))
        .ok_or_else(|| invalid("resource quantity overflow"))?;
    let denominator = scale * div;
    i64::try_from(numerator / denominator + i128::from(numerator % denominator != 0))
        .map_err(|_| invalid("resource quantity overflow"))
}
pub fn resources(c: &Value) -> Result<LinuxContainerResources> {
    let limits = &c["resources"]["limits"];
    let requests = &c["resources"]["requests"];
    let cpu = limits["cpu"]
        .as_str()
        .map(|s| quantity(s, true))
        .transpose()?
        .unwrap_or(0);
    let memory = limits["memory"]
        .as_str()
        .map(|s| quantity(s, false))
        .transpose()?
        .unwrap_or(0);
    let request = requests["cpu"]
        .as_str()
        .map(|s| quantity(s, true))
        .transpose()?
        .unwrap_or(cpu);
    Ok(LinuxContainerResources {
        cpu_period: 100_000,
        cpu_quota: if cpu > 0 {
            cpu.checked_mul(100)
                .ok_or_else(|| invalid("CPU limit overflow"))?
                .max(1000)
        } else {
            0
        },
        cpu_shares: (request.saturating_mul(1024) / 1000).clamp(2, 262144),
        memory_limit_in_bytes: memory,
        ..Default::default()
    })
}
pub fn strings(v: &Value) -> Result<Vec<String>> {
    if v.is_null() {
        return Ok(vec![]);
    }
    v.as_array()
        .ok_or_else(|| invalid("expected string list"))?
        .iter()
        .map(|v| {
            v.as_str()
                .filter(|s| !s.contains('\0'))
                .map(str::to_owned)
                .ok_or_else(|| invalid("invalid string list"))
        })
        .collect()
}
pub fn security(p: &Value, c: &Value) -> LinuxContainerSecurityContext {
    let sc = &c["securityContext"];
    let ps = &p["spec"]["securityContext"];
    LinuxContainerSecurityContext {
        run_as_user: Some(Int64Value {
            value: sc["runAsUser"]
                .as_i64()
                .or(ps["runAsUser"].as_i64())
                .expect("validated UID"),
        }),
        run_as_group: sc["runAsGroup"]
            .as_i64()
            .or(ps["runAsGroup"].as_i64())
            .map(|value| Int64Value { value }),
        no_new_privs: true,
        readonly_rootfs: sc["readOnlyRootFilesystem"].as_bool().unwrap_or(false),
        capabilities: Some(Capability {
            drop_capabilities: vec!["ALL".into()],
            ..Default::default()
        }),
        seccomp: Some(SecurityProfile {
            profile_type: security_profile::ProfileType::RuntimeDefault as i32,
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    pub fn fixture() -> Value {
        json!({"metadata":{"name":"web","namespace":"default","uid":"3e17c2c0-49e2-4f2b-a917-89da3f986647"},"spec":{"nodeName":"worker","automountServiceAccountToken":false,"enableServiceLinks":false,"dnsPolicy":"Default","containers":[{"name":"web","image":"busybox:1.37.0","securityContext":{"runAsUser":65534,"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]},"seccompProfile":{"type":"RuntimeDefault"}}}]}})
    }
    #[test]
    fn validates_assignment_and_rejects_unimplemented_security_semantics() {
        let p = fixture();
        validate(&p, "worker").unwrap();
        assert!(validate(&p, "foreign").is_err());
        for (pointer, value) in [
            ("/spec/hostNetwork", json!(true)),
            ("/spec/containers/0/securityContext/privileged", json!(true)),
            ("/spec/containers/0/securityContext/runAsUser", json!(0)),
            (
                "/spec/containers/0/securityContext/allowPrivilegeEscalation",
                json!(true),
            ),
            (
                "/spec/containers/0/securityContext/seccompProfile",
                json!({"type":"Unconfined"}),
            ),
            (
                "/spec/containers/0/securityContext/capabilities/add",
                json!(["SYS_ADMIN"]),
            ),
            (
                "/spec/volumes",
                json!([{"name":"host","hostPath":{"path":"/"}}]),
            ),
            ("/spec/automountServiceAccountToken", json!(true)),
        ] {
            let mut bad = p.clone();
            let (parent, key) = pointer.rsplit_once('/').unwrap();
            bad.pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert(key.into(), value);
            assert!(validate(&bad, "worker").is_err(), "{pointer}");
        }
        for uid in ["..", "../outside", "bad/name", ""] {
            let mut bad = p.clone();
            bad["metadata"]["uid"] = json!(uid);
            assert!(validate(&bad, "worker").is_err());
        }
        assert!(!owned(&labels("other", "uid"), "worker"));
        assert!(!owned(&labels("worker", ".."), "worker"));
    }
    #[test]
    fn quantities_preserve_cpu_and_memory_limits_without_float_rounding() {
        for (s, cpu, expected) in [
            ("100m", true, 100),
            ("0.1", true, 100),
            ("0.0001", true, 1),
            ("64Mi", false, 67108864),
            ("1.5Gi", false, 1610612736),
            ("1000m", false, 1),
        ] {
            assert_eq!(quantity(s, cpu).unwrap(), expected);
        }
        for s in [
            "-1",
            "not-a-number",
            "1.2.3",
            "999999999999999999999999999999999999999Gi",
        ] {
            assert!(quantity(s, false).is_err());
        }
        let r=resources(&json!({"resources":{"limits":{"cpu":"100m","memory":"64Mi"},"requests":{"cpu":"50m"}}})).unwrap();
        assert_eq!(r.cpu_quota, 10000);
        assert_eq!(r.cpu_period, 100000);
        assert_eq!(r.memory_limit_in_bytes, 67108864);
        assert_eq!(r.cpu_shares, 51);
    }
    #[test]
    fn probes_cannot_choose_a_host_or_add_arbitrary_credentials() {
        validate_probe(&json!({"httpGet":{"port":8080,"path":"/ready"}})).unwrap();
        for p in [
            json!({"httpGet":{"port":8080,"host":"metadata.internal"}}),
            json!({"httpGet":{"port":8080,"httpHeaders":[{"name":"Authorization","value":"secret"}]}}),
            json!({"httpGet":{"port":8080,"path":"//outside"}}),
            json!({"httpGet":{"port":8080},"exec":{"command":["true"]}}),
        ] {
            assert!(validate_probe(&p).is_err());
        }
    }
}
