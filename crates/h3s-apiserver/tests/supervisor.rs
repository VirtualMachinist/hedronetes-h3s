mod common;
use common::{Server, JOIN_TOKEN};
use futures_util::SinkExt;
use h3s_certs::Identity;
use h3s_kubelet::{Agent, Config};
use serde_json::{json, Value};
use std::{fs, io, net::SocketAddr, path::Path, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinSet,
};
use tokio_tungstenite::{
    connect_async_tls_with_config,
    tungstenite::{client::IntoClientRequest, Message},
    Connector,
};

struct Running(tokio::task::JoinHandle<()>);
impl Drop for Running {
    fn drop(&mut self) {
        self.0.abort();
    }
}
async fn wait_for(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}
async fn enrolled(s: &Server, dir: &Path) -> (Agent, Arc<rustls::ClientConfig>) {
    fs::write(dir.join("ca.pem"), s.pki.ca_pem()).unwrap();
    let agent = Agent::connect(Config {
        server: s.endpoint(),
        kubelet_port: 0,
        runtime_endpoint: None,
        service_proxy_nft: None,
        cluster_dns: None,
        ca_file: dir.join("ca.pem"),
        node_name: "worker".into(),
        node_ip: "192.0.2.2".parse().unwrap(),
        data_dir: dir.join("agent"),
        token: Some(JOIN_TOKEN.into()),
    })
    .await
    .unwrap();
    agent.reconcile().await.unwrap();
    let state: Value =
        serde_json::from_slice(&fs::read(dir.join("agent/agent/identity.json")).unwrap()).unwrap();
    let identity = Identity::from_pem(
        state["certificate_pem"].as_str().unwrap().into(),
        state["private_key_pem"].as_str().unwrap().into(),
    )
    .unwrap();
    let tls = Arc::new(h3s_certs::node_client_config(s.pki.ca_pem(), &identity, "worker").unwrap());
    (agent, tls)
}
fn worker(s: &Server, tls: Arc<rustls::ClientConfig>, target: SocketAddr) -> Running {
    let url = s.endpoint().replace("https://", "wss://") + "/v1-h3s/connect";
    Running(tokio::spawn(async move {
        h3s_supervisor::run_worker(&url, tls, target).await.unwrap();
    }))
}
async fn echo() -> (SocketAddr, Running) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut tasks = JoinSet::new();
        loop {
            tokio::select! {
                accepted=listener.accept()=>{
                    let (mut socket,_)=accepted.unwrap();
                    tasks.spawn(async move {let (mut r,mut w)=socket.split();let _=tokio::io::copy(&mut r,&mut w).await;});
                },
                Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
            }
        }
    });
    (addr, Running(task))
}
fn headers() -> Vec<(&'static str, &'static str)> {
    vec![
        ("connection", "upgrade"),
        ("upgrade", "websocket"),
        ("sec-websocket-version", "13"),
        ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
        ("sec-websocket-protocol", h3s_supervisor::PROTOCOL),
    ]
}
#[tokio::test]
async fn supervisor_authentication_requires_native_enrolled_node_and_single_connection() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let (_agent, tls) = enrolled(&s, dir.path()).await;
    let unregistered = s
        .pki
        .issue_client("system:node:stranger", Some("system:nodes"))
        .unwrap();
    for (client, expected) in [
        (s.pki.client_config(None).unwrap(), 401),
        (s.admin(), 403),
        (s.pki.client_config(Some(&unregistered)).unwrap(), 403),
    ] {
        let r = s
            .raw(client, "GET", "/v1-h3s/connect", json!({}), &headers())
            .await;
        assert_eq!(r.status(), expected);
    }
    for (path, extra, expected) in [
        ("/v1-h3s/connect?node=other", None, 400),
        (
            "/v1-h3s/connect",
            Some(("origin", "https://example.com")),
            400,
        ),
        (
            "/v1-h3s/connect",
            Some(("authorization", "Bearer wrong")),
            401,
        ),
        (
            "/v1-h3s/connect",
            Some(("impersonate-user", "system:node:other")),
            403,
        ),
    ] {
        let mut h = headers();
        if let Some(e) = extra {
            h.push(e);
        }
        assert_eq!(
            s.raw((*tls).clone(), "GET", path, json!({}), &h)
                .await
                .status(),
            expected
        );
    }
    let mut bad = headers();
    bad.pop();
    bad.push(("sec-websocket-protocol", "h3s.tunnel.v0"));
    assert_eq!(
        s.raw((*tls).clone(), "GET", "/v1-h3s/connect", json!({}), &bad)
            .await
            .status(),
        400
    );
    let (target, _echo) = echo().await;
    let _worker = worker(&s, tls.clone(), target);
    wait_for(|| s.supervisor.connected("worker")).await;
    assert!(!s.supervisor.connected("other"));
    assert_eq!(
        s.raw(
            (*tls).clone(),
            "GET",
            "/v1-h3s/connect",
            json!({}),
            &headers()
        )
        .await
        .status(),
        409
    );
    assert_eq!(
        s.supervisor.open_kubelet("other").await.unwrap_err().kind(),
        io::ErrorKind::NotConnected
    );
}

