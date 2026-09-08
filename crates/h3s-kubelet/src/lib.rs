//! Native agent enrollment and Node/Lease lifecycle. CRI and Pod execution are
//! not implemented yet; this agent explicitly reports RuntimeNotReady.
use h3s_api::{JoinRequest, JoinResponse};
use h3s_auth::bootstrap::{random_secret, valid_password, valid_token};
use h3s_certs::{private, Identity};
use reqwest::{Client, Method, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{fs, io::Read, net::IpAddr, path::PathBuf, sync::Arc, time::Duration};

pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("agent configuration: {0}")]
    Configuration(&'static str),
    #[error("agent credential operation failed: {0}")]
    Credentials(#[from] h3s_certs::Error),
    #[error("agent I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("agent HTTPS request failed")]
    Transport(#[from] reqwest::Error),
    #[error("agent API returned status {0}")]
    Status(u16),
}
type Result<T> = std::result::Result<T, Error>;
pub struct Config {
    pub server: String,
    pub ca_file: PathBuf,
    pub node_name: String,
    pub node_ip: IpAddr,
    pub data_dir: PathBuf,
    pub token: Option<String>,
}
/// Private on-disk state: no Debug, never included in Node status or log output.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Enrollment {
    version: u32,
    server: String,
    node_name: String,
    ca_pem: String,
    private_key_pem: String,
    csr_pem: String,
    password: String,
    certificate_pem: Option<String>,
}
pub struct Agent {
    _lock: fs::File,
    client: Client,
    endpoint: Url,
    name: String,
    ip: IpAddr,
    tunnel_tls: Arc<rustls::ClientConfig>,
    tunnel_url: String,
}
fn invalid(message: &'static str) -> Error {
    Error::Configuration(message)
}
fn endpoint(server: &str) -> Result<Url> {
    let url = Url::parse(server).map_err(|_| invalid("invalid server URL"))?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(invalid(
            "server must be an HTTPS origin without userinfo, path, query or fragment",
        ));
    }
    Ok(url)
}
fn builder() -> reqwest::ClientBuilder {
    Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(15))
        .tls_sslkeylogfile(false)
}
async fn body(mut response: reqwest::Response) -> Result<(u16, Value)> {
    let code = response.status().as_u16();
    let mut data = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if data.len() + chunk.len() > 2 * 1024 * 1024 {
            return Err(invalid("API response exceeds limit"));
        }
        data.extend_from_slice(&chunk);
    }
    let value = serde_json::from_slice(&data).map_err(|_| invalid("API did not return JSON"))?;
    Ok((code, value))
}
impl Agent {
    pub async fn connect(config: Config) -> Result<Self> {
        let endpoint = endpoint(&config.server)?;
        if !h3s_api::valid_node_name(&config.node_name) {
            return Err(invalid("invalid node name"));
        }
        if config.node_ip.is_unspecified()
            || config.node_ip.is_multicast()
            || config.node_ip.is_loopback()
        {
            return Err(invalid(
                "node IP must identify its reachable node interface",
            ));
        }
        let ca_file = fs::File::open(&config.ca_file)?;
        if !ca_file.metadata()?.is_file() {
            return Err(invalid("CA must be a regular file"));
        }
        let mut ca = String::new();
        ca_file.take(1024 * 1024 + 1).read_to_string(&mut ca)?;
        if ca.len() > 1024 * 1024 {
            return Err(invalid("CA file exceeds limit"));
        }
        let ca_cert = reqwest::Certificate::from_pem(ca.as_bytes())?;
        let dir = config.data_dir.join("agent");
        private::directory(&dir)?;
        let lock = private::exclusive_process_lock(&dir.join(".agent.lock"))?;
        let path = dir.join("identity.json");
        let mut enrollment: Enrollment = if path.try_exists()? {
            serde_json::from_slice(&private::read(&path, 1024 * 1024)?)
                .map_err(|_| invalid("corrupt agent identity file"))?
        } else {
            let (private_key_pem, csr_pem) = h3s_certs::node_key_and_csr()?;
            let value = Enrollment {
                version: 1,
                server: endpoint.to_string(),
                node_name: config.node_name.clone(),
                ca_pem: ca.clone(),
                private_key_pem,
                csr_pem,
                password: random_secret()
                    .map_err(|_| invalid("secure random source unavailable"))?,
                certificate_pem: None,
            };
            private::write(
                &path,
                &serde_json::to_vec(&value).expect("enrollment serialization"),
                false,
            )?;
            value
        };
        if enrollment.version != 1
            || enrollment.server != endpoint.as_str()
            || enrollment.node_name != config.node_name
            || enrollment.ca_pem != ca
            || !valid_password(&enrollment.password)
        {
            return Err(invalid("agent identity belongs to a different server, CA or node, or has an unsupported format"));
        }
        if enrollment.certificate_pem.is_none() {
            let token = config
                .token
                .as_deref()
                .filter(|v| valid_token(v))
                .ok_or_else(|| invalid("initial enrollment requires a valid join token"))?;
            let anonymous = builder().tls_certs_only([ca_cert]).build()?;
            let joined = JoinRequest {
                node_name: config.node_name.clone(),
                password: enrollment.password.clone(),
                csr_pem: enrollment.csr_pem.clone(),
            };
            let response = anonymous
                .post(endpoint.join("v1-h3s/join").expect("fixed path"))
                .bearer_auth(token)
                .json(&joined)
                .send()
                .await?;
            let (code, value) = body(response).await?;
            if !matches!(code, 200 | 201) {
                return Err(Error::Status(code));
            }
            let response: JoinResponse = serde_json::from_value(value)
                .map_err(|_| invalid("invalid enrollment response"))?;
            let identity = Identity::from_pem(
                response.certificate_pem.clone(),
                enrollment.private_key_pem.clone(),
            )?;
            h3s_certs::node_client_config(&ca, &identity, &config.node_name)?;
            enrollment.certificate_pem = Some(response.certificate_pem);
            private::write(
                &path,
                &serde_json::to_vec(&enrollment).expect("enrollment serialization"),
                true,
            )?;
        }
        let identity = Identity::from_pem(
            enrollment.certificate_pem.expect("enrolled certificate"),
            enrollment.private_key_pem,
        )?;
        let tls = h3s_certs::node_client_config(&ca, &identity, &config.node_name)?;
        let client = builder().tls_backend_preconfigured(tls.clone()).build()?;
        let mut tunnel_endpoint = endpoint.join("v1-h3s/connect").expect("fixed tunnel path");
        tunnel_endpoint
            .set_scheme("wss")
            .map_err(|_| invalid("tunnel URL scheme"))?;
        Ok(Self {
            _lock: lock,
            client,
            endpoint,
            name: config.node_name,
            ip: config.node_ip,
            tunnel_tls: Arc::new(tls),
            tunnel_url: tunnel_endpoint.to_string(),
        })
    }
    async fn request(
        &self,
        method: Method,
        path: &str,
        value: Option<Value>,
    ) -> Result<(u16, Value)> {
        let mut request = self
            .client
            .request(method, self.endpoint.join(path).expect("fixed API path"));
        if let Some(value) = value {
            request = request.json(&value);
        }
        body(request.send().await?).await
    }
    /// Register and report a real running agent without claiming CRI readiness.
    pub async fn reconcile(&self) -> Result<()> {
        let path = format!("api/v1/nodes/{}", self.name);
        let (code, mut node) = self.request(Method::GET, &path, None).await?;
        if code == 404 {
            let created=self.request(Method::POST,"api/v1/nodes",Some(json!({"apiVersion":"v1","kind":"Node","metadata":{"name":self.name,"labels":{
                "kubernetes.io/hostname":self.name,"kubernetes.io/os":std::env::consts::OS,
                "kubernetes.io/arch":match std::env::consts::ARCH {"aarch64"=>"arm64","x86_64"=>"amd64",arch=>arch}
            }}}))).await?;
            if created.0 != 201 {
                return Err(Error::Status(created.0));
            }
            node = created.1;
        } else if code != 200 {
            return Err(Error::Status(code));
        }
        let time = now();
        let transition = node["status"]["conditions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|v| {
                v["type"] == "Ready" && v["status"] == "False" && v["reason"] == "RuntimeNotReady"
            })
            .and_then(|v| v["lastTransitionTime"].as_str())
            .unwrap_or(&time)
            .to_owned();
        node["status"] = json!({"addresses":[{"type":"InternalIP","address":self.ip.to_string()},{"type":"Hostname","address":self.name}],
            "conditions":[{"type":"Ready","status":"False","reason":"RuntimeNotReady","message":"agent enrolled; CRI workload runtime is not implemented",
                "lastHeartbeatTime":time,"lastTransitionTime":transition}]});
        let (code, updated) = self
            .request(Method::PUT, &format!("{path}/status"), Some(node))
            .await?;
        if code != 200 {
            return Err(Error::Status(code));
        }
        let lease_path = format!(
            "apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/{}",
            self.name
        );
        let (code, mut lease) = self.request(Method::GET, &lease_path, None).await?;
        let create = code == 404;
        if !create && code != 200 {
            return Err(Error::Status(code));
        }
        if create {
            lease = json!({"apiVersion":"coordination.k8s.io/v1","kind":"Lease","metadata":{"name":self.name,"namespace":"kube-node-lease",
            }});
        }
        // The Node may have been deleted and recreated while its old Lease survived.
        lease["metadata"]["ownerReferences"] = json!([{
            "apiVersion":"v1","kind":"Node","name":self.name,"uid":updated["metadata"]["uid"]
        }]);
        lease["spec"] =
            json!({"holderIdentity":self.name,"leaseDurationSeconds":40,"renewTime":now()});
        let target = if create {
            "apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases"
        } else {
            &lease_path
        };
        let (code, _) = self
            .request(
                if create { Method::POST } else { Method::PUT },
                target,
                Some(lease),
            )
            .await?;
        if code != if create { 201 } else { 200 } {
            return Err(Error::Status(code));
        }
        Ok(())
    }
    pub async fn run(&self) -> Result<()> {
        let heartbeats = async {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Err(error) = self.reconcile().await {
                    eprintln!("h3s agent {}: {error}; retrying", self.name);
                }
            }
        };
        let tunnel = async {
            let mut delay = 1;
            loop {
                let started = tokio::time::Instant::now();
                let result = h3s_supervisor::run_worker(
                    &self.tunnel_url,
                    self.tunnel_tls.clone(),
                    "127.0.0.1:10250".parse().expect("fixed loopback endpoint"),
                )
                .await;
                if started.elapsed() > Duration::from_secs(60) {
                    delay = 1;
                }
                match result {
                    Ok(()) => eprintln!("h3s supervisor disconnected; reconnecting in {delay}s"),
                    Err(error) => eprintln!("h3s supervisor: {error}; reconnecting in {delay}s"),
                }
                tokio::time::sleep(Duration::from_secs(delay)).await;
                delay = (delay * 2).min(30);
            }
        };
        // Network stalls on heartbeats must not stop polling the tunnel, and
        // a tunnel outage must not stop direct authenticated Node/Lease updates.
        tokio::select! {
            _=tokio::signal::ctrl_c()=>Ok(()),
            _=heartbeats=>Ok(()),
            _=tunnel=>Ok(()),
        }
    }
}
fn now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .expect("UTC time")
}
