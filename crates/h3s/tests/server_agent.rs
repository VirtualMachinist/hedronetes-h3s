//! Real binary composition and crash recovery. No CRI is supplied: these tests
//! require truthful NotReady and do not claim container/network acceptance.
use k8s_openapi::api::{coordination::v1::Lease, core::v1::Node};
use kube::{api::ListParams, Api, Client};
use serde_json::Value;
use std::{
    fs,
    net::TcpListener,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::time::{sleep, timeout};

struct Process {
    child: Child,
    log: std::path::PathBuf,
}
impl Process {
    fn start(dir: &Path, args: &[String]) -> Self {
        let log = dir.join(format!(
            "process-{}.log",
            h3s_auth::bootstrap::random_secret().unwrap()
        ));
        let file = fs::File::create(&log).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_h3s"))
            .args(args)
            .env_remove("H3S_TOKEN")
            .stdin(Stdio::null())
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .spawn()
            .unwrap();
        Self { child, log }
    }
    fn assert_running(&mut self) {
        assert!(
            self.child.try_wait().unwrap().is_none(),
            "process exited: {}",
            fs::read_to_string(&self.log).unwrap()
        );
    }
    fn stop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
        }
        self.child.wait().unwrap();
    }
}
impl Drop for Process {
    fn drop(&mut self) {
        self.stop();
    }
}
fn port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}
fn server_args(dir: &Path, api_port: u16, kubelet_port: u16) -> Vec<String> {
    [
        "server".into(),
        "--data-dir".into(),
        dir.join("runtime").display().to_string(),
        "--write-kubeconfig".into(),
        dir.join("admin.kubeconfig").display().to_string(),
        "--bind-address".into(),
        "127.0.0.1".into(),
        "--https-listen-port".into(),
        api_port.to_string(),
        "--node-name".into(),
        "server-node".into(),
        "--node-ip".into(),
        "192.0.2.10".into(),
        "--kubelet-port".into(),
        kubelet_port.to_string(),
    ]
    .into()
}
async fn client(dir: &Path, process: &mut Process) -> Client {
    timeout(Duration::from_secs(30), async {
        loop {
            process.assert_running();
            if let Ok(config) = fs::read_to_string(dir.join("admin.kubeconfig")) {
                let client = h3s_controllers::client_from_kubeconfig(&config)
                    .await
                    .unwrap();
                if Api::<Node>::all(client.clone())
                    .list(&ListParams::default())
                    .await
                    .is_ok()
                {
                    return client;
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("API startup timed out")
}
async fn node(client: &Client, process: &mut Process, name: &str, ip: &str) -> Node {
    timeout(Duration::from_secs(60), async {
        loop {
            process.assert_running();
            if let Ok(node) = Api::<Node>::all(client.clone()).get(name).await {
                let lease = Api::<Lease>::namespaced(client.clone(), "kube-node-lease")
                    .get(name)
                    .await;
                let cidr = node.spec.as_ref().and_then(|s| s.pod_cidr.as_ref());
                let status = node.status.as_ref();
                let ready = status
                    .and_then(|s| s.conditions.as_ref())
                    .is_some_and(|cs| {
                        cs.iter().any(|c| {
                            c.type_ == "Ready"
                                && c.status == "False"
                                && c.reason.as_deref() == Some("RuntimeNotReady")
                        })
                    });
                let address = status
                    .and_then(|s| s.addresses.as_ref())
                    .is_some_and(|a| a.iter().any(|a| a.type_ == "InternalIP" && a.address == ip));
                let owner = lease
                    .ok()
                    .and_then(|l| l.metadata.owner_references)
                    .is_some_and(|os| {
                        os.iter()
                            .any(|o| Some(&o.uid) == node.metadata.uid.as_ref())
                    });
                if cidr.is_some() && ready && address && owner {
                    return node;
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("node registration/CIDR/Lease timed out")
}
async fn health(client: &Client, process: &mut Process, name: &str) {
    timeout(Duration::from_secs(60), async {
        loop {
            process.assert_running();
            let req = http::Request::get(format!("/api/v1/nodes/{name}/proxy/healthz"))
                .body(Vec::new())
                .unwrap();
            if client
                .request_text(req)
                .await
                .is_ok_and(|body| body == "ok\n")
            {
                break;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("supervisor-backed kubelet health timed out");
    let req = http::Request::get(format!("/api/v1/nodes/{name}/proxy/readyz"))
        .body(Vec::new())
        .unwrap();
    assert!(matches!(client.request_text(req).await, Err(kube::Error::Api(e)) if e.code == 503));
}

#[tokio::test]
async fn server_and_separate_worker_enroll_and_recover_with_retained_identities_and_cidrs() {
    let dir = tempfile::tempdir().unwrap();
    let worker_dir = tempfile::tempdir().unwrap();
    let args = server_args(dir.path(), port(), port());
    let mut server = Process::start(dir.path(), &args);
    let client = client(dir.path(), &mut server).await;
    let original = node(&client, &mut server, "server-node", "192.0.2.10").await;
    health(&client, &mut server, "server-node").await;
    let identity_path = dir.path().join("runtime/agent/identity.json");
    let identity = fs::read(&identity_path).unwrap();
    let serving = fs::read(dir.path().join("runtime/agent/serving.json")).unwrap();
    let enrollment: Value = serde_json::from_slice(&identity).unwrap();
    let mut worker = Process::start(
        worker_dir.path(),
        &[
            "agent".into(),
            "--server".into(),
            enrollment["server"].as_str().unwrap().into(),
            "--server-ca-file".into(),
            dir.path()
                .join("runtime/server/ca.crt")
                .display()
                .to_string(),
            "--token-file".into(),
            dir.path()
                .join("runtime/server/node-token")
                .display()
                .to_string(),
            "--data-dir".into(),
            worker_dir.path().join("runtime").display().to_string(),
            "--node-name".into(),
            "worker-node".into(),
            "--node-ip".into(),
            "192.0.2.11".into(),
            "--kubelet-port".into(),
            port().to_string(),
        ],
    );
    let original_worker = node(&client, &mut worker, "worker-node", "192.0.2.11").await;
    health(&client, &mut worker, "worker-node").await;
    assert_ne!(
        original.spec.as_ref().unwrap().pod_cidr,
        original_worker.spec.as_ref().unwrap().pod_cidr
    );
    let worker_identity_path = worker_dir.path().join("runtime/agent/identity.json");
    let worker_identity = fs::read(&worker_identity_path).unwrap();
    assert_ne!(identity, worker_identity);
    let ledger_request = || {
        http::Request::get(h3s_api::network::NODE_CIDR_PATH)
            .body(Vec::new())
            .unwrap()
    };
    let ledger: Value = client.request(ledger_request()).await.unwrap();
    assert_eq!(ledger["reservations"].as_object().unwrap().len(), 2);

    server.stop(); // Crash, then reopen the exact registry, PKI and local identity.
    server = Process::start(dir.path(), &args);
    let restarted = node(&client, &mut server, "server-node", "192.0.2.10").await;
    health(&client, &mut server, "server-node").await;
    health(&client, &mut worker, "worker-node").await;
    assert_eq!(original.metadata.uid, restarted.metadata.uid);
    assert_eq!(
        original.spec.as_ref().unwrap().pod_cidr,
        restarted.spec.as_ref().unwrap().pod_cidr
    );
    assert_eq!(identity, fs::read(&identity_path).unwrap());
    assert_eq!(
        serving,
        fs::read(dir.path().join("runtime/agent/serving.json")).unwrap()
    );
    assert_eq!(worker_identity, fs::read(&worker_identity_path).unwrap());
    assert_eq!(
        ledger,
        client.request::<Value>(ledger_request()).await.unwrap()
    );

    // A deleted local Node must be recreated by the embedded heartbeat, with
    // the name's durable subnet and a Lease referring to its new UID.
    Api::<Node>::all(client.clone())
        .delete("server-node", &Default::default())
        .await
        .unwrap();
    let replacement = node(&client, &mut server, "server-node", "192.0.2.10").await;
    assert_ne!(original.metadata.uid, replacement.metadata.uid);
    assert_eq!(
        original.spec.unwrap().pod_cidr,
        replacement.spec.unwrap().pod_cidr
    );
    assert_eq!(identity, fs::read(identity_path).unwrap());
}

#[tokio::test]
async fn disable_agent_serves_api_without_node_identity_or_local_listener() {
    let dir = tempfile::tempdir().unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut args = server_args(dir.path(), port(), occupied.local_addr().unwrap().port());
    args.push("--disable-agent".into());
    // Local identity inputs are irrelevant when the local agent is disabled.
    let pos = args.iter().position(|v| v == "--node-name").unwrap();
    args[pos + 1] = "INVALID NAME".into();
    let mut server = Process::start(dir.path(), &args);
    let client = client(dir.path(), &mut server).await;
    assert!(Api::<Node>::all(client)
        .list(&ListParams::default())
        .await
        .unwrap()
        .items
        .is_empty());
    assert!(!dir.path().join("runtime/agent").exists());
    server.assert_running();
}

#[tokio::test]
async fn local_agent_listener_failure_terminates_the_composed_server() {
    let dir = tempfile::tempdir().unwrap();
    let occupied = TcpListener::bind("127.0.0.1:0").unwrap();
    let args = server_args(dir.path(), port(), occupied.local_addr().unwrap().port());
    let mut server = Process::start(dir.path(), &args);
    let exit = timeout(Duration::from_secs(40), async {
        loop {
            if let Some(exit) = server.child.try_wait().unwrap() {
                return exit;
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("failed local agent left the API process running");
    assert!(!exit.success());
    let log = fs::read_to_string(&server.log).unwrap();
    assert!(log.contains("agent I/O"), "{log}");
}
