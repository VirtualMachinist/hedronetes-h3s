//! Kubernetes Pod to CRI translation. The executable subset is
//! `h3s_api::pod_profile::PodRuntimeProfile`; this module only adds node
//! identity and turns the profile's decisions into CRI structures.
use crate::{invalid, Result};
use h3s_api::pod_profile::PodRuntimeProfile;
pub use h3s_api::pod_profile::{fields, safe_component, text};
use h3s_cri::v1::*;
use serde_json::Value;
use std::collections::HashMap;
pub const NODE: &str = "io.hedronetes.node";
pub const UID: &str = "io.hedronetes.pod.uid";
pub const HASH: &str = "io.hedronetes.container.hash";
pub fn labels(node: &str, uid: &str) -> HashMap<String, String> {
    HashMap::from([(NODE.into(), node.into()), (UID.into(), uid.into())])
}
pub fn owned(labels: &HashMap<String, String>, node: &str) -> bool {
    labels.get(NODE).is_some_and(|s| s == node)
        && labels.get(UID).is_some_and(|s| safe_component(s))
}
/// Identity, the runtime profile, then assignment to this node.
pub fn validate(p: &Value, node: &str) -> Result<()> {
    for k in ["uid", "name", "namespace"] {
        if !safe_component(text(&p["metadata"], k)?) {
            return Err(invalid("invalid Pod identity"));
        }
    }
    PodRuntimeProfile.check(&p["spec"])?;
    if text(&p["spec"], "nodeName")? != node {
        return Err(invalid("Pod is assigned to another node"));
    }
    Ok(())
}
/// Shared checked conversion keeps scheduler reservations and runtime limits aligned.
pub fn quantity(s: &str, cpu: bool) -> Result<i64> {
    h3s_api::quantity::quantity(s, cpu)
        .ok_or_else(|| invalid("unsupported or overflowing resource quantity"))
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
/// CRI security context from the profile's resolved container identity.
pub fn security(p: &Value, c: &Value) -> Result<LinuxContainerSecurityContext> {
    let execution = PodRuntimeProfile.container(&p["spec"], c)?;
    Ok(LinuxContainerSecurityContext {
        run_as_user: Some(Int64Value {
            value: execution.run_as_user,
        }),
        run_as_group: Some(Int64Value {
            value: execution.run_as_group,
        }),
        masked_paths: masked_paths(),
        readonly_paths: [
            "/proc/bus",
            "/proc/fs",
            "/proc/irq",
            "/proc/sys",
            "/proc/sysrq-trigger",
        ]
        .map(str::to_owned)
        .to_vec(),
        no_new_privs: true,
        readonly_rootfs: execution.read_only_root_filesystem,
        capabilities: Some(Capability {
            drop_capabilities: vec!["ALL".into()],
            ..Default::default()
        }),
        seccomp: Some(SecurityProfile {
            profile_type: security_profile::ProfileType::RuntimeDefault as i32,
            ..Default::default()
        }),
        ..Default::default()
    })
}

// Kubernetes v1.34 DefaultProcMount policy, from pkg/securitycontext/util.go
// (Apache-2.0). Runtime defaults must be explicit in CRI, not assumed from OCI.
fn masked_paths() -> Vec<String> {
    let mut paths = [
        "/proc/asound",
        "/proc/acpi",
        "/proc/interrupts",
        "/proc/kcore",
        "/proc/keys",
        "/proc/latency_stats",
        "/proc/timer_list",
        "/proc/timer_stats",
        "/proc/sched_debug",
        "/proc/scsi",
        "/sys/firmware",
        "/sys/devices/virtual/powercap",
    ]
    .map(str::to_owned)
    .to_vec();
    if let Ok(cpus) = std::fs::read_dir("/sys/devices/system/cpu") {
        for cpu in cpus.flatten() {
            let name = cpu.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name
                .strip_prefix("cpu")
                .is_some_and(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
            {
                let path = cpu.path().join("thermal_throttle");
                if path.exists() {
                    paths.push(path.to_string_lossy().into_owned());
                }
            }
        }
    }
    paths.sort();
    paths
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
        let ctx = security(&p, &p["spec"]["containers"][0]).unwrap();
        assert_eq!(ctx.run_as_user.as_ref().map(|v| v.value), Some(65534));
        assert_eq!(ctx.run_as_group.as_ref().map(|v| v.value), Some(65534));
        assert!(ctx.masked_paths.contains(&"/proc/kcore".into()));
        assert!(ctx.readonly_paths.contains(&"/proc/sys".into()));
        assert!(validate(&p, "foreign").is_err());
        for (pointer, value) in [
            ("/spec/hostNetwork", json!(true)),
            ("/spec/containers/0/securityContext/privileged", json!(true)),
            ("/spec/containers/0/securityContext/runAsUser", json!(0)),
            ("/spec/containers/0/securityContext/runAsGroup", json!(0)),
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
        let mut grouped = p.clone();
        grouped["spec"]["containers"][0]["securityContext"]["runAsGroup"] = json!(1000);
        assert_eq!(
            super::security(&grouped, &grouped["spec"]["containers"][0])
                .unwrap()
                .run_as_group
                .as_ref()
                .map(|v| v.value),
            Some(1000)
        );
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
}
