mod common;
use common::{Server, JOIN_TOKEN};
use h3s_certs::Identity;
use h3s_kubelet::{Agent, Config};
use http_body_util::{BodyExt, Empty};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{fs, io, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::{net::TcpStream, task::JoinHandle};

struct Running(JoinHandle<()>);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.abort();
    }
}
fn config(s: &Server, dir: &Path, token: bool) -> Config {
    fs::write(dir.join("ca.pem"), s.pki.ca_pem()).unwrap();
    Config {
        server: s.endpoint(),
        ca_file: dir.join("ca.pem"),
        node_name: "worker".into(),
        node_ip: "192.0.2.2".parse().unwrap(),
        data_dir: dir.join("state"),
        token: token.then(|| JOIN_TOKEN.into()),
        kubelet_port: 0,
    }
}
async fn text(s: &Server, tls: rustls::ClientConfig, path: &str) -> (u16, String) {
    let response = s.raw(tls, "GET", path, json!({}), &[]).await;
    let code = response.status().as_u16();
    (
        code,
        String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap(),
    )
}
async fn healthy(s: &Server) -> (u16, String) {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let r = text(s, s.admin(), "/api/v1/nodes/worker/proxy/healthz").await;
            if r.0 == 200 {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap()
}
fn client(s: &Server, dir: &Path) -> rustls::ClientConfig {
    let state: Value =
        serde_json::from_slice(&fs::read(dir.join("state/agent/identity.json")).unwrap()).unwrap();
    let id = Identity::from_pem(
        state["certificate_pem"].as_str().unwrap().into(),
        state["private_key_pem"].as_str().unwrap().into(),
    )
    .unwrap();
    h3s_certs::node_client_config(s.pki.ca_pem(), &id, "worker").unwrap()
}
async fn direct(
    address: SocketAddr,
    tls: rustls::ClientConfig,
    path: &str,
) -> io::Result<(u16, String)> {
    let socket = TcpStream::connect(address).await?;
    let tls = tokio_rustls::TlsConnector::from(Arc::new(tls))
        .connect(h3s_certs::kubelet_server_name("worker"), socket)
        .await?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
        .await
        .map_err(io::Error::other)?;
    let _task = Running(tokio::spawn(async move {
        let _ = connection.await;
    }));
    let request = hyper::Request::builder()
        .method("GET")
        .uri(path)
        .header("host", "kubelet")
        .body(Empty::<axum::body::Bytes>::new())
        .unwrap();
    let response = sender
        .send_request(request)
        .await
        .map_err(io::Error::other)?;
    let code = response.status().as_u16();
    let bytes = response
        .into_body()
        .collect()
        .await
        .map_err(io::Error::other)?
        .to_bytes();
    Ok((code, String::from_utf8(bytes.to_vec()).unwrap()))
}
#[tokio::test]
async fn actual_kubelet_health_uses_mutual_tls_tunnel_and_authorized_node_proxy() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let agent = Agent::connect(config(&s, dir.path(), true)).await.unwrap();
    let _agent = Running(tokio::spawn(async move {
        agent.run().await.unwrap();
    }));
    assert_eq!(healthy(&s).await, (200, "ok\n".into()));
    let ready = text(&s, s.admin(), "/api/v1/nodes/worker/proxy/readyz").await;
    assert_eq!(ready.0, 503);
    assert!(ready.1.contains("CRI workload runtime is not implemented"));
    let node = s
        .json(s.admin(), "GET", "/api/v1/nodes/worker", json!({}))
        .await
        .1;
    assert_eq!(node["status"]["conditions"][0]["status"], "False");
    let port = node["status"]["daemonEndpoints"]["kubeletEndpoint"]["Port"]
        .as_u64()
        .unwrap() as u16;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let node_client = client(&s, dir.path());
    assert_eq!(
        direct(addr, node_client.clone(), "/healthz")
            .await
            .unwrap()
            .0,
        403
    );
    assert_eq!(direct(addr, s.admin(), "/healthz").await.unwrap().0, 403);
    assert!(direct(addr, s.pki.client_config(None).unwrap(), "/healthz")
        .await
        .is_err());
    assert_eq!(
        text(&s, node_client, "/api/v1/nodes/worker/proxy/healthz")
            .await
            .0,
        403
    );
    let reader = s.pki.issue_client("health-reader", None).unwrap();
    let reader = s.pki.client_config(Some(&reader)).unwrap();
    assert_eq!(
        text(&s, reader.clone(), "/api/v1/nodes/worker/proxy/healthz")
            .await
            .0,
        403
    );
    let role = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole","metadata":{"name":"health-reader"},"rules":[{"apiGroups":[""],"resources":["nodes/proxy"],"resourceNames":["worker"],"verbs":["get"]}]});
    assert_eq!(
        s.json(
            s.admin(),
            "POST",
            "/apis/rbac.authorization.k8s.io/v1/clusterroles",
            role
        )
        .await
        .0,
        201
    );
    let binding = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRoleBinding","metadata":{"name":"health-reader"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"ClusterRole","name":"health-reader"},"subjects":[{"kind":"User","apiGroup":"rbac.authorization.k8s.io","name":"health-reader"}]});
    assert_eq!(
        s.json(
            s.admin(),
            "POST",
            "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings",
            binding
        )
        .await
        .0,
        201
    );
    assert_eq!(
        text(&s, reader.clone(), "/api/v1/nodes/worker/proxy/healthz")
            .await
            .0,
        200
    );
    assert_eq!(
        text(&s, reader.clone(), "/api/v1/nodes/other/proxy/healthz")
            .await
            .0,
        403
    );
    assert_eq!(
        text(&s, reader.clone(), "/api/v1/nodes/worker/proxy/configz")
            .await
            .0,
        404
    );
    assert_eq!(
        text(
            &s,
            reader,
            "/api/v1/nodes/worker/proxy/healthz?target=other"
        )
        .await
        .0,
        400
    );
    assert_eq!(
        text(&s, s.admin(), "/api/v1/nodes/missing/proxy/healthz")
            .await
            .0,
        404
    );
}

