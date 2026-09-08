//! Hedronetes (`h3s`) multicall binary.
//!
//! P0 stub: clap personalities for `h3s`, `h3s server`, and `h3s agent`.
//! No API server, kubelet, skip-auth, or SSE watch path.

use clap::{Args, Parser, Subcommand};

/// Hedronetes (h3s) — Kubernetes-compatible cluster distribution in one binary.
#[derive(Debug, Parser)]
#[command(
    name = "h3s",
    version,
    about = "Hedronetes (h3s): Kubernetes-compatible cluster distribution in one Rust binary",
    long_about = "k3s, written in Rust, without embedding a Go control plane.\n\n\
         P0 stub: server/agent personalities parse and print help. \
         The API server and kubelet are not implemented.",
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

/// P0 stub arguments for `h3s server`.
#[derive(Debug, Args)]
struct ServerArgs {}

/// P0 stub arguments for `h3s agent`.
#[derive(Debug, Args)]
struct AgentArgs {}

fn install_rustls_provider() {
    // rustls 0.23 default crypto is aws-lc-rs. OpenSSL is not a default feature.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

fn run_server(_args: ServerArgs) -> ! {
    eprintln!("h3s server: P0 stub. Control plane is not implemented. See SPEC.md.");
    std::process::exit(1);
}

fn run_agent(_args: AgentArgs) -> ! {
    eprintln!("h3s agent: P0 stub. Node agent is not implemented. See SPEC.md.");
    std::process::exit(1);
}

fn run_command(command: Command) -> ! {
    match command {
        Command::Server(args) => run_server(args),
        Command::Agent(args) => run_agent(args),
    }
}

#[tokio::main]
async fn main() {
    install_rustls_provider();
    match Multicall::parse() {
        Multicall::H3s(cli) => run_command(cli.command),
        Multicall::Server(args) => run_server(args),
        Multicall::Agent(args) => run_agent(args),
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
