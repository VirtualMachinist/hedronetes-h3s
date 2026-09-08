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

struct Server {
    address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    pki: ClusterPki,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Server {
    async fn start(dir: &std::path::Path) -> Self {
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
    fn admin(&self) -> rustls::ClientConfig {
        self.pki.client_config(Some(self.pki.admin())).unwrap()
    }
    async fn raw(
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
            .header("Host", "localhost")
            .header("Content-Type", "application/json");
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
    async fn json(
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
    async fn namespace(&self, name: &str) {
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
    async fn configmap(&self, name: &str, value: &str) -> Value {
        let (status,v)=self.json(self.admin(),"POST","/api/v1/namespaces/team-a/configmaps",json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":name},"data":{"value":value}})).await;
        assert_eq!(status, 201, "{v}");
        v
    }
}
#[tokio::test]
async fn tls_auth_and_namespace_rbac_precede_resource_access() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let path = "/api/v1/namespaces/team-a/configmaps";
    let (status, v) = s
        .json(s.pki.client_config(None).unwrap(), "GET", path, json!({}))
        .await;
    assert_eq!(status, 401);
    assert_eq!(v["kind"], "Status");
    let response = s
        .raw(
            s.pki.client_config(None).unwrap(),
            "GET",
            path,
            json!({}),
            &[("X-Remote-User", "h3s-admin")],
        )
        .await;
    assert_eq!(response.status(), 401);
    let alice = s.pki.issue_client("alice", Some("developers")).unwrap();
    let client = || s.pki.client_config(Some(&alice)).unwrap();
    assert_eq!(s.json(client(), "GET", "/api", json!({})).await.0, 200);
    assert_eq!(s.json(client(), "GET", path, json!({})).await.0, 403);
    let role = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{"name":"reader"},"rules":[{"verbs":["get","list","watch"],"apiGroups":[""],"resources":["configmaps"]}]});
    assert_eq!(
        s.json(
            s.admin(),
            "POST",
            "/apis/rbac.authorization.k8s.io/v1/namespaces/team-a/roles",
            role
        )
        .await
        .0,
        201
    );
    let binding = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding","metadata":{"name":"reader"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":"reader"},"subjects":[{"kind":"Group","apiGroup":"rbac.authorization.k8s.io","name":"developers"}]});
    assert_eq!(
        s.json(
            s.admin(),
            "POST",
            "/apis/rbac.authorization.k8s.io/v1/namespaces/team-a/rolebindings",
            binding
        )
        .await
        .0,
        201
    );
    assert_eq!(s.json(client(), "GET", path, json!({})).await.0, 200);
    assert_eq!(s.json(client(), "POST", path, json!({})).await.0, 403);
    assert_eq!(
        s.json(
            client(),
            "GET",
            "/api/v1/namespaces/default/configmaps",
            json!({})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        s.raw(
            client(),
            "GET",
            path,
            json!({}),
            &[("Impersonate-User", "h3s-admin")]
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        s.raw(
            s.admin(),
            "GET",
            path,
            json!({}),
            &[("Authorization", "Bearer invalid")]
        )
        .await
        .status(),
        401
    );
}
#[tokio::test]
async fn durable_crud_conflicts_delete_preconditions_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let created = s.configmap("settings", "one").await;
    let path = "/api/v1/namespaces/team-a/configmaps/settings";
    let mut changed = created.clone();
    changed["data"]["value"] = "two".into();
    let (code, updated) = s.json(s.admin(), "PUT", path, changed).await;
    assert_eq!(code, 200);
    assert_eq!(updated["metadata"]["uid"], created["metadata"]["uid"]);
    assert_ne!(
        updated["metadata"]["resourceVersion"],
        created["metadata"]["resourceVersion"]
    );
    assert_eq!(s.json(s.admin(), "PUT", path, created).await.0, 409);
    let ca = s.pki.ca_pem().to_owned();
    drop(s);
    tokio::task::yield_now().await;
    let s = Server::start(dir.path()).await;
    assert_eq!(ca, s.pki.ca_pem());
    let (code, reopened) = s.json(s.admin(), "GET", path, json!({})).await;
    assert_eq!(code, 200);
    assert_eq!(reopened, updated);
    assert_eq!(
        s.json(
            s.admin(),
            "DELETE",
            path,
            json!({"preconditions":{"uid":"wrong"}})
        )
        .await
        .0,
        409
    );
    assert_eq!(
        s.json(
            s.admin(),
            "DELETE",
            path,
            json!({"preconditions":{"uid":reopened["metadata"]["uid"]}})
        )
        .await
        .0,
        200
    );
    assert_eq!(s.json(s.admin(), "GET", path, json!({})).await.0, 404);
}
#[tokio::test]
async fn paginated_list_keeps_snapshot_and_watch_replays_json_events() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    s.configmap("a", "original").await;
    s.configmap("b", "original").await;
    let path = "/api/v1/namespaces/team-a/configmaps";
    let (status, page) = s
        .json(s.admin(), "GET", &format!("{path}?limit=1"), json!({}))
        .await;
    assert_eq!(status, 200);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let rv = page["metadata"]["resourceVersion"].as_str().unwrap();
    let created = s.configmap("c", "later").await;
    let token = page["metadata"]["continue"].as_str().unwrap();
    let (status, next) = s
        .json(
            s.admin(),
            "GET",
            &format!("{path}?limit=1&continue={token}"),
            json!({}),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(next["metadata"]["resourceVersion"], rv);
    assert_eq!(next["items"][0]["metadata"]["name"], "b");
    assert_eq!(next["metadata"]["continue"], "");
    let response = s
        .raw(
            s.admin(),
            "GET",
            &format!("{path}?watch=true&resourceVersion={rv}&timeoutSeconds=1"),
            json!({}),
            &[],
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let events: Vec<Value> = String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "ADDED");
    assert_eq!(events[0]["object"], created);
    assert_eq!(
        s.json(
            s.admin(),
            "GET",
            &format!("{path}?labelSelector=app%3Dweb"),
            json!({})
        )
        .await
        .0,
        400
    );
}
#[tokio::test]
async fn validation_and_secret_write_only_string_data() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let path = "/api/v1/namespaces/team-a/secrets";
    let secret = json!({"apiVersion":"v1","kind":"Secret","metadata":{"name":"test-secret"},"stringData":{"token":"synthetic-test-value"}});
    let (code, value) = s.json(s.admin(), "POST", path, secret).await;
    assert_eq!(code, 201, "{value}");
    assert!(value.get("stringData").is_none());
    assert_eq!(value["data"]["token"], "c3ludGhldGljLXRlc3QtdmFsdWU=");
    let bad =
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bad","namespace":"other"}});
    assert_eq!(
        s.json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/team-a/configmaps",
            bad
        )
        .await
        .0,
        400
    );
    let bad = json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bad"},"data":{"invalid":123}});
    assert_eq!(
        s.json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/team-a/configmaps",
            bad
        )
        .await
        .0,
        422
    );
}