#[tokio::test]
async fn serving_csr_is_node_bound_and_cannot_request_control_plane_or_client_privileges() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let agent = Agent::connect(config(&s, dir.path(), true)).await.unwrap();
    agent.reconcile().await.unwrap();
    let tls = client(&s, dir.path());
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params =
        rcgen::CertificateParams::new(vec!["kubernetes".into(), "127.0.0.1".into()]).unwrap();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "h3s-admin");
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "system:masters");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
    let csr = params.serialize_request(&key).unwrap().pem().unwrap();
    let body = json!({"csr_pem":csr});
    for (credential, expected) in [(s.pki.client_config(None).unwrap(), 401), (s.admin(), 403)] {
        assert_eq!(
            s.json(credential, "POST", "/v1-h3s/serving", body.clone())
                .await
                .0,
            expected
        );
    }
    let unregistered = s
        .pki
        .issue_client("system:node:unregistered", Some("system:nodes"))
        .unwrap();
    assert_eq!(
        s.json(
            s.pki.client_config(Some(&unregistered)).unwrap(),
            "POST",
            "/v1-h3s/serving",
            body.clone()
        )
        .await
        .0,
        403
    );
    assert_eq!(
        s.json(
            tls.clone(),
            "POST",
            "/v1-h3s/serving",
            json!({"csr_pem":csr,"node_name":"kubernetes"})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        s.json(
            tls.clone(),
            "POST",
            "/v1-h3s/serving",
            json!({"csr_pem":"invalid"})
        )
        .await
        .0,
        400
    );
    let (code, response) = s.json(tls, "POST", "/v1-h3s/serving", body).await;
    assert_eq!(code, 200);
    assert_eq!(response.as_object().unwrap().len(), 1);
    assert!(!response.to_string().contains("PRIVATE KEY"));
    let identity = Identity::from_pem(
        response["certificate_pem"].as_str().unwrap().into(),
        key.serialize_pem(),
    )
    .unwrap();
    assert!(h3s_certs::kubelet_server_config(s.pki.ca_pem(), &identity, "worker").is_ok());
    assert!(h3s_certs::kubelet_server_config(s.pki.ca_pem(), &identity, "kubernetes").is_err());
    assert!(h3s_certs::node_client_config(s.pki.ca_pem(), &identity, "worker").is_err());
    assert_ne!(h3s_certs::kubelet_dns_name("kubernetes"), "kubernetes");
}

