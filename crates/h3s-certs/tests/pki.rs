use h3s_certs::ClusterPki;
use rustls::pki_types::ServerName;
use std::{fs, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::{TlsAcceptor, TlsConnector};

fn names() -> Vec<String> {
    vec![
        "localhost".into(),
        "127.0.0.1".into(),
        "kubernetes.default.svc".into(),
    ]
}
fn pki(root: &tempfile::TempDir) -> ClusterPki {
    ClusterPki::open_or_create(&root.path().join("pki"), &names()).unwrap()
}

async fn exchange(
    server: rustls::ServerConfig,
    client: rustls::ClientConfig,
    name: &'static str,
) -> (bool, bool, bool) {
    let (a, b) = tokio::io::duplex(16384);
    let server = async move {
        let mut stream = TlsAcceptor::from(Arc::new(server)).accept(a).await?;
        let authenticated = stream
            .get_ref()
            .1
            .peer_certificates()
            .is_some_and(|c| !c.is_empty());
        assert_eq!(stream.read_u8().await?, 7);
        stream.write_u8(9).await?;
        stream.flush().await?;
        Ok::<_, std::io::Error>(authenticated)
    };
    let client = async move {
        let mut stream = TlsConnector::from(Arc::new(client))
            .connect(ServerName::try_from(name).unwrap(), b)
            .await?;
        stream.write_u8(7).await?;
        stream.flush().await?;
        assert_eq!(stream.read_u8().await?, 9);
        Ok::<_, std::io::Error>(())
    };
    let (a, b) = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(server, client)
    })
    .await
    .unwrap();
    (a.is_ok(), b.is_ok(), a.unwrap_or(false))
}

#[tokio::test]
async fn verified_tls_allows_admin_and_absent_identity_but_rejects_foreign_or_wrong_eku() {
    let root = tempfile::tempdir().unwrap();
    let cluster = pki(&root);
    assert_eq!(
        exchange(
            cluster.server_config().unwrap(),
            cluster.client_config(Some(cluster.admin())).unwrap(),
            "localhost"
        )
        .await,
        (true, true, true)
    );
    assert_eq!(
        exchange(
            cluster.server_config().unwrap(),
            cluster.client_config(None).unwrap(),
            "127.0.0.1"
        )
        .await,
        (true, true, false)
    );
    let other_root = tempfile::tempdir().unwrap();
    let other = pki(&other_root);
    let (server_ok, client_ok, _) = exchange(
        cluster.server_config().unwrap(),
        cluster.client_config(Some(other.admin())).unwrap(),
        "localhost",
    )
    .await;
    assert!(!server_ok && !client_ok);
    let serving = cluster.issue_serving("worker", &["worker".into()]).unwrap();
    let (server_ok, client_ok, _) = exchange(
        cluster.server_config().unwrap(),
        cluster.client_config(Some(&serving)).unwrap(),
        "localhost",
    )
    .await;
    assert!(!server_ok && !client_ok);
    let (server_ok, client_ok, _) = exchange(
        cluster.server_config().unwrap(),
        cluster.client_config(None).unwrap(),
        "wrong-host",
    )
    .await;
    assert!(!server_ok && !client_ok);
}

#[test]
fn reopen_preserves_trust_and_checks_requested_sans_and_private_modes() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let first = pki(&root);
    let second = pki(&root);
    assert_eq!(first.ca_pem(), second.ca_pem());
    assert_eq!(
        first.admin().certificate_pem(),
        second.admin().certificate_pem()
    );
    let dir = root.path().join("pki");
    let file = dir.join("cluster-pki.json");
    let before = fs::read(&file).unwrap();
    assert_eq!(
        fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(ClusterPki::open_or_create(&dir, &["changed.example".into()]).is_err());
    assert_eq!(before, fs::read(&file).unwrap());
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(ClusterPki::open_or_create(&dir, &names()).is_err());
    assert_eq!(before, fs::read(&file).unwrap());
}

