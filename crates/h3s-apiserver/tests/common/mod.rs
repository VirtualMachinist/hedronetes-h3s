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

pub struct Server {
    address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    pub pki: ClusterPki,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    pub async fn start(dir: &std::path::Path) -> Self {
        let pki =
            ClusterPki::open_or_create(&dir.join("tls"), &["localhost".into(), "127.0.0.1".into()])
                .unwrap();
        let store = Arc::new(SqliteStore::open(dir.join("registry.db")).await.unwrap());
        let api = Api::new(store).await.unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(h3s_apiserver::serve(
            listener,
            pki.server_config().unwrap(),
            api.router(),
            std::future::pending(),
        ));
        Self { address, task, pki }
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
            .send_request(
                req.body(Full::new(Bytes::from(serde_json::to_vec(&body).unwrap())))
                    .unwrap(),
            )
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
