//! Hedronetes (`h3s`) multicall binary.
//!
//! API foundation with persistent PKI/storage. The node runtime is in progress.

use clap::{Args, Parser, Subcommand};

/// Hedronetes (h3s) — Kubernetes-compatible cluster distribution in one binary.
#[derive(Debug, Parser)]
#[command(
    name = "h3s",
    version,
    about = "Hedronetes (h3s): Kubernetes-compatible cluster distribution in one Rust binary",
    long_about = "k3s, written in Rust, without embedding a Go control plane.\n\n\
         API foundation: use server --disable-agent. \
         Agents enroll with a supervisor tunnel; an explicit local CRI endpoint enables Pod reconciliation.",
    multicall = true,
    subcommand_required = true,
    arg_required_else_help = true,
    subcommand_value_name = "COMMAND",
    subcommand_help_heading = "Commands",
    propagate_version = true
)]
enum Multicall {
    /// Hedronetes (h3s): Kubernetes-compatible cluster distribution in one Rust binary
    H3s(H3sCli),
    /// Start the control plane + datastore + supervisor (embedded agent unless disabled).
    Server(ServerArgs),
    /// Enroll a worker and maintain Node/Lease status (workload runtime incomplete).
    Agent(AgentArgs),
    /// Inspect the configured local CRI v1 runtime without changing workloads.
    RuntimeInfo(RuntimeArgs),
}

#[derive(Debug, Parser)]
#[command(
    name = "h3s",
    version,
    about = "Hedronetes (h3s): Kubernetes-compatible cluster distribution in one Rust binary",
    arg_required_else_help = true,
    subcommand_value_name = "COMMAND",
    subcommand_help_heading = "Commands",
    propagate_version = true
)]
struct H3sCli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the control plane + datastore + supervisor (embedded agent unless disabled).
    Server(ServerArgs),
    /// Enroll a worker and maintain Node/Lease status (workload runtime incomplete).
    Agent(AgentArgs),
    /// Inspect the configured local CRI v1 runtime without changing workloads.
    RuntimeInfo(RuntimeArgs),
}

/// Server configuration; runtime data is isolated from companion stores.
#[derive(Debug, Args)]
struct ServerArgs {
    #[arg(long, default_value = "/var/lib/hedronetes")]
    data_dir: std::path::PathBuf,
    #[arg(long, default_value = "0.0.0.0")]
    bind_address: std::net::IpAddr,
    #[arg(long, default_value_t = 6443)]
    https_listen_port: u16,
    #[arg(long)]
    tls_san: Vec<String>,
    #[arg(long, default_value = "/etc/hedronetes/h3s.yaml")]
    write_kubeconfig: std::path::PathBuf,
    /// Run the API without a local agent (required while node runtime is incomplete).
    #[arg(long)]
    disable_agent: bool,
    /// Shared enrollment token; prefer --token-file over a command-line value.
    #[arg(
        long,
        env = "H3S_TOKEN",
        hide_env_values = true,
        conflicts_with = "token_file"
    )]
    token: Option<h3s_auth::bootstrap::Token>,
    #[arg(long)]
    token_file: Option<std::path::PathBuf>,
}

/// Native worker enrollment and lifecycle; runtime readiness is explicit.
#[derive(Debug, Args)]
struct AgentArgs {
    /// Private loopback kubelet listener; never binds a reachable interface.
    #[arg(long,default_value_t=10250,value_parser=clap::value_parser!(u16).range(1..))]
    kubelet_port: u16,
    /// Use an operator-configured local CRI v1 runtime for assigned Pods.
    #[arg(long)]
    container_runtime_endpoint: Option<String>,
    #[arg(long)]
    server: String,
    /// Trusted CA copied through an authenticated operator channel.
    #[arg(long)]
    server_ca_file: std::path::PathBuf,
    #[arg(long)]
    node_name: String,
    #[arg(long)]
    node_ip: std::net::IpAddr,
    #[arg(long, default_value = "/var/lib/hedronetes")]
    data_dir: std::path::PathBuf,
    #[arg(
        long,
        env = "H3S_TOKEN",
        hide_env_values = true,
        conflicts_with = "token_file"
    )]
    token: Option<h3s_auth::bootstrap::Token>,
    #[arg(long)]
    token_file: Option<std::path::PathBuf>,
}