#[test]
fn corrupt_future_and_mismatched_bundles_are_not_reinitialized() {
    for scenario in ["truncated", "future", "key-mismatch", "expired"] {
        let root = tempfile::tempdir().unwrap();
        let _ = pki(&root);
        let dir = root.path().join("pki");
        let file = dir.join("cluster-pki.json");
        let mut data: serde_json::Value =
            serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
        match scenario {
            "future" => data["version"] = 999.into(),
            "key-mismatch" => {
                data["admin"]["private_key_pem"] = data["server"]["private_key_pem"].clone()
            }
            "expired" => {
                let key = rcgen::KeyPair::generate().unwrap();
                let issuer = rcgen::Issuer::from_ca_cert_pem(
                    data["ca"]["certificate_pem"].as_str().unwrap(),
                    rcgen::KeyPair::from_pem(data["ca"]["private_key_pem"].as_str().unwrap())
                        .unwrap(),
                )
                .unwrap();
                let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
                params.not_before = rcgen::date_time_ymd(2020, 1, 1);
                params.not_after = rcgen::date_time_ymd(2021, 1, 1);
                params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ClientAuth];
                data["admin"]["certificate_pem"] =
                    params.signed_by(&key, &issuer).unwrap().pem().into();
                data["admin"]["private_key_pem"] = key.serialize_pem().into();
            }
            _ => {}
        }
        let bytes = if scenario == "truncated" {
            b"{partial".to_vec()
        } else {
            serde_json::to_vec(&data).unwrap()
        };
        fs::write(&file, &bytes).unwrap();
        assert!(
            ClusterPki::open_or_create(&dir, &names()).is_err(),
            "{scenario}"
        );
        assert_eq!(fs::read(&file).unwrap(), bytes, "{scenario}");
    }
}

#[test]
fn concurrent_bootstrap_keeps_one_ca() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("pki");
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let dir = dir.clone();
            std::thread::spawn(move || {
                ClusterPki::open_or_create(&dir, &names())
                    .unwrap()
                    .ca_pem()
                    .to_owned()
            })
        })
        .collect();
    let cas: Vec<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert!(cas.iter().all(|c| c == &cas[0]));
}

#[test]
fn symlinks_are_rejected_and_kubeconfig_embeds_verified_material() {
    let root = tempfile::tempdir().unwrap();
    let cluster = pki(&root);
    std::os::unix::fs::symlink(root.path().join("pki"), root.path().join("alias")).unwrap();
    assert!(ClusterPki::open_or_create(&root.path().join("alias"), &names()).is_err());
    let cfg: serde_json::Value = serde_json::from_str(
        &cluster
            .kubeconfig("https://127.0.0.1:6443", cluster.admin())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        cfg["clusters"][0]["cluster"]["server"],
        "https://127.0.0.1:6443"
    );
    assert!(cfg["clusters"][0]["cluster"]
        .get("insecure-skip-tls-verify")
        .is_none());
    assert!(cluster
        .kubeconfig("http://localhost", cluster.admin())
        .is_err());
    assert!(cluster.issue_client("", None).is_err());
}

fn strict_verify(root: &std::path::Path, ca: &str, certificate: &str, purpose: &str) -> bool {
    fs::write(root.join("verify-ca.pem"), ca).unwrap();
    fs::write(root.join("verify-leaf.pem"), certificate).unwrap();
    let mut command = std::process::Command::new("openssl");
    command
        .args(["verify", "-x509_strict", "-purpose", purpose, "-CAfile"])
        .arg(root.join("verify-ca.pem"));
    if purpose == "sslserver" {
        command.args(["-verify_hostname", "localhost"]);
    }
    command
        .arg(root.join("verify-leaf.pem"))
        .output()
        .expect("OpenSSL is required for the independent strict X.509 regression")
        .status
        .success()
}

#[test]
fn issued_leaf_certificates_pass_independent_strict_x509_validation() {
    let root = tempfile::tempdir().unwrap();
    let cluster = pki(&root);
    for identity in [
        cluster
            .issue_client("system:node:worker", Some("system:nodes"))
            .unwrap(),
        cluster.issue_client("scoped-operator", None).unwrap(),
    ] {
        assert!(strict_verify(
            root.path(),
            cluster.ca_pem(),
            identity.certificate_pem(),
            "sslclient"
        ));
    }
    assert!(strict_verify(
        root.path(),
        cluster.ca_pem(),
        cluster.admin().certificate_pem(),
        "sslclient"
    ));
    let serving = cluster
        .issue_serving("worker", &["localhost".into()])
        .unwrap();
    assert!(strict_verify(
        root.path(),
        cluster.ca_pem(),
        serving.certificate_pem(),
        "sslserver"
    ));
}

