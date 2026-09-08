//! Desired Pods are fetched through the node-authorized API; CRI is authoritative
//! for observed processes. A failed/incomplete LIST never triggers orphan GC.
use crate::{inputs, invalid, now, pod, probe, Agent, Error, Result};
use h3s_certs::private;
use h3s_cri::{v1::*, Cri};
use reqwest::Method;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    time::Duration,
};
macro_rules! rpc {
    ($e:expr) => {
        $e.await.map_err(h3s_cri::Error::from)?.into_inner()
    };
}
pub struct Runtime {
    endpoint: String,
    root: PathBuf,
    probes: tokio::sync::Mutex<probe::State>,
}
impl Runtime {
    pub fn new(endpoint: String, root: PathBuf) -> Result<Self> {
        let path = endpoint.strip_prefix("unix://").unwrap_or(&endpoint);
        if !Path::new(path).is_absolute()
            || path.contains('\0')
            || Path::new(path)
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(invalid("runtime endpoint must be an absolute Unix socket"));
        }
        private::directory(&root)?;
        Ok(Self {
            endpoint,
            root,
            probes: tokio::sync::Mutex::new(probe::State::default()),
        })
    }
    pub async fn healthy(&self) -> bool {
        tokio::time::timeout(Duration::from_secs(5), async {
            let cri = Cri::connect(&self.endpoint).await?;
            let status = cri
                .runtime()
                .status(StatusRequest { verbose: false })
                .await
                .map_err(h3s_cri::Error::from)?
                .into_inner()
                .status;
            Ok::<bool, h3s_cri::Error>(status.is_some_and(|s| {
                ["RuntimeReady", "NetworkReady"]
                    .iter()
                    .all(|kind| s.conditions.iter().any(|c| c.r#type == *kind && c.status))
            }))
        })
        .await
        .is_ok_and(|v| v.unwrap_or(false))
    }
    pub async fn sweep(&self, agent: &Agent) -> Result<()> {
        let path = format!("api/v1/pods?fieldSelector=spec.nodeName%3D{}", agent.name);
        let (code, list) = agent.request(Method::GET, &path, None).await?;
        if code != 200 {
            return Err(Error::Status(code));
        }
        if list["metadata"]["continue"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
        {
            return Err(invalid("incomplete assigned Pod list"));
        }
        let pods = list["items"]
            .as_array()
            .filter(|p| p.len() <= 256)
            .ok_or_else(|| invalid("invalid or oversized assigned Pod list"))?;
        let mut desired = HashSet::new();
        for p in pods {
            if pod::text(&p["spec"], "nodeName")? != agent.name {
                return Err(invalid("assigned list contains foreign Pod"));
            }
            for key in ["name", "namespace"] {
                if !pod::safe_component(pod::text(&p["metadata"], key)?) {
                    return Err(invalid("invalid Pod path identity"));
                }
            }
            let uid = pod::text(&p["metadata"], "uid")?;
            if !pod::safe_component(uid) || !desired.insert(uid.to_owned()) {
                return Err(invalid("invalid or duplicate Pod UID"));
            }
        }
        let cri = Cri::connect(&self.endpoint).await?;
        let filter = PodSandboxFilter {
            label_selector: HashMap::from([(pod::NODE.into(), agent.name.clone())]),
            ..Default::default()
        };
        let sandboxes = rpc!(cri.runtime().list_pod_sandbox(ListPodSandboxRequest {
            filter: Some(filter)
        }))
        .items;
        let mut probes = self.probes.lock().await;
        let mut observed_ids = HashSet::new();
        for p in pods {
            let uid = pod::text(&p["metadata"], "uid")?;
            let own: Vec<_> = sandboxes
                .iter()
                .filter(|s| {
                    pod::owned(&s.labels, &agent.name)
                        && s.labels.get(pod::UID).is_some_and(|id| id == uid)
                })
                .cloned()
                .collect();
            if !p["metadata"]["deletionTimestamp"].is_null() {
                for s in own {
                    remove(&cri, &s, &agent.name).await?;
                }
                continue;
            }
            match self.sync(&cri, agent, p, own, &mut probes).await {
                Ok(status) => {
                    for c in status["containerStatuses"].as_array().into_iter().flatten() {
                        if let Some(id) = c["containerID"]
                            .as_str()
                            .and_then(|s| s.strip_prefix("containerd://"))
                        {
                            observed_ids.insert(id.to_owned());
                        }
                    }
                    publish(agent, p, status).await?;
                }
                Err(e) => {
                    eprintln!("h3s Pod {uid}: {e}; retrying");
                    let mut status = if p["status"].is_object() {
                        p["status"].clone()
                    } else {
                        json!({})
                    };
                    if status["phase"].is_null() {
                        status["phase"] = json!("Pending");
                    }
                    status["reason"] = json!("PodSyncError");
                    status["message"] =
                        json!("Pod configuration or runtime operation failed; retrying");
                    conditions(&mut status, p, false, false);
                    publish(agent, p, status).await?;
                }
            }
        }
        // Only the complete, validated API snapshot can establish absence.
        for s in &sandboxes {
            if pod::owned(&s.labels, &agent.name) && !desired.contains(&s.labels[pod::UID]) {
                remove(&cri, s, &agent.name).await?;
                let dir = self.root.join(&s.labels[pod::UID]);
                if dir.try_exists()? {
                    private::directory(&dir)?;
                    std::fs::remove_dir_all(dir)?;
                }
            }
        }
        probes.retain(&observed_ids);
        Ok(())
    }
    async fn sync(
        &self,
        cri: &Cri,
        agent: &Agent,
        p: &Value,
        mut sandboxes: Vec<PodSandbox>,
        probes: &mut probe::State,
    ) -> Result<Value> {
        pod::validate(p, &agent.name)?;
        let uid = pod::text(&p["metadata"], "uid")?;
        let name = pod::text(&p["metadata"], "name")?;
        let ns = pod::text(&p["metadata"], "namespace")?;
        let root = self.root.join(uid);
        private::directory(&root)?;
        let logs = root.join("logs");
        private::directory(&logs)?;
        let mut prepared = vec![];
        for c in p["spec"]["containers"]
            .as_array()
            .expect("validated containers")
        {
            let hash = format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(c).expect("container JSON"))
            );
            let mut labels = pod::labels(&agent.name, uid);
            labels.insert(pod::HASH.into(), hash);
            let config = ContainerConfig {
                metadata: Some(ContainerMetadata {
                    name: pod::text(c, "name")?.into(),
                    attempt: 0,
                }),
                image: Some(ImageSpec {
                    image: pod::text(c, "image")?.into(),
                    ..Default::default()
                }),
                command: pod::strings(&c["command"])?,
                args: pod::strings(&c["args"])?,
                working_dir: c["workingDir"].as_str().unwrap_or("").into(),
                labels,
                linux: Some(LinuxContainerConfig {
                    resources: Some(pod::resources(c)?),
                    security_context: Some(pod::security(p, c)),
                }),
                ..Default::default()
            };
            prepared.push((c, config));
        }
        sandboxes.sort_by_key(|s| std::cmp::Reverse(s.created_at));
        let sandbox_attempt = sandboxes
            .iter()
            .filter_map(|s| s.metadata.as_ref().map(|m| m.attempt))
            .max()
            .map(|n| n.saturating_add(1))
            .unwrap_or(0);
        let existing = sandboxes
            .iter()
            .find(|s| s.state == PodSandboxState::SandboxReady as i32)
            .map(|s| s.id.clone());
        for s in &sandboxes {
            if Some(&s.id) != existing.as_ref() {
                remove(cri, s, &agent.name).await?;
            }
        }
        let sandbox = PodSandboxConfig {
            metadata: Some(PodSandboxMetadata {
                name: name.into(),
                namespace: ns.into(),
                uid: uid.into(),
                attempt: sandbox_attempt,
            }),
            hostname: p["spec"]["hostname"].as_str().unwrap_or(name).into(),
            log_directory: logs
                .to_str()
                .ok_or_else(|| invalid("non UTF-8 Pod log path"))?
                .into(),
            dns_config: Some(dns(p)?),
            labels: pod::labels(&agent.name, uid),
            linux: Some(LinuxPodSandboxConfig {
                cgroup_parent: "h3s-pods.slice".into(),
                security_context: Some(LinuxSandboxSecurityContext {
                    namespace_options: Some(NamespaceOption {
                        network: NamespaceMode::Pod as i32,
                        pid: NamespaceMode::Container as i32,
                        ipc: NamespaceMode::Pod as i32,
                        ..Default::default()
                    }),
                    seccomp: Some(SecurityProfile {
                        profile_type: security_profile::ProfileType::RuntimeDefault as i32,
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let sandbox_id = if let Some(id) = existing {
            id
        } else {
            rpc!(cri.runtime().run_pod_sandbox(RunPodSandboxRequest {
                config: Some(sandbox.clone()),
                runtime_handler: "youki".into()
            }))
            .pod_sandbox_id
        };
        let ps = rpc!(cri.runtime().pod_sandbox_status(PodSandboxStatusRequest {
            pod_sandbox_id: sandbox_id.clone(),
            verbose: false
        }))
        .status
        .ok_or_else(|| invalid("runtime omitted sandbox status"))?;
        let ip = ps.network.map(|n| n.ip).unwrap_or_default();
        let all = rpc!(cri.runtime().list_containers(ListContainersRequest {
            filter: Some(ContainerFilter {
                pod_sandbox_id: sandbox_id.clone(),
                label_selector: pod::labels(&agent.name, uid),
                ..Default::default()
            })
        }))
        .containers;
        let mut statuses = vec![];
        let mut all_ready = true;
        let mut any_running = false;
        let mut all_terminated = true;
        let mut all_success = true;
        for (c, mut config) in prepared {
            let container_name = pod::text(c, "name")?;
            let mut matches: Vec<_> = all
                .iter()
                .filter(|r| {
                    pod::owned(&r.labels, &agent.name)
                        && r.labels.get(pod::UID).is_some_and(|s| s == uid)
                        && r.metadata
                            .as_ref()
                            .is_some_and(|m| m.name == container_name)
                })
                .collect();
            matches.sort_by_key(|r| {
                std::cmp::Reverse(r.metadata.as_ref().map(|m| m.attempt).unwrap_or(0))
            });
            let mut current = None;
            if let Some(r) = matches.first() {
                current = rpc!(cri.runtime().container_status(ContainerStatusRequest {
                    container_id: r.id.clone(),
                    verbose: false
                }))
                .status;
            }
            let old_restart = p["status"]["containerStatuses"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|s| s["name"] == container_name)
                .and_then(|s| s["restartCount"].as_u64())
                .unwrap_or(0)
                .min(u32::MAX as u64) as u32;
            let same = matches
                .first()
                .is_some_and(|r| r.labels.get(pod::HASH) == config.labels.get(pod::HASH));
            let restart = should_restart(
                current.as_ref(),
                same,
                p["spec"]["restartPolicy"].as_str().unwrap_or("Always"),
            );
            if restart
                && current.as_ref().is_none_or(|s| {
                    s.state != ContainerState::ContainerExited as i32 || !same || retry_due(s)
                })
            {
                let attempt = current
                    .as_ref()
                    .and_then(|s| s.metadata.as_ref())
                    .map(|m| m.attempt.saturating_add(1))
                    .unwrap_or(old_restart);
                config.metadata.as_mut().expect("metadata").attempt = attempt;
                config.log_path = format!("{container_name}-{attempt}.log");
                let env = inputs::env(agent, p, c).await?;
                let vars: BTreeMap<_, _> = env
                    .iter()
                    .map(|e| (e.key.clone(), e.value.clone()))
                    .collect();
                config.command = config
                    .command
                    .iter()
                    .map(|s| inputs::expand(s, &vars))
                    .collect();
                config.args = config
                    .args
                    .iter()
                    .map(|s| inputs::expand(s, &vars))
                    .collect();
                config.envs = env;
                pull(cri, c, &sandbox).await?;
                if let Some(s) = &current {
                    stop(cri, &s.id, p).await?;
                }
                let id = rpc!(cri.runtime().create_container(CreateContainerRequest {
                    pod_sandbox_id: sandbox_id.clone(),
                    config: Some(config),
                    sandbox_config: Some(sandbox.clone())
                }))
                .container_id;
                rpc!(cri.runtime().start_container(StartContainerRequest {
                    container_id: id.clone()
                }));
                current = rpc!(cri.runtime().container_status(ContainerStatusRequest {
                    container_id: id,
                    verbose: false
                }))
                .status;
            } else if current
                .as_ref()
                .is_some_and(|s| same && s.state == ContainerState::ContainerCreated as i32)
            {
                let id = current.as_ref().expect("created").id.clone();
                rpc!(cri.runtime().start_container(StartContainerRequest {
                    container_id: id.clone()
                }));
                current = rpc!(cri.runtime().container_status(ContainerStatusRequest {
                    container_id: id,
                    verbose: false
                }))
                .status;
            }
            let Some(s) = current else {
                return Err(invalid("runtime omitted container status"));
            };
            // Retain the latest observation for recovery/restart counts; reclaim
            // superseded containers only after a replacement is observed.
            for old in matches {
                if old.id != s.id {
                    stop(cri, &old.id, p).await?;
                    rpc!(cri.runtime().remove_container(RemoveContainerRequest {
                        container_id: old.id.clone()
                    }));
                }
            }
            let running = s.state == ContainerState::ContainerRunning as i32;
            let terminated = s.state == ContainerState::ContainerExited as i32;
            let ready = running && probes.ready(cri, &s.id, c, &ip, s.started_at).await?;
            any_running |= running;
            all_ready &= ready;
            all_terminated &= terminated;
            all_success &= terminated && s.exit_code == 0;
            let state = if running {
                json!({"running":{"startedAt":stamp(s.started_at)}})
            } else if terminated {
                json!({"terminated":{"exitCode":s.exit_code,"reason":if s.exit_code==0{"Completed"}else{"Error"},"startedAt":stamp(s.started_at),"finishedAt":stamp(s.finished_at),"containerID":format!("containerd://{}",s.id)}})
            } else {
                json!({"waiting":{"reason":"ContainerCreating"}})
            };
            statuses.push(json!({"name":container_name,"image":c["image"],"imageID":s.image_ref,"containerID":format!("containerd://{}",s.id),"ready":ready,"started":running,"restartCount":s.metadata.map(|m|m.attempt).unwrap_or(old_restart),"state":state}));
        }
        let policy = p["spec"]["restartPolicy"].as_str().unwrap_or("Always");
        let phase = if all_terminated && policy != "Always" && (policy == "Never" || all_success) {
            if all_success {
                "Succeeded"
            } else {
                "Failed"
            }
        } else if any_running || all_terminated {
            "Running"
        } else {
            "Pending"
        };
        let mut status = json!({"phase":phase,"hostIP":agent.ip.to_string(),"containerStatuses":statuses,"startTime":p["status"]["startTime"].as_str().map(str::to_owned).unwrap_or_else(now)});
        if !ip.is_empty() {
            status["podIP"] = json!(ip);
            status["podIPs"] = json!([{"ip":ip}]);
        }
        conditions(&mut status, p, all_ready, true);
        Ok(status)
    }
}
async fn pull(cri: &Cri, c: &Value, sandbox: &PodSandboxConfig) -> Result<()> {
    let image = ImageSpec {
        image: pod::text(c, "image")?.into(),
        ..Default::default()
    };
    let policy = c["imagePullPolicy"].as_str().unwrap_or("IfNotPresent");
    if policy != "Always"
        && rpc!(cri.images().image_status(ImageStatusRequest {
            image: Some(image.clone()),
            verbose: false
        }))
        .image
        .is_some()
    {
        return Ok(());
    }
    if policy == "Never" {
        return Err(invalid("image is absent and pull policy is Never"));
    }
    rpc!(cri.images().pull_image(PullImageRequest {
        image: Some(image),
        sandbox_config: Some(sandbox.clone()),
        ..Default::default()
    }));
    Ok(())
}
fn retry_due(s: &ContainerStatus) -> bool {
    let attempt = s.metadata.as_ref().map(|m| m.attempt).unwrap_or(0);
    let delay = 10u64.saturating_mul(1u64 << attempt.min(5)).min(300);
    time::OffsetDateTime::now_utc().unix_timestamp_nanos() - i128::from(s.finished_at)
        >= i128::from(delay) * 1_000_000_000
}
fn should_restart(s: Option<&ContainerStatus>, same: bool, policy: &str) -> bool {
    match s {
        None => true,
        Some(_) if !same => true,
        Some(s) if s.state == ContainerState::ContainerExited as i32 => {
            policy == "Always" || (policy == "OnFailure" && s.exit_code != 0)
        }
        Some(s) => s.state == ContainerState::ContainerUnknown as i32,
    }
}
async fn stop(cri: &Cri, id: &str, p: &Value) -> Result<()> {
    rpc!(cri.runtime().stop_container(StopContainerRequest {
        container_id: id.into(),
        timeout: p["spec"]["terminationGracePeriodSeconds"]
            .as_i64()
            .unwrap_or(30)
            .clamp(0, 30)
    }));
    Ok(())
}
async fn remove(cri: &Cri, s: &PodSandbox, node: &str) -> Result<()> {
    if !pod::owned(&s.labels, node) {
        return Err(invalid("refusing foreign sandbox cleanup"));
    }
    let containers = rpc!(cri.runtime().list_containers(ListContainersRequest {
        filter: Some(ContainerFilter {
            pod_sandbox_id: s.id.clone(),
            ..Default::default()
        })
    }))
    .containers;
    for c in &containers {
        if !pod::owned(&c.labels, node) || c.labels.get(pod::UID) != s.labels.get(pod::UID) {
            return Err(invalid("sandbox contains foreign container"));
        }
    }
    for c in containers {
        stop(cri, &c.id, &json!({})).await?;
        rpc!(cri
            .runtime()
            .remove_container(RemoveContainerRequest { container_id: c.id }));
    }
    rpc!(cri.runtime().stop_pod_sandbox(StopPodSandboxRequest {
        pod_sandbox_id: s.id.clone()
    }));
    rpc!(cri.runtime().remove_pod_sandbox(RemovePodSandboxRequest {
        pod_sandbox_id: s.id.clone()
    }));
    Ok(())
}
fn conditions(status: &mut Value, p: &Value, ready: bool, initialized: bool) {
    let now = now();
    let values:Vec<_> = [("PodScheduled",true),("Initialized",initialized),("ContainersReady",ready),("Ready",ready)]
        .into_iter().map(|(kind,ok)|{
            let value=if ok{"True"}else{"False"};
            let old=p["status"]["conditions"].as_array().into_iter().flatten()
                .find(|c|c["type"]==kind&&c["status"]==value)
                .and_then(|c|c["lastTransitionTime"].as_str()).unwrap_or(&now);
            json!({"type":kind,"status":value,"lastTransitionTime":old,"reason":if ok{"KubeletObservedReady"}else{"ContainersNotReady"}})
        }).collect();
    status["conditions"] = json!(values);
}

async fn publish(agent: &Agent, p: &Value, status: Value) -> Result<()> {
    if p["status"] == status {
        return Ok(());
    }
    let mut updated = p.clone();
    updated["status"] = status;
    let path = format!(
        "api/v1/namespaces/{}/pods/{}/status",
        pod::text(&p["metadata"], "namespace")?,
        pod::text(&p["metadata"], "name")?
    );
    let (code, _) = agent.request(Method::PUT, &path, Some(updated)).await?;
    if !matches!(code, 200 | 404 | 409) {
        return Err(Error::Status(code));
    }
    Ok(())
}
fn stamp(ns: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ns))
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .format(&time::format_description::well_known::Rfc3339)
        .expect("UTC")
}
fn dns(p: &Value) -> Result<DnsConfig> {
    if p["spec"]["dnsPolicy"] == "None" {
        let c = &p["spec"]["dnsConfig"];
        let mut options = vec![];
        for option in c["options"].as_array().into_iter().flatten() {
            pod::fields(option, &["name", "value"])?;
            let name = pod::text(option, "name")?;
            options.push(if let Some(value) = option["value"].as_str() {
                format!("{name}:{value}")
            } else {
                name.into()
            });
        }
        return Ok(DnsConfig {
            servers: pod::strings(&c["nameservers"])?,
            searches: pod::strings(&c["searches"])?,
            options,
        });
    }
    let text = std::fs::read_to_string("/etc/resolv.conf")?;
    let mut config = DnsConfig::default();
    for line in text.lines() {
        let mut words = line.split('#').next().unwrap_or("").split_whitespace();
        match words.next() {
            Some("nameserver") => {
                if let Some(s) = words.next() {
                    config.servers.push(s.into());
                }
            }
            Some("search") => config.searches = words.map(str::to_owned).collect(),
            Some("options") => config.options.extend(words.map(str::to_owned)),
            _ => {}
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn restart_policy_adopts_created_running_and_terminal_containers() {
        for state in [
            ContainerState::ContainerCreated,
            ContainerState::ContainerRunning,
        ] {
            let s = ContainerStatus {
                state: state as i32,
                ..Default::default()
            };
            assert!(!should_restart(Some(&s), true, "Always"));
            assert!(should_restart(Some(&s), false, "Always"));
        }
        for (code, policy, want) in [
            (0, "Never", false),
            (1, "Never", false),
            (0, "OnFailure", false),
            (1, "OnFailure", true),
            (0, "Always", true),
            (1, "Always", true),
        ] {
            let s = ContainerStatus {
                state: ContainerState::ContainerExited as i32,
                exit_code: code,
                ..Default::default()
            };
            assert_eq!(should_restart(Some(&s), true, policy), want);
        }
        let recent = ContainerStatus {
            state: ContainerState::ContainerExited as i32,
            finished_at: time::OffsetDateTime::now_utc().unix_timestamp_nanos() as i64,
            ..Default::default()
        };
        assert!(!retry_due(&recent));
        let old = ContainerStatus {
            finished_at: recent.finished_at - 11_000_000_000,
            ..recent
        };
        assert!(retry_due(&old));
    }
    #[test]
    fn readiness_transition_time_changes_only_with_condition_status() {
        let p = json!({"status":{"conditions":[{"type":"Ready","status":"True","lastTransitionTime":"2026-09-08T00:00:00Z"}]}});
        let mut status = json!({});
        conditions(&mut status, &p, true, true);
        assert_eq!(
            status["conditions"][3]["lastTransitionTime"],
            "2026-09-08T00:00:00Z"
        );
        conditions(&mut status, &p, false, false);
        assert_eq!(status["conditions"][1]["status"], "False");
        assert_ne!(
            status["conditions"][3]["lastTransitionTime"],
            "2026-09-08T00:00:00Z"
        );
    }
}