#[tokio::test]
async fn tunnel_multiplexes_large_duplex_streams_and_half_close_without_cross_talk() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let (_agent, tls) = enrolled(&s, dir.path()).await;
    let (target, _echo) = echo().await;
    let _worker = worker(&s, tls, target);
    wait_for(|| s.supervisor.connected("worker")).await;
    let mut tasks = JoinSet::new();
    for tag in 0..4u8 {
        let mut stream = s.supervisor.open_kubelet("worker").await.unwrap();
        tasks.spawn(async move {
            let data = vec![tag; 1024 * 1024 + 37];
            let (mut read, mut write) = tokio::io::split(&mut stream);
            let sent = async {
                write.write_all(&data).await.unwrap();
                write.shutdown().await.unwrap();
            };
            let received = async {
                let mut output = Vec::new();
                read.read_to_end(&mut output).await.unwrap();
                assert_eq!(output, data);
            };
            tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(sent, received);
            })
            .await
            .unwrap();
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap();
    }
    // Per-connection and per-stream backpressure must not destroy later opens.
    let mut stream = s.supervisor.open_kubelet("worker").await.unwrap();
    stream.write_all(b"still-open").await.unwrap();
    let mut data = [0; 10];
    stream.read_exact(&mut data).await.unwrap();
    assert_eq!(&data, b"still-open");
}

#[tokio::test]
async fn tunnel_disconnect_releases_node_and_active_streams_and_refuses_unavailable_target() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let (_agent, tls) = enrolled(&s, dir.path()).await;
    let (target, _echo) = echo().await;
    let running = worker(&s, tls.clone(), target);
    wait_for(|| s.supervisor.connected("worker")).await;
    let mut stream = s.supervisor.open_kubelet("worker").await.unwrap();
    running.0.abort();
    wait_for(|| !s.supervisor.connected("worker")).await;
    let mut buffer = [0];
    let result = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buffer))
        .await
        .unwrap();
    assert!(matches!(result, Ok(0) | Err(_)));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unavailable = listener.local_addr().unwrap();
    drop(listener);
    let _worker = worker(&s, tls, unavailable);
    wait_for(|| s.supervisor.connected("worker")).await;
    assert_eq!(
        s.supervisor
            .open_kubelet("worker")
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::ConnectionRefused
    );
    assert!(s.supervisor.connected("worker"));
}
#[tokio::test]
async fn actual_agent_reconnects_after_api_restart_and_shutdown_cleans_upgrade_tasks() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let (agent, _) = enrolled(&s, dir.path()).await;
    let _run = Running(tokio::spawn(async move {
        agent.run().await.unwrap();
    }));
    wait_for(|| s.supervisor.connected("worker")).await;
    let old = s.supervisor.clone();
    let before = s
        .json(s.admin(), "GET", "/api/v1/nodes/worker", json!({}))
        .await
        .1;
    let s = s.restart(root.path()).await;
    wait_for(|| !old.connected("worker")).await;
    wait_for(|| s.supervisor.connected("worker")).await;
    let after = s
        .json(s.admin(), "GET", "/api/v1/nodes/worker", json!({}))
        .await
        .1;
    assert_eq!(before["metadata"]["uid"], after["metadata"]["uid"]);
    assert_eq!(
        after["status"]["conditions"][0]["reason"],
        "RuntimeNotReady"
    );
}

#[tokio::test]
async fn malformed_websocket_messages_close_connection_and_release_registration() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let (_agent, tls) = enrolled(&s, dir.path()).await;
    for message in [
        Message::Text("not tunnel bytes".into()),
        Message::Binary(vec![0; h3s_supervisor::socket::MAX_FRAME + 1].into()),
    ] {
        let mut request = (s.endpoint().replace("https://", "wss://") + "/v1-h3s/connect")
            .into_client_request()
            .unwrap();
        request.headers_mut().insert(
            "sec-websocket-protocol",
            h3s_supervisor::PROTOCOL.parse().unwrap(),
        );
        let (mut ws, _) = connect_async_tls_with_config(
            request,
            None,
            true,
            Some(Connector::Rustls(tls.clone())),
        )
        .await
        .unwrap();
        wait_for(|| s.supervisor.connected("worker")).await;
        ws.send(message).await.unwrap();
        wait_for(|| !s.supervisor.connected("worker")).await;
    }
}

#[tokio::test]
async fn concurrent_stream_admission_preserves_existing_traffic_and_recovers_capacity() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let dir = tempfile::tempdir().unwrap();
    let (_agent, tls) = enrolled(&s, dir.path()).await;
    let (target, _echo) = echo().await;
    let _worker = worker(&s, tls, target);
    wait_for(|| s.supervisor.connected("worker")).await;
    let mut streams = Vec::new();
    for _ in 0..h3s_supervisor::MAX_OPEN_STREAMS {
        streams.push(s.supervisor.open_kubelet("worker").await.unwrap());
    }
    assert_eq!(
        s.supervisor
            .open_kubelet("worker")
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    streams[0].write_all(b"a").await.unwrap();
    assert_eq!(streams[0].read_u8().await.unwrap(), b'a');
    drop(streams);
    for _ in 0..32 {
        let mut stream = s.supervisor.open_kubelet("worker").await.unwrap();
        stream.write_all(b"b").await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), b'b');
    }
    assert!(s.supervisor.connected("worker"));
}
