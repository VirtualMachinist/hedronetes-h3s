//! Private, mutually authenticated kubelet API. Runtime readiness comes from the
//! actual current implementation state; no successful Pod/runtime is fabricated.
use crate::{Agent, Error, Result};
use bytes::Bytes;
use h3s_certs::{private, Identity};
use http_body_util::Full;
use hyper::{body::Incoming, service::service_fn, Request, Response};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Deserialize, Serialize};
use std::{convert::Infallible, sync::Arc, time::Duration};
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
pub async fn serve(listener: TcpListener, tls: rustls::ServerConfig, node: String) -> Result<()> {
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
                let acceptor=acceptor.clone();let node=node.clone();
                tasks.spawn(async move {
                    let _permit=permit;
                    let Ok(Ok(tls))=tokio::time::timeout(Duration::from_secs(5),acceptor.accept(stream)).await else {return;};
                    let allowed=tls.get_ref().1.peer_certificates().and_then(|p|p.first()).and_then(|der|h3s_auth::User::from_verified_certificate(der.as_ref()).ok()).is_some_and(|u|u.name==h3s_api::KUBELET_CLIENT_ID);
                    let handler=service_fn(move |request|health(request,allowed,node.clone()));
                    let _=hyper::server::conn::http1::Builder::new().timer(TokioTimer::new()).header_read_timeout(Duration::from_secs(5)).keep_alive(false).serve_connection(TokioIo::new(tls),handler).await;
                });
            },
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
        }
    }
}
async fn health(
    request: Request<Incoming>,
    allowed: bool,
    node: String,
) -> std::result::Result<Response<Full<Bytes>>, Infallible> {
    let (status, body) = if !allowed
        || request.headers().contains_key("authorization")
        || request
            .headers()
            .keys()
            .any(|k| k.as_str().starts_with("impersonate-"))
    {
        (403, "kubelet client identity required\n")
    } else if request.method() != "GET" {
        (405, "GET required\n")
    } else if request.uri().query().is_some() {
        (400, "query parameters are not supported\n")
    } else {
        match request.uri().path() {
            "/healthz" => {
                eprintln!("h3s kubelet node={node}: GET /healthz -> 200");
                (200, "ok\n")
            }
            "/readyz" => {
                eprintln!("h3s kubelet node={node}: GET /readyz -> 503");
                (
                    503,
                    "runtime not ready: CRI workload runtime is not implemented\n",
                )
            }
            _ => (404, "kubelet endpoint is not implemented\n"),
        }
    };
    Ok(Response::builder()
        .status(status)
        .header("content-type", "text/plain; charset=utf-8")
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from_static(body.as_bytes())))
        .expect("fixed response"))
}