#[test]
fn valid_legacy_serving_leaf_is_repaired_once_without_changing_trust_key_sans_or_expiry() {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    use x509_parser::prelude::{FromDer, X509Certificate};
    let root = tempfile::tempdir().unwrap();
    let _ = pki(&root);
    let dir = root.path().join("pki");
    let file = dir.join("cluster-pki.json");
    let mut bundle: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    let der = CertificateDer::from_pem_slice(
        bundle["server"]["certificate_pem"]
            .as_str()
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    let (_, original) = X509Certificate::from_der(der.as_ref()).unwrap();
    let expiry = original.validity().not_after.to_datetime();
    let mut params = rcgen::CertificateParams::new(names()).unwrap();
    params.distinguished_name = rcgen::DistinguishedName::new();
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "h3s-apiserver");
    params.not_before = original.validity().not_before.to_datetime();
    params.not_after = expiry;
    params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
    params.use_authority_key_identifier_extension = false;
    let issuer = rcgen::Issuer::from_ca_cert_pem(
        bundle["ca"]["certificate_pem"].as_str().unwrap(),
        rcgen::KeyPair::from_pem(bundle["ca"]["private_key_pem"].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    let key =
        rcgen::KeyPair::from_pem(bundle["server"]["private_key_pem"].as_str().unwrap()).unwrap();
    let legacy = params.signed_by(&key, &issuer).unwrap().pem();
    assert!(!strict_verify(
        root.path(),
        bundle["ca"]["certificate_pem"].as_str().unwrap(),
        &legacy,
        "sslserver"
    ));
    bundle["server"]["certificate_pem"] = legacy.clone().into();
    fs::write(&file, serde_json::to_vec(&bundle).unwrap()).unwrap();
    let workers: Vec<_> = (0..6)
        .map(|_| {
            let dir = dir.clone();
            std::thread::spawn(move || {
                // Repair must preserve even SANs not requested by this caller.
                let _ = ClusterPki::open_or_create(&dir, &["localhost".into()]).unwrap();
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let repaired: serde_json::Value = serde_json::from_slice(&fs::read(&file).unwrap()).unwrap();
    assert_eq!(repaired["ca"], bundle["ca"]);
    assert_eq!(repaired["admin"], bundle["admin"]);
    assert_eq!(
        repaired["server"]["private_key_pem"],
        bundle["server"]["private_key_pem"]
    );
    let cert = repaired["server"]["certificate_pem"].as_str().unwrap();
    assert_ne!(cert, legacy);
    assert!(strict_verify(
        root.path(),
        repaired["ca"]["certificate_pem"].as_str().unwrap(),
        cert,
        "sslserver"
    ));
    let der = CertificateDer::from_pem_slice(cert.as_bytes()).unwrap();
    let (_, parsed) = X509Certificate::from_der(der.as_ref()).unwrap();
    assert_eq!(parsed.validity().not_after.to_datetime(), expiry);
    let before = fs::read(&file).unwrap();
    let _ = ClusterPki::open_or_create(&dir, &names()).unwrap();
    assert_eq!(before, fs::read(&file).unwrap());
}

#[test]
fn coredns_client_replaces_three_day_lifetime() {
    let root = tempfile::tempdir().unwrap();
    let cluster = pki(&root);
    let short = cluster
        .issue_client_with_lifetime("system:coredns", None, time::Duration::days(3))
        .unwrap();
    let short_left = short.not_after().unwrap() - time::OffsetDateTime::now_utc();
    assert!(short_left <= time::Duration::days(4));
    let durable = cluster.issue_coredns_client().unwrap();
    assert_eq!(
        durable.common_name().unwrap(),
        ClusterPki::COREDNS_CLIENT_NAME
    );
    let durable_left = durable.not_after().unwrap() - time::OffsetDateTime::now_utc();
    assert!(durable_left >= time::Duration::days(360));
    let renewed = cluster
        .renew_client(&short, time::Duration::days(365))
        .unwrap();
    assert_eq!(renewed.common_name().unwrap(), "system:coredns");
    assert!(renewed.not_after().unwrap() - time::OffsetDateTime::now_utc() >= time::Duration::days(360));
    assert!(strict_verify(
        root.path(),
        cluster.ca_pem(),
        durable.certificate_pem(),
        "sslclient"
    ));
}
