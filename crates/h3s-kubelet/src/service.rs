//! Private, mutually authenticated kubelet API. Runtime readiness comes from the
//! actual current implementation state; no successful Pod/runtime is fabricated.
use crate::{runtime, Agent, Error, Result};
use bytes::Bytes;
use h3s_certs::{private, Identity};
use http_body_util::Full;
use hyper::{body::Incoming, service::service_fn, Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, convert::Infallible, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Serving {
    version: u32,
    node: String,
    ca_pem: String,
    private_key_pem: String,
    csr_pem: String,
    certificate_pem: Option<String>,
}
/// Caller holds the agent directory's lifetime process lock.
pub async fn configuration(agent: &Agent) -> Result<rustls::ServerConfig> {
    let path = agent.agent_dir.join("serving.json");
    let mut state: Serving = if path.try_exists()? {
        serde_json::from_slice(&private::read(&path, 1024 * 1024)?)
            .map_err(|_| Error::Configuration("corrupt kubelet serving identity"))?
    } else {
        let (key, csr) = h3s_certs::node_key_and_csr()?;
        let state = Serving {
            version: 1,
            node: agent.name.clone(),
            ca_pem: agent.ca_pem.clone(),
            private_key_pem: key,
            csr_pem: csr,
            certificate_pem: None,
        };
        private::write(
            &path,
            &serde_json::to_vec(&state).expect("serving identity serialization"),
            false,
        )?;
        state
    };
    if state.version != 1 || state.node != agent.name || state.ca_pem != agent.ca_pem {
        return Err(Error::Configuration(
            "kubelet identity belongs to another node/CA or unsupported format",
        ));
    }
    if state.certificate_pem.is_none() {
        let response = agent
            .client
            .post(
                agent
                    .endpoint
                    .join("v1-h3s/serving")
                    .expect("fixed serving path"),
            )
            .json(&h3s_api::ServingRequest {
                csr_pem: state.csr_pem.clone(),
            })
            .send()
            .await?;
        let (code, value) = crate::body(response).await?;
        if code != 200 {
            return Err(Error::Status(code));
        }
        let certificate: h3s_api::ServingResponse = serde_json::from_value(value)
            .map_err(|_| Error::Configuration("invalid serving certificate response"))?;
        let identity = Identity::from_pem(
            certificate.certificate_pem.clone(),
            state.private_key_pem.clone(),
        )?;
        h3s_certs::kubelet_server_config(&agent.ca_pem, &identity, &agent.name)?;
        state.certificate_pem = Some(certificate.certificate_pem);
        private::write(
            &path,
            &serde_json::to_vec(&state).expect("serving identity serialization"),
            true,
        )?;
    }
    let identity = Identity::from_pem(
        state.certificate_pem.expect("serving certificate"),
        state.private_key_pem,
    )?;
    Ok(h3s_certs::kubelet_server_config(
        &agent.ca_pem,
        &identity,
        &agent.name,
    )?)
}
pub async fn serve(
    listener: TcpListener,
    tls: rustls::ServerConfig,
    node: String,
    ready: Arc<std::sync::atomic::AtomicBool>,
    runtime: Option<Arc<runtime::Runtime>>,
) -> Result<()> {
    if !listener.local_addr()?.ip().is_loopback() {
        return Err(Error::Configuration("kubelet listener must be loopback"));
    }
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let slots = Arc::new(Semaphore::new(16));
    let mut tasks = JoinSet::new();
    loop {
        tokio::select! {
            accepted=listener.accept()=>{
                let (stream,_)=accepted?;
                let Ok(permit)=slots.clone().try_acquire_owned() else {drop(stream);continue;};
                let acceptor=acceptor.clone();let node=node.clone();let ready=ready.clone();let runtime=runtime.clone();
                tasks.spawn(async move {
                    let _permit=permit;
                    let Ok(Ok(tls))=tokio::time::timeout(Duration::from_secs(5),acceptor.accept(stream)).await else {return;};
                    let allowed=tls.get_ref().1.peer_certificates().and_then(|p|p.first()).and_then(|der|h3s_auth::User::from_verified_certificate(der.as_ref()).ok()).is_some_and(|u|u.name==h3s_api::KUBELET_CLIENT_ID);
                    let handler=service_fn(move |request|handle(request,allowed,node.clone(),ready.clone(),runtime.clone()));
                    let _=hyper::server::conn::http1::Builder::new().timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(5)).keep_alive(false).serve_connection(TokioIo::new(tls),handler).await;
                });
            },
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
        }
    }
}
fn text(status: u16, body: &'static str) -> Response<Full<Bytes>> {
    reply(
        status,
        "text/plain; charset=utf-8",
        Bytes::from_static(body.as_bytes()),
    )
}
fn reply(status: u16, content_type: &'static str, body: Bytes) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header("content-type", content_type)
        .header("cache-control", "no-store")
        .body(Full::new(body))
        .expect("fixed response")
}
fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value.contains("..")
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}
async fn handle(
    request: Request<Incoming>,
    allowed: bool,
    node: String,
    ready: Arc<std::sync::atomic::AtomicBool>,
    runtime: Option<Arc<runtime::Runtime>>,
) -> std::result::Result<Response<Full<Bytes>>, Infallible> {
    if !allowed
        || request.headers().contains_key("authorization")
        || request
            .headers()
            .keys()
            .any(|k| k.as_str().starts_with("impersonate-"))
    {
        return Ok(text(403, "kubelet client identity required\n"));
    }
    let path = request.uri().path().to_owned();
    let method = request.method().as_str().to_owned();
    let query = request.uri().query().unwrap_or("").to_owned();
    if matches!(path.as_str(), "/healthz" | "/readyz") {
        if method != "GET" {
            return Ok(text(405, "GET required\n"));
        }
        if !query.is_empty() {
            return Ok(text(400, "query parameters are not supported\n"));
        }
        return Ok(match path.as_str() {
            "/healthz" => {
                eprintln!("h3s kubelet node={node}: GET /healthz -> 200");
                text(200, "ok\n")
            }
            "/readyz" if ready.load(std::sync::atomic::Ordering::Relaxed) => {
                eprintln!("h3s kubelet node={node}: GET /readyz -> 200");
                text(200, "ok\n")
            }
            _ => {
                eprintln!("h3s kubelet node={node}: GET /readyz -> 503");
                text(
                    503,
                    "runtime not ready: CRI runtime is absent or not ready\n",
                )
            }
        });
    }
    let pairs: Vec<(String, String)> = serde_urlencoded::from_str(&query).unwrap_or_default();
    if path == "/containerLogs" {
        return Ok(container_logs(method, &pairs, runtime.as_deref()));
    }
    if path == "/exec" {
        return Ok(exec_sync(method, &pairs, runtime.as_deref()).await);
    }
    Ok(text(404, "kubelet endpoint is not implemented\n"))
}
fn container_logs(
    method: String,
    pairs: &[(String, String)],
    runtime: Option<&runtime::Runtime>,
) -> Response<Full<Bytes>> {
    if method != "GET" {
        return text(405, "GET required\n");
    }
    let Some(runtime) = runtime else {
        return text(501, "runtime is not configured\n");
    };
    let mut q = BTreeMap::new();
    for (k, v) in pairs {
        if q.insert(k.as_str(), v.as_str()).is_some() {
            return text(400, "duplicate query parameter\n");
        }
        if !matches!(
            k.as_str(),
            "uid" | "container" | "attempt" | "timestamps" | "tailLines"
        ) {
            return text(400, "unsupported log query parameter\n");
        }
    }
    let uid = q.get("uid").copied().unwrap_or("");
    let container = q.get("container").copied().unwrap_or("");
    if !valid_id(uid) || !valid_id(container) {
        return text(400, "uid and container are required\n");
    }
    let attempt = match q.get("attempt").copied().unwrap_or("0").parse::<u32>() {
        Ok(n) => n,
        Err(_) => return text(400, "invalid attempt\n"),
    };
    let timestamps = matches!(q.get("timestamps").copied(), Some("true") | Some("1"));
    let tail_lines = match q.get("tailLines") {
        None => None,
        Some(v) => match v.parse::<usize>() {
            Ok(n) => Some(n),
            Err(_) => return text(400, "invalid tailLines\n"),
        },
    };
    match runtime.read_container_log(uid, container, attempt) {
        Ok(bytes) => reply(
            200,
            "text/plain; charset=utf-8",
            Bytes::from(runtime::format_container_log(
                &bytes, timestamps, tail_lines,
            )),
        ),
        Err(_) => text(404, "container log not found\n"),
    }
}
async fn exec_sync(
    method: String,
    pairs: &[(String, String)],
    runtime: Option<&runtime::Runtime>,
) -> Response<Full<Bytes>> {
    if method != "POST" {
        return text(405, "POST required\n");
    }
    let Some(runtime) = runtime else {
        return text(501, "runtime is not configured\n");
    };
    let mut container_id = "";
    let mut timeout = 10i64;
    let mut cmd = Vec::new();
    let mut seen_id = false;
    let mut seen_timeout = false;
    for (k, v) in pairs {
        match k.as_str() {
            "containerId" => {
                if seen_id {
                    return text(400, "duplicate query parameter\n");
                }
                seen_id = true;
                container_id = v;
            }
            "timeoutSeconds" => {
                if seen_timeout {
                    return text(400, "duplicate query parameter\n");
                }
                seen_timeout = true;
                match v.parse::<i64>() {
                    Ok(n) if (1..=30).contains(&n) => timeout = n,
                    _ => return text(400, "invalid timeoutSeconds\n"),
                }
            }
            "command" => {
                if v.is_empty() {
                    return text(400, "command must not be empty\n");
                }
                cmd.push(v.clone());
            }
            _ => return text(400, "unsupported exec query parameter\n"),
        }
    }
    if !valid_id(container_id) || cmd.is_empty() {
        return text(400, "containerId and command are required\n");
    }
    match runtime.exec_sync(container_id, cmd, timeout).await {
        Ok(result) => reply(
            200,
            "application/json",
            Bytes::from(
                serde_json::to_vec(&serde_json::json!({
                    "stdout": String::from_utf8_lossy(&result.stdout),
                    "stderr": String::from_utf8_lossy(&result.stderr),
                    "exitCode": result.exit_code
                }))
                .expect("exec JSON"),
            ),
        ),
        Err(_) => text(502, "exec failed\n"),
    }
}
