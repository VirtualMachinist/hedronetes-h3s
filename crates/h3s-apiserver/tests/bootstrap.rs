mod common;
use common::{Server, JOIN_TOKEN};
use h3s_certs::Identity;
use h3s_kubelet::{Agent, Config};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use std::{fs, path::Path};

fn request(name: &str, password: &str, csr: &str) -> Value {
    json!({"node_name":name,"password":password,"csr_pem":csr})
}
async fn join(s: &Server, token: &str, body: Value) -> (u16, Value) {
    let response = s
        .raw(
            s.pki.client_config(None).unwrap(),
            "POST",
            "/v1-h3s/join",
            body,
            &[("authorization", &format!("Bearer {token}"))],
        )
        .await;
    let code = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&bytes).unwrap())
}
fn config(s: &Server, root: &Path, token: bool) -> Config {
    fs::write(root.join("ca.pem"), s.pki.ca_pem()).unwrap();
    Config {
        server: s.endpoint(),
        kubelet_port: 0,
        runtime_endpoint: None,
        ca_file: root.join("ca.pem"),
        node_name: "worker".into(),
        node_ip: "192.0.2.2".parse().unwrap(),
        data_dir: root.join("node-state"),
        token: token.then(|| JOIN_TOKEN.into()),
    }
}

#[tokio::test]
async fn bootstrap_verifies_token_csr_and_overrides_requested_privileges() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let password = h3s_auth::bootstrap::random_secret().unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    let mut params = rcgen::CertificateParams::new(vec!["admin.example".into()]).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "h3s-admin");
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "system:masters");
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    let csr = params.serialize_request(&key).unwrap().pem().unwrap();
    let body = request("worker", &password, &csr);
    let (code, error) = join(&s, "invalid-token", body.clone()).await;
    assert_eq!(code, 401);
    assert!(!error.to_string().contains(&password));
    assert_eq!(
        join(
            &s,
            JOIN_TOKEN,
            request("worker", &"b".repeat(64), "not-a-csr")
        )
        .await
        .0,
        400
    );
    assert_eq!(
        join(&s, JOIN_TOKEN, json!({"padding":"x".repeat(17 * 1024)}))
            .await
            .0,
        413
    );
    let mut unknown = body.clone();
    unknown["groups"] = json!(["system:masters"]);
    assert_eq!(join(&s, JOIN_TOKEN, unknown).await.0, 400);
    let (code, response) = join(&s, JOIN_TOKEN, body).await;
    assert_eq!(code, 201, "{response}");
    assert_eq!(response.as_object().unwrap().len(), 1);
    assert!(!response.to_string().contains("PRIVATE KEY"));
    let identity = Identity::from_pem(
        response["certificate_pem"].as_str().unwrap().into(),
        key.serialize_pem(),
    )
    .unwrap();
    let tls = h3s_certs::node_client_config(s.pki.ca_pem(), &identity, "worker").unwrap();
    let user =
        h3s_auth::User::from_verified_certificate(identity.certificate_der().unwrap().as_ref())
            .unwrap();
    assert_eq!(user.node_name(), Some("worker"));
    assert!(!user.is_superuser());
    assert_eq!(
        s.json(tls, "GET", "/api/v1/namespaces/default/secrets", json!({}))
            .await
            .0,
        403
    );
    assert_eq!(
        s.json(
            s.admin(),
            "GET",
            "/api/v1/h3s-node-identities/worker",
            json!({})
        )
        .await
        .0,
        404
    );
    let duplicate = s
        .raw(
            s.pki.client_config(None).unwrap(),
            "POST",
            "/v1-h3s/join",
            request("worker", &password, &csr),
            &[
                ("authorization", &format!("Bearer {JOIN_TOKEN}")),
                ("authorization", "Bearer wrong"),
            ],
        )
        .await;
    assert_eq!(duplicate.status(), 401);
    let impersonated = s
        .raw(
            s.pki.client_config(None).unwrap(),
            "POST",
            "/v1-h3s/join",
            request("worker", &password, &csr),
            &[
                ("authorization", &format!("Bearer {JOIN_TOKEN}")),
                ("impersonate-user", "h3s-admin"),
            ],
        )
        .await;
    assert_eq!(impersonated.status(), 403);
}

#[tokio::test]
async fn enrollment_name_password_survives_restart_and_concurrent_claims() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let (_, csr) = h3s_certs::node_key_and_csr().unwrap();
    let a = h3s_auth::bootstrap::random_secret().unwrap();
    let b = h3s_auth::bootstrap::random_secret().unwrap();
    let (one, two) = tokio::join!(
        join(&s, JOIN_TOKEN, request("worker", &a, &csr)),
        join(&s, JOIN_TOKEN, request("worker", &b, &csr))
    );
    let mut codes = vec![one.0, two.0];
    codes.sort();
    assert_eq!(codes, vec![201, 401]);
    let (winner, loser) = if one.0 == 201 { (&a, &b) } else { (&b, &a) };
    let s = s.restart(root.path()).await;
    assert_eq!(
        join(&s, JOIN_TOKEN, request("worker", loser, &csr)).await.0,
        401
    );
    assert_eq!(
        join(&s, JOIN_TOKEN, request("worker", winner, &csr))
            .await
            .0,
        200
    );
    let n = json!({"apiVersion":"v1","kind":"Node","metadata":{"name":"existing"}});
    assert_eq!(s.json(s.admin(), "POST", "/api/v1/nodes", n).await.0, 201);
    assert_eq!(
        join(&s, JOIN_TOKEN, request("existing", &a, &csr)).await.0,
        409
    );
    for name in ["", "../worker", "WORKER", "worker/other", "worker:admin"] {
        assert_eq!(
            join(&s, JOIN_TOKEN, request(name, &a, &csr)).await.0,
            400,
            "{name}"
        );
    }
}

