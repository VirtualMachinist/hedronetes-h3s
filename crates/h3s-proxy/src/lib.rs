//! Native Service reconciliation with node-scoped API credentials and owned nftables.
mod api;
mod nft;
mod plan;
pub use plan::{plan, Plan};
use reqwest::{Client, Url};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
pub const TABLE: &str = "h3s_proxy";
pub const MAX_RULESET: usize = 2 * 1024 * 1024;
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("service proxy: {0}")]
    Invalid(&'static str),
    #[error("service proxy API transport failed")]
    Transport(#[from] reqwest::Error),
    #[error("service proxy API status {0}")]
    Status(u16),
    #[error("service proxy I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("service proxy JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("service proxy private state: {0}")]
    Private(#[from] h3s_certs::Error),
    #[error("nft helper failed: {0}")]
    Nft(String),
}
pub type Result<T> = std::result::Result<T, Error>;
pub struct Config {
    pub nft_binary: PathBuf,
    pub state_dir: PathBuf,
    pub node_name: String,
    /// Hex digest of cluster CA and node name; never private material.
    pub owner: String,
}
/// Preserve last known good rules across API and process outages. Membership
/// changes require a complete snapshot; periodic kernel inspection repairs drift.
pub async fn run(
    config: Config,
    client: Client,
    endpoint: Url,
    ready: Arc<AtomicBool>,
) -> Result<()> {
    h3s_certs::private::directory(&config.state_dir)?;
    let _lock = h3s_certs::private::exclusive_process_lock(&config.state_dir.join(".lock"))?;
    let mut backend = nft::Backend::new(&config.nft_binary, &config.owner)?;
    let mut desired: Option<String> = None;
    let mut desired_valid = false;
    loop {
        if let Some(rules) = &desired {
            match backend.reconcile(rules).await {
                Ok(_) => ready.store(desired_valid, Ordering::Relaxed),
                Err(error) => {
                    ready.store(false, Ordering::Relaxed);
                    eprintln!("{error}; retrying");
                }
            }
        }
        match api::snapshot(&client, &endpoint).await {
            Ok(snapshot) => {
                match plan(
                    &snapshot.services.items,
                    &snapshot.slices.items,
                    &snapshot.nodes.items,
                    &config.node_name,
                )
                .and_then(|plan| plan.render(&config.owner))
                {
                    Ok(rules) => {
                        match backend.reconcile(&rules).await {
                            Ok(changed) => {
                                if changed {
                                    h3s_certs::private::write(
                                        &config.state_dir.join("rules.nft"),
                                        rules.as_bytes(),
                                        true,
                                    )?;
                                    eprintln!("h3s Service proxy: installed {} bytes of owned nftables rules", rules.len());
                                }
                                desired = Some(rules);
                                desired_valid = true;
                                ready.store(true, Ordering::Relaxed);
                            }
                            Err(error) => {
                                ready.store(false, Ordering::Relaxed);
                                eprintln!("{error}; retrying");
                            }
                        }
                    }
                    Err(error) => {
                        desired_valid = false;
                        ready.store(false, Ordering::Relaxed);
                        eprintln!("{error}; keeping last valid Service rules");
                    }
                }
                tokio::select! {
                    _ = api::changed(&client, &endpoint, "api/v1/services", &snapshot.services.revision) => {},
                    _ = api::changed(&client, &endpoint, "apis/discovery.k8s.io/v1/endpointslices", &snapshot.slices.revision) => {},
                    _ = api::changed(&client, &endpoint, "api/v1/nodes", &snapshot.nodes.revision) => {},
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {},
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => {
                eprintln!("{error}; keeping last valid Service rules");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}
