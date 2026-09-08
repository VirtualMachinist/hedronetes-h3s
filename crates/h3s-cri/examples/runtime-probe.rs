//! Direct CRI integration fixture, not a Kubernetes workload acceptance test.
//! Arguments: UNIX_ENDPOINT PRIVATE_LOG_ROOT IMAGE@sha256:DIGEST
use h3s_cri::{v1::*, Cri};
use serde_json::json;
use std::{collections::HashMap, path::PathBuf, time::Duration};
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const LABEL: &str = "hedronetes.io/runtime-probe";
fn require(value: bool, message: &'static str) -> Result<()> {
    if value {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn rpc<T>(value: std::result::Result<tonic::Response<T>, tonic::Status>) -> Result<T> {
    Ok(value.map_err(h3s_cri::Error::from)?.into_inner())
}
async fn exercise(cri: &Cri, id: &str, root: &std::path::Path, image: &str) -> Result<()> {
    let labels = HashMap::from([(LABEL.to_string(), id.to_string())]);
    let version = cri.version();
    println!(
        "{}",
        json!({"run_id":id,"runtime":version.runtime_name,"version":version.runtime_version,"api":version.runtime_api_version})
    );
    let status = rpc(cri.runtime().status(StatusRequest { verbose: false }).await)?
        .status
        .ok_or("missing runtime status")?;
    for kind in ["RuntimeReady", "NetworkReady"] {
        require(
            status
                .conditions
                .iter()
                .any(|c| c.r#type == kind && c.status),
            "runtime/network is not ready",
        )?;
    }
    let mut builder = std::fs::DirBuilder::new();
    use std::os::unix::fs::DirBuilderExt;
    builder.recursive(true).mode(0o700).create(root)?;
    let sandbox = PodSandboxConfig {
        metadata: Some(PodSandboxMetadata {
            name: format!("h3s-cri-{id}"),
            uid: id.into(),
            namespace: "h3s-runtime-probe".into(),
            attempt: 0,
        }),
        hostname: "h3s-cri-probe".into(),
        log_directory: root.to_str().ok_or("non UTF-8 log root")?.into(),
        labels: labels.clone(),
        linux: Some(LinuxPodSandboxConfig {
            cgroup_parent: "h3s-m1.slice".into(),
            security_context: Some(LinuxSandboxSecurityContext {
                namespace_options: Some(NamespaceOption {
                    network: NamespaceMode::Pod as i32,
                    pid: NamespaceMode::Pod as i32,
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
    let pulled = rpc(cri
        .images()
        .pull_image(PullImageRequest {
            image: Some(ImageSpec {
                image: image.into(),
                ..Default::default()
            }),
            sandbox_config: Some(sandbox.clone()),
            ..Default::default()
        })
        .await)?;
    require(!pulled.image_ref.is_empty(), "empty pulled image ID")?;
    let pod = rpc(cri
        .runtime()
        .run_pod_sandbox(RunPodSandboxRequest {
            config: Some(sandbox.clone()),
            runtime_handler: "youki".into(),
        })
        .await)?
    .pod_sandbox_id;
    let pod_status = rpc(cri
        .runtime()
        .pod_sandbox_status(PodSandboxStatusRequest {
            pod_sandbox_id: pod.clone(),
            verbose: false,
        })
        .await)?
    .status
    .ok_or("missing sandbox status")?;
    require(
        pod_status.state == PodSandboxState::SandboxReady as i32,
        "sandbox not ready",
    )?;
    let ip = pod_status.network.ok_or("missing sandbox network")?.ip;
    require(
        ip.starts_with("10.42.2."),
        "sandbox did not receive a project CNI address",
    )?;
    let marker = format!("h3s-cri-{id}");
    let config = ContainerConfig {
        metadata: Some(ContainerMetadata {
            name: "fixture".into(),
            attempt: 0,
        }),
        image: Some(ImageSpec {
            image: image.into(),
            ..Default::default()
        }),
        command: vec!["/bin/sh".into(), "-c".into()],
        args: vec![format!(
            "echo {marker}; trap 'exit 0' TERM; while :; do sleep 1; done"
        )],
        working_dir: "/".into(),
        labels: labels.clone(),
        log_path: "fixture.log".into(),
        linux: Some(LinuxContainerConfig {
            resources: Some(LinuxContainerResources {
                memory_limit_in_bytes: 64 * 1024 * 1024,
                cpu_period: 100000,
                cpu_quota: 10000,
                ..Default::default()
            }),
            security_context: Some(LinuxContainerSecurityContext {
                run_as_user: Some(Int64Value { value: 65534 }),
                no_new_privs: true,
                readonly_rootfs: true,
                capabilities: Some(Capability {
                    add_capabilities: vec![],
                    drop_capabilities: vec!["ALL".into()],
                    ..Default::default()
                }),
                seccomp: Some(SecurityProfile {
                    profile_type: security_profile::ProfileType::RuntimeDefault as i32,
                    ..Default::default()
                }),
                ..Default::default()
            }),
        }),
        ..Default::default()
    };
    let container = rpc(cri
        .runtime()
        .create_container(CreateContainerRequest {
            pod_sandbox_id: pod.clone(),
            config: Some(config),
            sandbox_config: Some(sandbox),
        })
        .await)?
    .container_id;
    rpc(cri
        .runtime()
        .start_container(StartContainerRequest {
            container_id: container.clone(),
        })
        .await)?;
    let observed = rpc(cri
        .runtime()
        .container_status(ContainerStatusRequest {
            container_id: container.clone(),
            verbose: true,
        })
        .await)?;
    require(
        observed
            .status
            .as_ref()
            .is_some_and(|s| s.state == ContainerState::ContainerRunning as i32),
        "container not running",
    )?;
    // This fixture deliberately targets containerd. Its verbose CRI info supplies
    // the real task PID; kernel files verify applied limits, not merely OCI input.
    let info: serde_json::Value = serde_json::from_str(
        observed
            .info
            .get("info")
            .ok_or("missing containerd task info")?,
    )?;
    let pid = info["pid"]
        .as_u64()
        .filter(|p| *p > 1)
        .ok_or("missing runtime task PID")?;
    let proc_status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
    let groups = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    let group = groups
        .lines()
        .find_map(|line| line.strip_prefix("0::/"))
        .ok_or("container is not in cgroup v2")?;
    require(
        group.split('/').any(|part| part == "h3s-m1.slice")
            && group.contains(&container)
            && !group.split('/').any(|p| p == ".."),
        "container was not placed in its project cgroup",
    )?;
    let cgroup = std::path::Path::new("/sys/fs/cgroup").join(group);
    let memory = std::fs::read_to_string(cgroup.join("memory.max"))?;
    let cpu = std::fs::read_to_string(cgroup.join("cpu.max"))?;
    println!(
        "{}",
        json!({"run_id":id,"container":container,"pid":pid,"cgroup":group,"memory_max":memory.trim(),"cpu_max":cpu.trim()})
    );
    require(memory.trim() == "67108864", "memory limit was not applied")?;
    require(cpu.trim() == "10000 100000", "CPU quota was not applied")?;
    require(
        proc_status
            .lines()
            .any(|s| s.starts_with("Seccomp:") && s.ends_with('2')),
        "init process seccomp filter missing",
    )?;
    let executed = rpc(cri
        .runtime()
        .exec_sync(ExecSyncRequest {
            container_id: container.clone(),
            cmd: vec![
                "/bin/sh".into(),
                "-c".into(),
                "id -u; grep -E '^Cap(Eff|Bnd)|^NoNewPrivs|^Seccomp:' /proc/self/status".into(),
            ],
            timeout: 5,
        })
        .await)?;
    let stdout = String::from_utf8(executed.stdout)?;
    println!(
        "{}",
        json!({"run_id":id,"container":container,"exec_stdout":stdout,"exec_exit_code":executed.exit_code})
    );
    require(
        executed.exit_code == 0 && stdout.starts_with("65534\n"),
        "exec did not run with configured non-root UID",
    )?;
    require(
        stdout
            .lines()
            .any(|s| s.starts_with("CapEff:") && s.ends_with("0000000000000000")),
        "effective capabilities were not dropped",
    )?;
    require(
        stdout
            .lines()
            .any(|s| s.starts_with("NoNewPrivs:") && s.ends_with('1')),
        "no-new-privileges missing",
    )?;
    require(
        stdout
            .lines()
            .any(|s| s.starts_with("Seccomp:") && s.ends_with('2')),
        "seccomp filter missing",
    )?;
    let log = root.join("fixture.log");
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if std::fs::read_to_string(&log).is_ok_and(|s| s.contains(&marker)) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await?;
    println!(
        "{}",
        json!({"run_id":id,"sandbox":pod,"container":container,"sandbox_ip":ip,"image":image,"image_id":pulled.image_ref,"container_running":true,"exec_stdout":stdout,"log_marker_verified":true,"runtime_handler":"youki"})
    );
    Ok(())
}
async fn cleanup(cri: &Cri, id: &str) -> Result<()> {
    // An RPC may have committed before a timeout. Find only this invocation's
    // exact label to reclaim any such resources, never all runtime workloads.
    let labels = HashMap::from([(LABEL.to_string(), id.to_string())]);
    let containers = rpc(cri
        .runtime()
        .list_containers(ListContainersRequest {
            filter: Some(ContainerFilter {
                label_selector: labels.clone(),
                ..Default::default()
            }),
        })
        .await)?
    .containers;
    for c in containers {
        require(
            c.labels.get(LABEL).is_some_and(|v| v == id),
            "runtime returned foreign container",
        )?;
        rpc(cri
            .runtime()
            .stop_container(StopContainerRequest {
                container_id: c.id.clone(),
                timeout: 3,
            })
            .await)?;
        rpc(cri
            .runtime()
            .remove_container(RemoveContainerRequest { container_id: c.id })
            .await)?;
    }
    let filter = PodSandboxFilter {
        label_selector: labels,
        ..Default::default()
    };
    let pods = rpc(cri
        .runtime()
        .list_pod_sandbox(ListPodSandboxRequest {
            filter: Some(filter.clone()),
        })
        .await)?
    .items;
    for pod in pods {
        require(
            pod.labels.get(LABEL).is_some_and(|v| v == id),
            "runtime returned foreign sandbox",
        )?;
        rpc(cri
            .runtime()
            .stop_pod_sandbox(StopPodSandboxRequest {
                pod_sandbox_id: pod.id.clone(),
            })
            .await)?;
        rpc(cri
            .runtime()
            .remove_pod_sandbox(RemovePodSandboxRequest {
                pod_sandbox_id: pod.id,
            })
            .await)?;
    }
    require(
        rpc(cri
            .runtime()
            .list_pod_sandbox(ListPodSandboxRequest {
                filter: Some(filter),
            })
            .await)?
        .items
        .is_empty(),
        "sandbox cleanup incomplete",
    )?;
    println!(
        "{}",
        json!({"run_id":id,"cleanup":"passed","images_retained":true})
    );
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    require(
        args.len() == 3,
        "usage: runtime-probe UNIX_ENDPOINT PRIVATE_LOG_ROOT IMAGE@sha256:DIGEST",
    )?;
    let root = PathBuf::from(&args[1]);
    require(root.is_absolute(), "log root must be absolute")?;
    let (image, digest) = args[2]
        .split_once("@sha256:")
        .ok_or("image must be pinned by digest")?;
    require(
        !image.contains('@')
            && !image.contains("://")
            && digest.len() == 64
            && digest.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid pinned image",
    )?;
    let cri = Cri::connect(&args[0]).await?;
    let id = uuid::Uuid::new_v4().to_string();
    let result = exercise(&cri, &id, &root.join(&id), &args[2]).await;
    let cleaned = cleanup(&cri, &id).await;
    if let Err(error) = &cleaned {
        eprintln!("runtime-probe cleanup failed for {id}: {error}");
    }
    result?;
    cleaned?;
    println!(
        "{}",
        json!({"run_id":id,"outcome":"passed","kubernetes_workload_acceptance":false})
    );
    Ok(())
}