#[tokio::test]
async fn real_agent_enrolls_reconciles_resumes_and_recovers_server_restart() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let agent_root = tempfile::tempdir().unwrap();
    let agent = Agent::connect(config(&s, agent_root.path(), true))
        .await
        .unwrap();
    agent.reconcile().await.unwrap();
    let (_, node) = s
        .json(s.admin(), "GET", "/api/v1/nodes/worker", json!({}))
        .await;
    let (_, lease) = s
        .json(
            s.admin(),
            "GET",
            "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/worker",
            json!({}),
        )
        .await;
    assert_eq!(node["status"]["conditions"][0]["status"], "False");
    assert_eq!(node["status"]["conditions"][0]["reason"], "RuntimeNotReady");
    assert_eq!(node["status"]["addresses"][0]["address"], "192.0.2.2");
    assert_eq!(lease["spec"]["holderIdentity"], "worker");
    assert!(
        Agent::connect(config(&s, agent_root.path(), true))
            .await
            .is_err(),
        "second process lock must fail"
    );
    let file = agent_root.path().join("node-state/agent/identity.json");
    let bytes = fs::read(&file).unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    drop(agent);
    let agent = Agent::connect(config(&s, agent_root.path(), false))
        .await
        .unwrap();
    agent.reconcile().await.unwrap();
    assert_eq!(bytes, fs::read(&file).unwrap());
    let s = s.restart(root.path()).await;
    agent.reconcile().await.unwrap();
    let (_, after) = s
        .json(s.admin(), "GET", "/api/v1/nodes/worker", json!({}))
        .await;
    let (_, renewed) = s
        .json(
            s.admin(),
            "GET",
            "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/worker",
            json!({}),
        )
        .await;
    assert_eq!(after["metadata"]["uid"], node["metadata"]["uid"]);
    assert_eq!(renewed["metadata"]["uid"], lease["metadata"]["uid"]);
    assert_ne!(
        renewed["metadata"]["resourceVersion"],
        lease["metadata"]["resourceVersion"]
    );
    assert_eq!(bytes, fs::read(&file).unwrap());
    assert_eq!(
        s.json(s.admin(), "DELETE", "/api/v1/nodes/worker", json!({}))
            .await
            .0,
        200
    );
    agent.reconcile().await.unwrap();
    let (_, replacement) = s
        .json(s.admin(), "GET", "/api/v1/nodes/worker", json!({}))
        .await;
    let (_, repaired) = s
        .json(
            s.admin(),
            "GET",
            "/apis/coordination.k8s.io/v1/namespaces/kube-node-lease/leases/worker",
            json!({}),
        )
        .await;
    assert_ne!(replacement["metadata"]["uid"], node["metadata"]["uid"]);
    assert_eq!(repaired["metadata"]["uid"], lease["metadata"]["uid"]);
    assert_eq!(
        repaired["metadata"]["ownerReferences"][0]["uid"],
        replacement["metadata"]["uid"]
    );
}

#[tokio::test]
async fn agent_rejects_invalid_token_ca_identity_rebinding_and_insecure_origins() {
    let root = tempfile::tempdir().unwrap();
    let s = Server::start(root.path()).await;
    let agent_root = tempfile::tempdir().unwrap();
    let mut c = config(&s, agent_root.path(), true);
    c.token = Some("wrong-token-with-at-least-thirty-two-characters".into());
    assert!(matches!(
        Agent::connect(c).await,
        Err(h3s_kubelet::Error::Status(401))
    ));
    let file = agent_root.path().join("node-state/agent/identity.json");
    let pending: Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert!(pending["certificate_pem"].is_null());
    let agent = Agent::connect(config(&s, agent_root.path(), true))
        .await
        .unwrap();
    drop(agent);
    let bytes = fs::read(&file).unwrap();
    let mut changed = config(&s, agent_root.path(), false);
    changed.node_name = "renamed".into();
    assert!(Agent::connect(changed).await.is_err());
    assert_eq!(bytes, fs::read(&file).unwrap());
    let other = tempfile::tempdir().unwrap();
    let foreign = Server::start(other.path()).await;
    let mut changed = config(&s, agent_root.path(), false);
    fs::write(&changed.ca_file, foreign.pki.ca_pem()).unwrap();
    assert!(Agent::connect(changed).await.is_err());
    assert_eq!(bytes, fs::read(&file).unwrap());
    for server in [
        "http://localhost:6443",
        "https://user:password@localhost",
        "https://localhost/api",
        "https://localhost?token=x",
    ] {
        let mut changed = config(&s, agent_root.path(), false);
        changed.server = server.into();
        assert!(Agent::connect(changed).await.is_err(), "{server}");
    }
    let fresh = tempfile::tempdir().unwrap();
    changed = config(&s, fresh.path(), true);
    fs::write(&changed.ca_file, foreign.pki.ca_pem()).unwrap();
    assert!(matches!(
        Agent::connect(changed).await,
        Err(h3s_kubelet::Error::Transport(_))
    ));
}