#[tokio::test]
async fn serving_identity_and_real_proxy_recover_across_worker_and_server_restarts() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let agent = Agent::connect(config(&s, dir.path(), true)).await.unwrap();
    let run = Running(tokio::spawn(async move {
        agent.run().await.unwrap();
    }));
    healthy(&s).await;
    let serving = dir.path().join("state/agent/serving.json");
    let bytes = fs::read(&serving).unwrap();
    let state: Value = serde_json::from_slice(&bytes).unwrap();
    let node: Value =
        serde_json::from_slice(&fs::read(dir.path().join("state/agent/identity.json")).unwrap())
            .unwrap();
    assert_ne!(state["private_key_pem"], node["private_key_pem"]);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&serving).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let before = s
        .json(s.admin(), "GET", "/api/v1/nodes/worker", json!({}))
        .await
        .1;
    let s = s.restart(root.path()).await;
    assert_eq!(healthy(&s).await.0, 200);
    assert_eq!(bytes, fs::read(&serving).unwrap());
    run.0.abort();
    while !run.0.is_finished() {
        tokio::task::yield_now().await;
    }
    let agent = Agent::connect(config(&s, dir.path(), false)).await.unwrap();
    let run = Running(tokio::spawn(async move {
        agent.run().await.unwrap();
    }));
    assert_eq!(healthy(&s).await.0, 200);
    assert_eq!(bytes, fs::read(&serving).unwrap());
    let after = s
        .json(s.admin(), "GET", "/api/v1/nodes/worker", json!({}))
        .await
        .1;
    assert_eq!(before["metadata"]["uid"], after["metadata"]["uid"]);
    run.0.abort();
    while !run.0.is_finished() {
        tokio::task::yield_now().await;
    }
    let mut corrupt: Value = serde_json::from_slice(&bytes).unwrap();
    corrupt["node"] = "other".into();
    let changed = serde_json::to_vec(&corrupt).unwrap();
    fs::write(&serving, &changed).unwrap();
    let agent = Agent::connect(config(&s, dir.path(), false)).await.unwrap();
    assert!(agent.run().await.is_err());
    assert_eq!(changed, fs::read(&serving).unwrap());
}

#[tokio::test]
async fn node_proxy_verifies_backend_node_name_inside_the_authenticated_tunnel() {
    // The outer tunnel is authenticated as worker in both cases. Only the
    // inner serving certificate differs; trusted CA membership is insufficient.
    for (serving_node, expected) in [("other", 502), ("worker", 200)] {
        let root = tempfile::tempdir().unwrap();
        let s = Server::start(root.path()).await;
        let dir = tempfile::tempdir().unwrap();
        let agent = Agent::connect(config(&s, dir.path(), true)).await.unwrap();
        agent.reconcile().await.unwrap();
        let (key, csr) = h3s_certs::node_key_and_csr().unwrap();
        let identity =
            Identity::from_pem(s.pki.sign_kubelet_csr(serving_node, &csr).unwrap(), key).unwrap();
        let tls =
            h3s_certs::kubelet_server_config(s.pki.ca_pem(), &identity, serving_node).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let _backend = Running(tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let accepted = tokio_rustls::TlsAcceptor::from(Arc::new(tls))
                .accept(socket)
                .await;
            let _ = result_tx.send(accepted.is_ok());
            if let Ok(tls) = accepted {
                let peer = &tls.get_ref().1.peer_certificates().unwrap()[0];
                let user = h3s_auth::User::from_verified_certificate(peer.as_ref()).unwrap();
                assert_eq!(user.name, h3s_api::KUBELET_CLIENT_ID);
                let handler = hyper::service::service_fn(
                    |request: hyper::Request<hyper::body::Incoming>| async move {
                        assert_eq!(request.uri().path(), "/healthz");
                        assert!(!request.headers().contains_key("authorization"));
                        Ok::<_, std::convert::Infallible>(hyper::Response::new(
                            http_body_util::Full::new(axum::body::Bytes::from_static(b"ok\n")),
                        ))
                    },
                );
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(tls), handler)
                    .await;
            }
        }));
        let url = s.endpoint().replace("https://", "wss://") + "/v1-h3s/connect";
        let node_tls = Arc::new(client(&s, dir.path()));
        let _tunnel = Running(tokio::spawn(async move {
            h3s_supervisor::run_worker(&url, node_tls, address)
                .await
                .unwrap();
        }));
        tokio::time::timeout(Duration::from_secs(5), async {
            while !s.supervisor.connected("worker") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let response = text(&s, s.admin(), "/api/v1/nodes/worker/proxy/healthz").await;
        assert_eq!(response.0, expected);
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), result_rx)
                .await
                .unwrap()
                .unwrap(),
            expected == 200
        );
        if expected == 200 {
            assert_eq!(response.1, "ok\n");
        }
        assert!(s.supervisor.connected("worker"));
    }
}
