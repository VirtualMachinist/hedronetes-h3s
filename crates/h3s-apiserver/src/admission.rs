//! In-process admission at the API write boundary. Kubernetes v1.34 policy
//! reference: https://v1-34.docs.kubernetes.io/docs/concepts/security/pod-security-standards/
use crate::{Failure, Result};
use h3s_api::pod_profile::PodRuntimeProfile;
use serde_json::Value;

const PREFIX: &str = "pod-security.kubernetes.io/";
#[derive(Clone, Copy, PartialEq)]
enum Level {
    Privileged,
    Baseline,
    Restricted,
}

fn forbidden(message: impl Into<String>) -> Failure {
    Failure::new(403, "Forbidden", message)
}

/// Validate supported configuration rather than silently ignoring a requested
/// policy/version. Audit and warning delivery will extend this contract later.
pub fn namespace(value: &Value) -> Result<()> {
    if let Some(labels) = value["metadata"]["labels"].as_object() {
        for (key, value) in labels {
            let Some(setting) = key.strip_prefix(PREFIX) else {
                continue;
            };
            let valid = match setting {
                "enforce" => matches!(
                    value.as_str(),
                    Some("privileged" | "baseline" | "restricted")
                ),
                "enforce-version" => matches!(value.as_str(), Some("latest" | "v1.34")),
                _ => false,
            };
            if !valid {
                return Err(Failure::new(422, "Invalid", format!("unsupported Pod Security setting {key}; supported: enforce and enforce-version (latest or v1.34)")));
            }
        }
    }
    Ok(())
}

pub fn lifecycle(namespace: &Value, create: bool) -> Result<()> {
    if create
        && (namespace["status"]["phase"] == "Terminating"
            || !namespace["metadata"]["deletionTimestamp"].is_null())
    {
        return Err(forbidden(
            "cannot create new content in a terminating namespace",
        ));
    }
    Ok(())
}

/// The runtime profile is not a Pod Security level: no namespace label can
/// admit a Pod the node cannot execute, and nothing forbidden is persisted.
pub fn runtime(pod: &Value) -> Result<()> {
    PodRuntimeProfile.check(&pod["spec"]).map_err(|e| {
        Failure::new(
            422,
            "Invalid",
            format!(
                "Pod cannot run under the {} runtime profile ({}): {e}",
                PodRuntimeProfile::NAME,
                PodRuntimeProfile::CONTRACT_SET
            ),
        )
    })
}

pub fn pod(namespace: &Value, pod: &Value) -> Result<()> {
    let name = namespace["metadata"]["name"].as_str().unwrap_or("");
    let default = if matches!(name, "kube-system" | "kube-public" | "kube-node-lease") {
        "privileged"
    } else {
        "restricted"
    };
    let policy = namespace["metadata"]["labels"][format!("{PREFIX}enforce")]
        .as_str()
        .unwrap_or(default);
    let level = match policy {
        "privileged" => Level::Privileged,
        "baseline" => Level::Baseline,
        "restricted" => Level::Restricted,
        _ => return Err(forbidden("invalid stored namespace Pod Security policy")),
    };
    let version = namespace["metadata"]["labels"][format!("{PREFIX}enforce-version")].as_str();
    if !matches!(version, None | Some("latest" | "v1.34")) {
        return Err(forbidden("unsupported stored Pod Security policy version"));
    }
    check(pod, level)
        .map_err(|field| forbidden(format!("violates PodSecurity {policy}:v1.34: {field}")))
}

