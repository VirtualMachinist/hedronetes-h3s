mod common;
use axum::body::Bytes;
use common::{Server, JOIN_TOKEN};
use h3s_certs::Identity;
use h3s_kubelet::{Agent, Config};
use http_body_util::{BodyExt, Full};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::{fs, path::Path, sync::Arc, time::Duration};
use tokio::task::JoinHandle;

struct Running(JoinHandle<()>);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn config(s: &Server, dir: &Path, runtime: bool) -> Config {
    fs::write(dir.join("ca.pem"), s.pki.ca_pem()).unwrap();
    Config {
        server: s.endpoint(),
        ca_file: dir.join("ca.pem"),
        node_name: "worker".into(),
        node_ip: "192.0.2.2".parse().unwrap(),
        data_dir: dir.join("state"),
        token: Some(JOIN_TOKEN.into()),
        kubelet_port: 0,
        runtime_endpoint: runtime
            .then(|| format!("unix://{}", dir.join("containerd.sock").display())),
        service_proxy_nft: None,
        cluster_dns: None,
    }
}

async fn text(s: &Server, method: &str, path: &str) -> (u16, String) {
    let response = s.raw(s.admin(), method, path, json!({}), &[]).await;
    let code = response.status().as_u16();
    let body = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    (code, body)
}

fn pod_spec(name: &str) -> Value {
    json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":name},"spec":{"automountServiceAccountToken":false,"securityContext":{"runAsNonRoot":true,"seccompProfile":{"type":"RuntimeDefault"}},"containers":[{"name":"web","image":"example.invalid/web:v1","securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}}]}})
}

async fn healthy(s: &Server) {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            let (code, _) = text(s, "GET", "/api/v1/nodes/worker/proxy/healthz").await;
            if code == 200 {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("kubelet health");
}

fn node_client(s: &Server, dir: &Path) -> rustls::ClientConfig {
    let state: Value =
        serde_json::from_slice(&fs::read(dir.join("state/agent/identity.json")).unwrap()).unwrap();
    let identity = Identity::from_pem(
        state["certificate_pem"].as_str().unwrap().into(),
        state["private_key_pem"].as_str().unwrap().into(),
    )
    .unwrap();
    h3s_certs::node_client_config(s.pki.ca_pem(), &identity, "worker").unwrap()
}

#[tokio::test]
async fn discovery_advertises_pod_log_and_exec() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    let (_, discovery) = s.json(s.admin(), "GET", "/api/v1", json!({})).await;
    let names: Vec<_> = discovery["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(names.contains(&"pods/log".into()), "{names:?}");
    assert!(names.contains(&"pods/exec".into()), "{names:?}");
}

#[tokio::test]
async fn log_follow_and_missing_pod_and_unassigned_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    let (code, body) = text(
        &s,
        "GET",
        "/api/v1/namespaces/default/pods/missing/log?follow=true",
    )
    .await;
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("follow"), "{body}");
    let (code, body) = text(&s, "GET", "/api/v1/namespaces/default/pods/missing/log").await;
    assert_eq!(code, 404, "{body}");
    let (code, created) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/default/pods",
            pod_spec("web"),
        )
        .await;
    assert_eq!(code, 201, "{created}");
    let (code, body) = text(&s, "GET", "/api/v1/namespaces/default/pods/web/log").await;
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("assigned"), "{body}");
    let (code, body) = text(&s, "GET", "/api/v1/namespaces/default/pods/web/exec").await;
    assert_eq!(code, 400, "{body}");
    assert!(body.contains("command"), "{body}");
}

#[tokio::test]
async fn assigned_pod_logs_read_kubelet_container_log_file() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let agent = Agent::connect(config(&s, dir.path(), true)).await.unwrap();
    let _agent = Running(tokio::spawn(async move {
        agent.run().await.unwrap();
    }));
    healthy(&s).await;
    let (code, created) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/default/pods",
            pod_spec("web"),
        )
        .await;
    assert_eq!(code, 201, "{created}");
    let (code, bound) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/default/pods/web/binding",
            json!({"apiVersion":"v1","kind":"Binding","metadata":{"name":"web","namespace":"default"},"target":{"apiVersion":"v1","kind":"Node","name":"worker"}}),
        )
        .await;
    assert_eq!(code, 201, "{bound}");
    let (code, mut pod) = s
        .json(
            s.admin(),
            "GET",
            "/api/v1/namespaces/default/pods/web",
            json!({}),
        )
        .await;
    assert_eq!(code, 200, "{pod}");
    let uid = pod["metadata"]["uid"].as_str().unwrap().to_owned();
    pod["status"]["containerStatuses"] = json!([{
        "name":"web",
        "image":"example.invalid/web:v1",
        "imageID":"example.invalid/web@sha256:0",
        "containerID":"containerd://abcdef0123456789",
        "ready":true,
        "restartCount":0,
        "started":true,
        "state":{"running":{"startedAt":"2026-09-08T00:00:00Z"}}
    }]);
    let (code, status) = s
        .json(
            s.admin(),
            "PUT",
            "/api/v1/namespaces/default/pods/web/status",
            pod,
        )
        .await;
    assert_eq!(code, 200, "{status}");
    let log = dir
        .path()
        .join("state/agent/pods")
        .join(&uid)
        .join("logs/web-0.log");
    fs::create_dir_all(log.parent().unwrap()).unwrap();
    fs::write(
        &log,
        "2026-09-08T00:00:00.000000000Z stdout F ready from cri\n",
    )
    .unwrap();
    let (code, body) = text(&s, "GET", "/api/v1/namespaces/default/pods/web/log").await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body, "ready from cri\n");
    let (code, body) = text(
        &s,
        "GET",
        "/api/v1/namespaces/default/pods/web/log?timestamps=true",
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert!(
        body.contains("2026-09-08T00:00:00.000000000Z ready from cri"),
        "{body}"
    );
}

