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
         Workload runtime and worker agent are not yet implemented.",
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
    /// Start a worker agent (kubelet + kube-proxy + CNI + tunnel client).
    Agent(AgentArgs),
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
    /// Start a worker agent (kubelet + kube-proxy + CNI + tunnel client).
    Agent(AgentArgs),
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
}

/// P0 stub arguments for `h3s agent`.
#[derive(Debug, Args)]
struct AgentArgs {}

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
    let pki = h3s_certs::ClusterPki::open_or_create(&server_dir.join("tls"), &sans)?;
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
    let api = h3s_apiserver::Api::new(store).await?;
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
    let server = h3s_apiserver::serve(listener, pki.server_config()?, api.router(), async {
        let _ = tokio::signal::ctrl_c().await;
    });
    tokio::select! {
        result = server => result?,
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
async fn run_command(command: Command) -> RunResult {
    match command {
        Command::Server(args) => run_server(args).await,
        Command::Agent(_) => Err("node agent is not implemented".into()),
    }
}
#[tokio::main]
async fn main() {
    install_rustls_provider();
    let result = match Multicall::parse() {
        Multicall::H3s(cli) => run_command(cli.command).await,
        Multicall::Server(args) => run_server(args).await,
        Multicall::Agent(_) => Err("node agent is not implemented".into()),
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
        let parsed = Multicall::try_parse_from(["h3s", "agent"]).expect("parse agent");
        assert!(matches!(
            parsed,
            Multicall::H3s(H3sCli {
                command: Command::Agent(_)
            })
        ));
    }
}
