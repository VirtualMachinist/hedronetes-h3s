#![allow(dead_code)]
use axum::body::Bytes;
use h3s_apiserver::Api;
use h3s_certs::ClusterPki;
use h3s_storage::SqliteStore;
use http_body_util::{BodyExt, Full};
use hyper::{body::Incoming, Request, Response};
use hyper_util::rt::TokioIo;
use rustls::pki_types::ServerName;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;

pub const JOIN_TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

pub struct Server {
    controller: Option<tokio::task::JoinHandle<Result<(), h3s_controllers::Error>>>,
    address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    pub pki: Arc<ClusterPki>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(controller) = &self.controller {
            controller.abort();
        }
    }
}
impl Server {
    pub async fn start(dir: &std::path::Path) -> Self {
        Self::start_mode(dir, false).await
    }
    pub async fn start_with_controllers(dir: &std::path::Path) -> Self {
        Self::start_mode(dir, true).await
    }
    pub async fn restart(mut self, dir: &std::path::Path) -> Self {
        let address = self.address;
        self.task.abort();
        let _ = (&mut self.task).await;
        if let Some(controller) = &mut self.controller {
            controller.abort();
            let _ = controller.await;
        }
        drop(self);
        Self::start_at(dir, false, Some(address)).await
    }
    async fn start_mode(dir: &std::path::Path, controllers: bool) -> Self {
        Self::start_at(dir, controllers, None).await
    }
    async fn start_at(
        dir: &std::path::Path,
        controllers: bool,
        address: Option<std::net::SocketAddr>,
    ) -> Self {
        let pki = Arc::new(
            ClusterPki::open_or_create(&dir.join("tls"), &["localhost".into(), "127.0.0.1".into()])
                .unwrap(),
        );
        let store = Arc::new(SqliteStore::open(dir.join("registry.db")).await.unwrap());
        let api = Api::new(store)
            .await
            .unwrap()
            .with_bootstrap(pki.clone(), JOIN_TOKEN)
            .unwrap();
        let listener = TcpListener::bind(address.unwrap_or_else(|| "127.0.0.1:0".parse().unwrap()))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(h3s_apiserver::serve(
            listener,
            pki.server_config().unwrap(),
            api.router(),
            std::future::pending(),
        ));
        let controller = if controllers {
            let identity = pki
                .issue_client(h3s_controllers::NAMESPACE_CONTROLLER_ID, None)
                .unwrap();
            let config = pki
                .kubeconfig(&format!("https://{address}"), &identity)
                .unwrap();
            let client = h3s_controllers::client_from_kubeconfig(&config)
                .await
                .unwrap();
            Some(tokio::spawn(h3s_controllers::run_namespace_controller(
                client,
                pki.ca_pem().to_owned(),
            )))
        } else {
            None
        };
        let server = Self {
            address,
            task,
            pki,
            controller,
        };
        for ns in ["default", "kube-system", "kube-public", "kube-node-lease"] {
            server.prepare_account(ns).await;
        }
        server
    }
    async fn prepare_account(&self, ns: &str) {
        if self.controller.is_some() {
            self.wait_account(ns).await;
        } else {
            // API contract tests control their registry fixture. Controller
            // integration tests opt into real background reconciliation instead.
            let (code,value)=self.json(self.admin(), "POST", &format!("/api/v1/namespaces/{ns}/serviceaccounts"), json!({"apiVersion":"v1","kind":"ServiceAccount","metadata":{"name":"default"}})).await;
            assert!(matches!(code, 201 | 409), "{value}");
        }
    }
    pub async fn wait_account(&self, ns: &str) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if self
                    .json(
                        self.admin(),
                        "GET",
                        &format!("/api/v1/namespaces/{ns}/serviceaccounts/default"),
                        json!({}),
                    )
                    .await
                    .0
                    == 200
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("default account controller did not converge");
    }
    pub fn endpoint(&self) -> String {
        format!("https://{}", self.address)
    }
    pub fn admin(&self) -> rustls::ClientConfig {
        self.pki.client_config(Some(self.pki.admin())).unwrap()
    }
    pub async fn raw(
        &self,
        config: rustls::ClientConfig,
        method: &str,
        path: &str,
        body: Value,
        headers: &[(&str, &str)],
    ) -> Response<Incoming> {
        self.raw_bytes(
            config,
            method,
            path,
            serde_json::to_vec(&body).unwrap(),
            headers,
        )
        .await
    }
    pub async fn raw_bytes(
        &self,
        config: rustls::ClientConfig,
        method: &str,
        path: &str,
        body: Vec<u8>,
        headers: &[(&str, &str)],
    ) -> Response<Incoming> {
        let socket = TcpStream::connect(self.address).await.unwrap();
        let tls = TlsConnector::from(Arc::new(config))
            .connect(ServerName::try_from("localhost").unwrap(), socket)
            .await
            .unwrap();
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(tls))
            .await
            .unwrap();
        tokio::spawn(async move {
            let _ = connection.await;
        });
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("Host", "localhost");
        if !headers
            .iter()
            .any(|(key, _)| key.eq_ignore_ascii_case("content-type"))
        {
            req = req.header("Content-Type", "application/json");
        }
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        sender
            .send_request(req.body(Full::new(Bytes::from(body))).unwrap())
            .await
            .unwrap()
    }
    pub async fn json(
        &self,
        config: rustls::ClientConfig,
        method: &str,
        path: &str,
        body: Value,
    ) -> (u16, Value) {
        let response = self.raw(config, method, path, body, &[]).await;
        let status = response.status().as_u16();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }
    pub async fn namespace(&self, name: &str) {
        assert_eq!(
            self.json(
                self.admin(),
                "POST",
                "/api/v1/namespaces",
                json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":name}})
            )
            .await
            .0,
            201
        );
        self.prepare_account(name).await;
    }
    pub async fn patch(
        &self,
        config: rustls::ClientConfig,
        path: &str,
        content_type: &str,
        value: Value,
    ) -> (u16, Value) {
        let response = self
            .raw(
                config,
                "PATCH",
                path,
                value,
                &[("Content-Type", content_type)],
            )
            .await;
        let code = response.status().as_u16();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (code, serde_json::from_slice(&body).unwrap())
    }
    pub async fn configmap(&self, name: &str, value: &str) -> Value {
        let (status,v)=self.json(self.admin(),"POST","/api/v1/namespaces/team-a/configmaps",json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":name},"data":{"value":value}})).await;
        assert_eq!(status, 201, "{v}");
        v
    }
}