#[tokio::test]
async fn assigned_pod_rest_exec_forwards_stub_kubelet_json() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let agent = Agent::connect(config(&s, dir.path(), false)).await.unwrap();
    agent.reconcile().await.unwrap();

    let (key, csr) = h3s_certs::node_key_and_csr().unwrap();
    let identity =
        Identity::from_pem(s.pki.sign_kubelet_csr("worker", &csr).unwrap(), key).unwrap();
    let tls = h3s_certs::kubelet_server_config(s.pki.ca_pem(), &identity, "worker").unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let _backend = Running(tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let tls = acceptor.accept(socket).await.unwrap();
        let peer = &tls.get_ref().1.peer_certificates().unwrap()[0];
        let user = h3s_auth::User::from_verified_certificate(peer.as_ref()).unwrap();
        assert_eq!(user.name, h3s_api::KUBELET_CLIENT_ID);
        let seen_tx = Arc::new(std::sync::Mutex::new(Some(seen_tx)));
        let handler =
            hyper::service::service_fn(move |request: hyper::Request<hyper::body::Incoming>| {
                let seen_tx = seen_tx.clone();
                async move {
                    assert!(!request.headers().contains_key("authorization"));
                    let method = request.method().as_str().to_owned();
                    let path = request.uri().path().to_owned();
                    let query = request.uri().query().unwrap_or("").to_owned();
                    seen_tx
                        .lock()
                        .unwrap()
                        .take()
                        .unwrap()
                        .send((method.clone(), path.clone(), query))
                        .unwrap();
                    let (status, content_type, body) = if method == "POST" && path == "/exec" {
                        (
                            200,
                            "application/json",
                            Bytes::from_static(
                                br#"{"stdout":"ok\n","stderr":"warning\n","exitCode":0}"#,
                            ),
                        )
                    } else {
                        (
                            404,
                            "text/plain; charset=utf-8",
                            Bytes::from_static(b"unexpected request\n"),
                        )
                    };
                    Ok::<_, std::convert::Infallible>(
                        hyper::Response::builder()
                            .status(status)
                            .header("content-type", content_type)
                            .body(Full::new(body))
                            .unwrap(),
                    )
                }
            });
        hyper::server::conn::http1::Builder::new()
            .serve_connection(TokioIo::new(tls), handler)
            .await
            .unwrap();
    }));
    let url = s.endpoint().replace("https://", "wss://") + "/v1-h3s/connect";
    let tunnel_tls = Arc::new(node_client(&s, dir.path()));
    let _tunnel = Running(tokio::spawn(async move {
        h3s_supervisor::run_worker(&url, tunnel_tls, address)
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

    let (code, created) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/default/pods",
            pod_spec("exec"),
        )
        .await;
    assert_eq!(code, 201, "{created}");
    let (code, bound) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/default/pods/exec/binding",
            json!({"apiVersion":"v1","kind":"Binding","metadata":{"name":"exec","namespace":"default"},"target":{"apiVersion":"v1","kind":"Node","name":"worker"}}),
        )
        .await;
    assert_eq!(code, 201, "{bound}");
    let (code, mut pod) = s
        .json(
            s.admin(),
            "GET",
            "/api/v1/namespaces/default/pods/exec",
            json!({}),
        )
        .await;
    assert_eq!(code, 200, "{pod}");
    pod["status"]["containerStatuses"] = json!([{
        "name":"web",
        "image":"example.invalid/web:v1",
        "imageID":"example.invalid/web@sha256:0",
        "containerID":"containerd://container-123",
        "ready":true,
        "restartCount":0,
        "started":true,
        "state":{"running":{"startedAt":"2026-09-08T00:00:00Z"}}
    }]);
    let (code, status) = s
        .json(
            s.admin(),
            "PUT",
            "/api/v1/namespaces/default/pods/exec/status",
            pod,
        )
        .await;
    assert_eq!(code, 200, "{status}");

    let (code, body) = text(
        &s,
        "POST",
        "/api/v1/namespaces/default/pods/exec/exec?container=web&stdout=true&stderr=true&timeoutSeconds=7&command=sh&command=-c&command=printf+ok",
    )
    .await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap(),
        json!({"stdout":"ok\n","stderr":"warning\n","exitCode":0})
    );

    let (method, path, query) = seen_rx.await.unwrap();
    assert_eq!(method, "POST");
    assert_eq!(path, "/exec");
    let query: Vec<(String, String)> = serde_urlencoded::from_str(&query).unwrap();
    assert!(query
        .iter()
        .any(|pair| pair == &("containerId".into(), "container-123".into())));
    assert!(query
        .iter()
        .any(|pair| pair == &("timeoutSeconds".into(), "7".into())));
    assert_eq!(
        query
            .iter()
            .filter(|(key, _)| key == "command")
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>(),
        ["sh", "-c", "printf ok"]
    );
}