fn items(value: &Value) -> impl Iterator<Item = &Value> {
    value.as_array().into_iter().flatten()
}
fn nonempty(value: &Value) -> bool {
    value.as_str().is_some_and(|v| !v.is_empty())
}
fn allowed(value: &Value, values: &[&str]) -> bool {
    value.is_null() || value.as_str().is_some_and(|v| values.contains(&v))
}
fn profile(context: &Value, profile: &str) -> bool {
    context[profile].is_null()
        || allowed(&context[profile]["type"], &["RuntimeDefault", "Localhost"])
}
fn check(pod: &Value, level: Level) -> std::result::Result<(), String> {
    if level == Level::Privileged {
        return Ok(());
    }
    let spec = &pod["spec"];
    let pc = &spec["securityContext"];
    let restricted = level == Level::Restricted;
    // The M1 worker is Linux-only. Do not accept a Windows OS declaration as a
    // way to bypass Linux-only checks and then execute it on a Linux worker.
    if !allowed(&spec["os"]["name"], &["linux"]) {
        return Err("spec.os.name must be linux for the supported worker".into());
    }
    for field in ["hostNetwork", "hostPID", "hostIPC"] {
        if spec[field] == true {
            return Err(format!("spec.{field} is forbidden"));
        }
    }
    for volume in items(&spec["volumes"]) {
        if !volume["hostPath"].is_null() {
            return Err("hostPath volumes are forbidden".into());
        }
        if restricted {
            let allowed_types = [
                "configMap",
                "csi",
                "downwardAPI",
                "emptyDir",
                "ephemeral",
                "persistentVolumeClaim",
                "projected",
                "secret",
            ];
            // Also reject mixed allowed+disallowed sources rather than hiding a
            // host volume behind an allowed field in an otherwise invalid object.
            let sources: Vec<_> = volume
                .as_object()
                .into_iter()
                .flatten()
                .filter(|(k, v)| k.as_str() != "name" && !v.is_null())
                .collect();
            if sources.len() != 1 || !allowed_types.contains(&sources[0].0.as_str()) {
                return Err("restricted volume type is required".into());
            }
        }
    }
    for sysctl in items(&pc["sysctls"]) {
        let name = sysctl["name"].as_str().unwrap_or("").replace('/', ".");
        if ![
            "kernel.shm_rmid_forced",
            "net.ipv4.ip_local_port_range",
            "net.ipv4.ip_unprivileged_port_start",
            "net.ipv4.tcp_syncookies",
            "net.ipv4.ping_group_range",
            "net.ipv4.ip_local_reserved_ports",
            "net.ipv4.tcp_keepalive_time",
            "net.ipv4.tcp_fin_timeout",
            "net.ipv4.tcp_keepalive_intvl",
            "net.ipv4.tcp_keepalive_probes",
            "net.ipv4.tcp_rmem",
            "net.ipv4.tcp_wmem",
        ]
        .contains(&name.as_str())
        {
            return Err("unsafe sysctl is forbidden".into());
        }
    }
    if let Some(annotations) = pod["metadata"]["annotations"].as_object() {
        for (key, value) in annotations {
            if key.starts_with("container.apparmor.security.beta.kubernetes.io/")
                && !value
                    .as_str()
                    .is_some_and(|v| v == "runtime/default" || v.starts_with("localhost/"))
            {
                return Err("unconfined AppArmor annotation is forbidden".into());
            }
        }
    }
    let containers: Vec<_> = ["containers", "initContainers", "ephemeralContainers"]
        .into_iter()
        .flat_map(|field| {
            items(&spec[field])
                .enumerate()
                .map(move |(i, c)| (format!("spec.{field}[{i}]"), c))
        })
        .collect();
    for (path, context) in std::iter::once(("spec.securityContext".into(), pc)).chain(
        containers
            .iter()
            .map(|(p, c)| (format!("{p}.securityContext"), &c["securityContext"])),
    ) {
        if context["windowsOptions"]["hostProcess"] == true {
            return Err(format!("{path}.windowsOptions.hostProcess is forbidden"));
        }
        if !profile(context, "appArmorProfile") || !profile(context, "seccompProfile") {
            return Err(format!(
                "{path} requires RuntimeDefault or Localhost profiles"
            ));
        }
        let selinux = &context["seLinuxOptions"];
        if nonempty(&selinux["user"])
            || nonempty(&selinux["role"])
            || !allowed(
                &selinux["type"],
                &[
                    "",
                    "container_t",
                    "container_init_t",
                    "container_kvm_t",
                    "container_engine_t",
                ],
            )
        {
            return Err(format!("{path}.seLinuxOptions is forbidden"));
        }
        if restricted && (context["runAsNonRoot"] == false || context["runAsUser"] == 0) {
            return Err(format!("{path} must not request root"));
        }
    }
    for (path, container) in containers {
        let context = &container["securityContext"];
        if context["privileged"] == true {
            return Err(format!("{path} must not be privileged"));
        }
        if !allowed(&context["procMount"], &["Default"]) {
            return Err(format!("{path} must use default procMount"));
        }
        if items(&container["ports"]).any(|p| p["hostPort"].as_i64().is_some_and(|v| v != 0)) {
            return Err(format!("{path} must not use host ports"));
        }
        for pointer in [
            "/livenessProbe",
            "/readinessProbe",
            "/startupProbe",
            "/lifecycle/postStart",
            "/lifecycle/preStop",
        ] {
            let handler = container.pointer(pointer).unwrap_or(&Value::Null);
            if ["httpGet", "tcpSocket"]
                .iter()
                .any(|p| nonempty(&handler[p]["host"]))
            {
                return Err(format!("{path}{pointer} must not set a host"));
            }
        }
        let capabilities = &context["capabilities"];
        let baseline = [
            "AUDIT_WRITE",
            "CHOWN",
            "DAC_OVERRIDE",
            "FOWNER",
            "FSETID",
            "KILL",
            "MKNOD",
            "NET_BIND_SERVICE",
            "SETFCAP",
            "SETGID",
            "SETPCAP",
            "SETUID",
            "SYS_CHROOT",
        ];
        for cap in items(&capabilities["add"]) {
            if !(if restricted {
                cap == "NET_BIND_SERVICE"
            } else {
                allowed(cap, &baseline)
            }) {
                return Err(format!("{path} adds forbidden capabilities"));
            }
        }
        if restricted {
            if context["allowPrivilegeEscalation"] != false {
                return Err(format!("{path} requires allowPrivilegeEscalation=false"));
            }
            if !items(&capabilities["drop"]).any(|c| c == "ALL") {
                return Err(format!("{path} must drop ALL capabilities"));
            }
            if context["runAsNonRoot"] != true && pc["runAsNonRoot"] != true {
                return Err(format!("{path} requires runAsNonRoot=true"));
            }
            let seccomp = if context["seccompProfile"].is_null() {
                &pc["seccompProfile"]
            } else {
                &context["seccompProfile"]
            };
            if !matches!(
                seccomp["type"].as_str(),
                Some("RuntimeDefault" | "Localhost")
            ) {
                return Err(format!("{path} requires an explicit seccomp profile"));
            }
        }
    }
    Ok(())
}