#[derive(Debug, Args)]
struct RuntimeArgs {
    #[arg(
        long,
        default_value = "unix:///run/hedronetes/containerd/containerd.sock"
    )]
    container_runtime_endpoint: String,
}
async fn runtime_info(args: RuntimeArgs) -> RunResult {
    let cri = h3s_cri::Cri::connect(&args.container_runtime_endpoint).await?;
    let status = cri
        .runtime()
        .status(h3s_cri::v1::StatusRequest { verbose: false })
        .await
        .map_err(h3s_cri::Error::from)?
        .into_inner();
    let conditions: Vec<_> = status.status.ok_or("CRI returned no runtime status")?.conditions.into_iter()
        .map(|c| serde_json::json!({"type":c.r#type,"status":c.status,"reason":c.reason,"message":c.message})).collect();
    let v = cri.version();
    println!(
        "{}",
        serde_json::json!({"runtime_name":v.runtime_name,"runtime_version":v.runtime_version,"runtime_api_version":v.runtime_api_version,"conditions":conditions})
    );
    Ok(())
}

fn install_rustls_provider() {
    // rustls 0.23 default crypto is aws-lc-rs. OpenSSL is not a default feature.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

type RunResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

async fn run_server(args: ServerArgs) -> RunResult {
    if !args.disable_agent {
        return Err(
            "node runtime is not implemented; use --disable-agent for the API foundation".into(),
        );
    }
    let server_dir = args.data_dir.join("server");
    let mut sans = vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "kubernetes".into(),
        "kubernetes.default".into(),
        "kubernetes.default.svc".into(),
        "kubernetes.default.svc.cluster.local".into(),
        "10.43.0.1".into(),
    ];
    if !args.bind_address.is_unspecified() {
        sans.push(args.bind_address.to_string());
    }
    sans.extend(args.tls_san);
    sans.sort();
    sans.dedup();
    let pki = std::sync::Arc::new(h3s_certs::ClusterPki::open_or_create(
        &server_dir.join("tls"),
        &sans,
    )?);
    let token = read_token(
        args.token.as_ref(),
        args.token_file.as_deref(),
        Some(&server_dir),
    )?
    .expect("server token generated");
    let ca_file = server_dir.join("ca.crt");
    if ca_file.try_exists()? {
        if h3s_certs::private::read(&ca_file, 1024 * 1024)? != pki.ca_pem().as_bytes() {
            return Err("existing CA export differs from cluster PKI".into());
        }
    } else {
        h3s_certs::private::write(&ca_file, pki.ca_pem().as_bytes(), false)?;
    }
    let db_dir = server_dir.join("db");
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(&db_dir)?;
    let metadata = std::fs::symlink_metadata(&db_dir)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("registry directory must not be a symlink".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err("registry directory requires mode 0700".into());
        }
    }
    let store = std::sync::Arc::new(h3s_storage::SqliteStore::open(db_dir.join("h3s.db")).await?);
    let api = h3s_apiserver::Api::new(store)
        .await?
        .with_bootstrap(pki.clone(), &token)?;
    let listener =
        tokio::net::TcpListener::bind((args.bind_address, args.https_listen_port)).await?;
    let local = listener.local_addr()?;
    let connect_ip = if args.bind_address.is_unspecified() {
        "127.0.0.1".parse().unwrap()
    } else {
        args.bind_address
    };
    let endpoint = format!(
        "https://{}",
        std::net::SocketAddr::new(connect_ip, local.port())
    );
    let config = pki.kubeconfig(&endpoint, pki.admin())?;
    write_kubeconfig(&args.write_kubeconfig, &config)?;
    eprintln!(
        "h3s API listening on https://{local}; kubeconfig: {}",
        args.write_kubeconfig.display()
    );
    let identity = pki.issue_client(h3s_controllers::NAMESPACE_CONTROLLER_ID, None)?;
    let controller_config = pki.kubeconfig(&endpoint, &identity)?;
    let client = h3s_controllers::client_from_kubeconfig(&controller_config).await?;
    let scheduler_identity = pki.issue_client(h3s_scheduler::SCHEDULER_ID, None)?;
    let scheduler_config = pki.kubeconfig(&endpoint, &scheduler_identity)?;
    let scheduler_client = h3s_controllers::client_from_kubeconfig(&scheduler_config).await?;
    let mut workload_clients = Vec::new();
    for identity in [
        h3s_controllers::DEPLOYMENT_CONTROLLER_ID,
        h3s_controllers::REPLICASET_CONTROLLER_ID,
        h3s_controllers::WORKLOAD_GC_ID,
        h3s_controllers::ENDPOINT_CONTROLLER_ID,
    ] {
        let identity = pki.issue_client(identity, None)?;
        let config = pki.kubeconfig(&endpoint, &identity)?;
        workload_clients.push(h3s_controllers::client_from_kubeconfig(&config).await?);
    }
    let endpoint_client = workload_clients.pop().unwrap();
    let gc_client = workload_clients.pop().unwrap();
    let rs_client = workload_clients.pop().unwrap();
    let deployment_client = workload_clients.pop().unwrap();
    let server = h3s_apiserver::serve(listener, pki.server_config()?, api.router(), async {
        let _ = tokio::signal::ctrl_c().await;
    });
    tokio::select! {
        result = server => result?,
        result = h3s_controllers::run_endpoint_controller(endpoint_client) => result?,
        result = h3s_controllers::run_deployment_controller(deployment_client) => result?,
        result = h3s_controllers::run_replicaset_controller(rs_client) => result?,
        result = h3s_controllers::run_workload_gc(gc_client) => result?,
        result = h3s_scheduler::run(scheduler_client) => result?,
        result = h3s_controllers::run_namespace_controller(client, pki.ca_pem().to_owned()) => result?,
    }
    Ok(())
}
fn write_kubeconfig(path: &std::path::Path, contents: &str) -> RunResult {
    use std::io::Write;
    if path.try_exists()? {
        let metadata = std::fs::symlink_metadata(path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err("kubeconfig must be a regular file".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err("kubeconfig requires mode 0600".into());
            }
        }
        if std::fs::read_to_string(path)? == contents {
            return Ok(());
        }
        return Err("refusing to overwrite a different kubeconfig; choose a project-owned --write-kubeconfig path".into());
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(contents.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}
fn read_token(
    token: Option<&h3s_auth::bootstrap::Token>,
    file: Option<&std::path::Path>,
    server: Option<&std::path::Path>,
) -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let text = if let Some(token) = token {
        Some(token.expose().to_owned())
    } else if let Some(file) = file {
        Some(
            String::from_utf8(h3s_certs::private::read(file, 1024)?)
                .map_err(|_| "token file is not UTF-8")?
                .trim()
                .to_owned(),
        )
    } else if let Some(dir) = server {
        let _lock = h3s_certs::private::exclusive_process_lock(&dir.join(".node-token.lock"))?;
        let path = dir.join("node-token");
        if !path.try_exists()? {
            let token = h3s_auth::bootstrap::random_secret()
                .map_err(|_| "secure random source unavailable")?;
            h3s_certs::private::write(&path, token.as_bytes(), false)?;
        }
        Some(
            String::from_utf8(h3s_certs::private::read(&path, 1024)?)
                .map_err(|_| "token file is not UTF-8")?
                .trim()
                .to_owned(),
        )
    } else {
        None
    };
    if text
        .as_deref()
        .is_some_and(|v| !h3s_auth::bootstrap::valid_token(v))
    {
        return Err("join token must be 32-256 printable ASCII bytes".into());
    }
    Ok(text)
}
async fn run_agent(args: AgentArgs) -> RunResult {
    let token = read_token(args.token.as_ref(), args.token_file.as_deref(), None)?;
    let agent = h3s_kubelet::Agent::connect(h3s_kubelet::Config {
        server: args.server,
        ca_file: args.server_ca_file,
        node_name: args.node_name,
        node_ip: args.node_ip,
        data_dir: args.data_dir,
        token,
        kubelet_port: args.kubelet_port,
        runtime_endpoint: args.container_runtime_endpoint,
    })
    .await?;
    agent.reconcile().await?;
    eprintln!("h3s agent enrolled; Node readiness follows configured runtime health");
    agent.run().await?;
    Ok(())
}
async fn run_command(command: Command) -> RunResult {
    match command {
        Command::Server(args) => run_server(args).await,
        Command::Agent(args) => run_agent(args).await,
        Command::RuntimeInfo(args) => runtime_info(args).await,
    }
}
#[tokio::main]
async fn main() {
    install_rustls_provider();
    let result = match Multicall::parse() {
        Multicall::H3s(cli) => run_command(cli.command).await,
        Multicall::Server(args) => run_server(args).await,
        Multicall::Agent(args) => run_agent(args).await,
        Multicall::RuntimeInfo(args) => runtime_info(args).await,
    };
    if let Err(error) = result {
        eprintln!("h3s: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn h3s_help_lists_server_and_agent() {
        let mut cmd = Multicall::command();
        let h3s = cmd.find_subcommand_mut("h3s").expect("h3s applet");
        let mut buf = Vec::new();
        h3s.write_help(&mut buf).unwrap();
        let help = String::from_utf8(buf).unwrap();
        assert!(help.contains("server"), "{help}");
        assert!(help.contains("agent"), "{help}");
    }

    #[test]
    fn server_help_is_display_help() {
        let err = Multicall::try_parse_from(["h3s", "server", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = err.to_string();
        assert!(help.contains("server"), "{help}");
    }

    #[test]
    fn agent_help_is_display_help() {
        let err = Multicall::try_parse_from(["h3s", "agent", "--help"]).unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = err.to_string();
        assert!(help.contains("agent"), "{help}");
    }

    #[test]
    fn parses_server_via_h3s_applet() {
        let parsed = Multicall::try_parse_from(["h3s", "server"]).expect("parse server");
        assert!(matches!(
            parsed,
            Multicall::H3s(H3sCli {
                command: Command::Server(_)
            })
        ));
    }

    #[test]
    fn parses_agent_via_h3s_applet() {
        let parsed = Multicall::try_parse_from([
            "h3s",
            "agent",
            "--server",
            "https://server:6443",
            "--server-ca-file",
            "/tmp/ca.crt",
            "--node-name",
            "worker",
            "--node-ip",
            "192.0.2.2",
        ])
        .expect("parse agent");
        assert!(matches!(
            parsed,
            Multicall::H3s(H3sCli {
                command: Command::Agent(_)
            })
        ));
    }
}
