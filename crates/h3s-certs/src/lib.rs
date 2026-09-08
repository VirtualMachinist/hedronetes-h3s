//! Persistent cluster PKI. TLS validates certificate chains before the HTTP
//! layer extracts identities. Issuance is an administrative operation.
use base64::{engine::general_purpose::STANDARD, Engine};
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::{
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer, ServerName, UnixTime},
    server::WebPkiClientVerifier,
    RootCertStore,
};
use serde::{Deserialize, Serialize};
use std::{fs, io::Write, path::Path, sync::Arc};
use time::{Duration, OffsetDateTime};
use x509_parser::prelude::{FromDer, X509Certificate};

pub mod private;

pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("PKI I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid PKI bundle: {0}")]
    Invalid(String),
    #[error("certificate generation: {0}")]
    Generate(#[from] rcgen::Error),
    #[error("TLS configuration: {0}")]
    Tls(#[from] rustls::Error),
    #[error("PKI serialization: {0}")]
    Json(#[from] serde_json::Error),
}

/// Contains a private key; deliberately does not implement Debug.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    certificate_pem: String,
    private_key_pem: String,
}
impl Identity {
    /// Parse a client-held key/certificate pair; callers must separately verify
    /// its CA chain and expected subject before using it as a node identity.
    pub fn from_pem(certificate_pem: String, private_key_pem: String) -> Result<Self> {
        let identity = Self {
            certificate_pem,
            private_key_pem,
        };
        identity.validate_key_pair()?;
        Ok(identity)
    }

    pub fn certificate_pem(&self) -> &str {
        &self.certificate_pem
    }
    pub fn certificate_der(&self) -> Result<CertificateDer<'static>> {
        CertificateDer::from_pem_slice(self.certificate_pem.as_bytes())
            .map_err(|_| Error::Invalid("certificate PEM".into()))
    }
    fn private_key_der(&self) -> Result<PrivateKeyDer<'static>> {
        PrivateKeyDer::from_pem_slice(self.private_key_pem.as_bytes())
            .map_err(|_| Error::Invalid("private key PEM".into()))
    }
    fn validate_key_pair(&self) -> Result<()> {
        rustls::sign::CertifiedKey::from_der(
            vec![self.certificate_der()?],
            self.private_key_der()?,
            &provider(),
        )?;
        Ok(())
    }
}

/// Atomically persisted in the runtime data directory, never in the Nix store.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterPki {
    version: u32,
    ca: Identity,
    server: Identity,
    admin: Identity,
}
fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::aws_lc_rs::default_provider())
}
impl ClusterPki {
    /// Corrupt, expired, insecure or incompatible existing material fails closed.
    /// A valid legacy serving leaf missing AKI is reissued atomically under a
    /// bundle lock, preserving CA, private key, subject, SANs and expiry. Other
    /// material is retained. Concurrent bootstrap uses create-if-absent.
    pub fn open_or_create(directory: &Path, server_names: &[String]) -> Result<Self> {
        private_directory(directory)?;
        // Serialize creation and the narrowly scoped legacy serving-cert repair.
        // The separate lock inode is stable across atomic bundle replacement.
        let _lock = bundle_lock(directory)?;
        let path = directory.join("cluster-pki.json");
        if path.try_exists()? {
            let mut pki = Self::load(&path, server_names)?;
            if pki.repair_legacy_serving_certificate()? {
                pki.validate(server_names)?;
                let mut tmp = tempfile::NamedTempFile::new_in(directory)?;
                serde_json::to_writer(tmp.as_file_mut(), &pki)?;
                tmp.as_file_mut().write_all(b"\n")?;
                tmp.as_file().sync_all()?;
                tmp.persist(&path).map_err(|e| Error::Io(e.error))?;
                fs::File::open(directory)?.sync_all()?;
            }
            return Ok(pki);
        }
        let pki = Self::generate(server_names)?;
        let mut tmp = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer(tmp.as_file_mut(), &pki)?;
        tmp.as_file_mut().write_all(b"\n")?;
        tmp.as_file().sync_all()?;
        match tmp.persist_noclobber(&path) {
            Ok(_) => {
                fs::File::open(directory)?.sync_all()?;
            }
            Err(e) if e.error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Self::load(&path, server_names)
            }
            Err(e) => return Err(Error::Io(e.error)),
        }
        Ok(pki)
    }
    fn load(path: &Path, server_names: &[String]) -> Result<Self> {
        let mut opts = fs::OpenOptions::new();
        opts.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.custom_flags(libc::O_NOFOLLOW);
        }
        let file = opts.open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > 1024 * 1024 {
            return Err(Error::Invalid(
                "bundle must be a regular file under 1 MiB".into(),
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(Error::Invalid("bundle requires mode 0600".into()));
            }
        }
        let pki: Self = serde_json::from_reader(file)?;
        if pki.version != 1 {
            return Err(Error::Invalid("unsupported bundle version".into()));
        }
        pki.validate(server_names)?;
        Ok(pki)
    }
    fn generate(server_names: &[String]) -> Result<Self> {
        if server_names.is_empty() {
            return Err(Error::Invalid("serving SAN required".into()));
        }
        let mut params = parameters("h3s-cluster-ca", None, &[], Duration::days(3650))?;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        let key = KeyPair::generate()?;
        let cert = params.self_signed(&key)?;
        let issuer = Issuer::from_params(&params, &key);
        let server = issue(
            &issuer,
            "h3s-apiserver",
            None,
            server_names,
            ExtendedKeyUsagePurpose::ServerAuth,
        )?;
        let admin = issue(
            &issuer,
            "h3s-admin",
            Some("system:masters"),
            &[],
            ExtendedKeyUsagePurpose::ClientAuth,
        )?;
        let pki = Self {
            version: 1,
            ca: Identity {
                certificate_pem: cert.pem(),
                private_key_pem: key.serialize_pem(),
            },
            server,
            admin,
        };
        pki.validate(server_names)?;
        Ok(pki)
    }
    fn repair_legacy_serving_certificate(&mut self) -> Result<bool> {
        use x509_parser::extensions::{GeneralName, ParsedExtension};
        let der = self.server.certificate_der()?;
        let (_, cert) = X509Certificate::from_der(der.as_ref())
            .map_err(|_| Error::Invalid("serving DER".into()))?;
        if cert.extensions().iter().any(|e| {
            matches!(
                e.parsed_extension(),
                ParsedExtension::AuthorityKeyIdentifier(_)
            )
        }) {
            return Ok(false);
        }
        let names: Vec<_> = cert.subject().iter_common_name().collect();
        if names.len() != 1
            || names[0].as_str().ok() != Some("h3s-apiserver")
            || cert.subject().iter_organization().next().is_some()
        {
            return Err(Error::Invalid(
                "legacy serving subject cannot be repaired automatically".into(),
            ));
        }
        let mut sans = Vec::new();
        let san = cert
            .subject_alternative_name()
            .map_err(|_| Error::Invalid("legacy serving SAN".into()))?
            .ok_or_else(|| Error::Invalid("legacy serving SAN absent".into()))?;
        for name in &san.value.general_names {
            sans.push(match name {
                GeneralName::DNSName(name) => (*name).to_owned(),
                GeneralName::IPAddress(bytes) if bytes.len() == 4 => {
                    std::net::Ipv4Addr::from(<[u8; 4]>::try_from(*bytes).unwrap()).to_string()
                }
                GeneralName::IPAddress(bytes) if bytes.len() == 16 => {
                    std::net::Ipv6Addr::from(<[u8; 16]>::try_from(*bytes).unwrap()).to_string()
                }
                _ => {
                    return Err(Error::Invalid(
                        "legacy serving SAN cannot be repaired automatically".into(),
                    ))
                }
            });
        }
        let mut params = parameters("h3s-apiserver", None, &sans, Duration::days(365))?;
        params.not_before = cert.validity().not_before.to_datetime();
        params.not_after = cert.validity().not_after.to_datetime();
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        params.serial_number = Some(uuid::Uuid::new_v4().as_bytes().to_vec().into());
        let key = KeyPair::from_pem(&self.server.private_key_pem)?;
        self.server.certificate_pem = params.signed_by(&key, &self.issuer()?)?.pem();
        Ok(true)
    }
    fn issuer(&self) -> Result<Issuer<'static, KeyPair>> {
        Ok(Issuer::from_ca_cert_pem(
            &self.ca.certificate_pem,
            KeyPair::from_pem(&self.ca.private_key_pem)?,
        )?)
    }
    fn validate(&self, server_names: &[String]) -> Result<()> {
        self.ca.validate_key_pair()?;
        self.server.validate_key_pair()?;
        self.admin.validate_key_pair()?;
        let ca_der = self.ca.certificate_der()?;
        let (_, ca) = X509Certificate::from_der(ca_der.as_ref())
            .map_err(|_| Error::Invalid("CA DER".into()))?;
        if !ca.is_ca() || !ca.validity().is_valid() {
            return Err(Error::Invalid("CA constraint or validity".into()));
        }
        ca.verify_signature(None)
            .map_err(|_| Error::Invalid("CA self-signature".into()))?;
        let roots = Arc::new(self.roots()?);
        let verifier = WebPkiClientVerifier::builder_with_provider(roots.clone(), provider())
            .build()
            .map_err(|e| Error::Invalid(e.to_string()))?;
        verifier.verify_client_cert(&self.admin.certificate_der()?, &[], UnixTime::now())?;
        let verifier =
            rustls::client::WebPkiServerVerifier::builder_with_provider(roots, provider())
                .build()
                .map_err(|e| Error::Invalid(e.to_string()))?;
        use rustls::client::danger::ServerCertVerifier;
        if server_names.is_empty() {
            return Err(Error::Invalid("serving SAN required".into()));
        }
        for name in server_names {
            let name = ServerName::try_from(name.as_str())
                .map_err(|_| Error::Invalid("serving SAN".into()))?;
            verifier.verify_server_cert(
                &self.server.certificate_der()?,
                &[],
                &name,
                &[],
                UnixTime::now(),
            )?;
        }
        Ok(())
    }
    pub fn ca_pem(&self) -> &str {
        self.ca.certificate_pem()
    }
    pub fn admin(&self) -> &Identity {
        &self.admin
    }
    pub fn roots(&self) -> Result<RootCertStore> {
        let mut roots = RootCertStore::empty();
        roots.add(self.ca.certificate_der()?)?;
        Ok(roots)
    }
    /// The authorized join handler chooses node subjects; joining clients cannot
    /// supply arbitrary usernames/groups.
    pub fn issue_client(&self, username: &str, group: Option<&str>) -> Result<Identity> {
        issue(
            &self.issuer()?,
            username,
            group,
            &[],
            ExtendedKeyUsagePurpose::ClientAuth,
        )
    }
    /// Verify CSR possession, then replace every requested certificate parameter
    /// with the server's node policy. CSR subjects, CA bits, SANs and usages
    /// never select privileges. Private keys remain on the worker.
    pub fn sign_node_csr(&self, node: &str, csr_pem: &str) -> Result<String> {
        if !h3s_api::valid_node_name(node) || csr_pem.len() > 8192 {
            return Err(Error::Invalid("invalid node name or CSR length".into()));
        }
        let mut csr = rcgen::CertificateSigningRequestParams::from_pem(csr_pem)?;
        let mut params = parameters(
            &format!("system:node:{node}"),
            Some("system:nodes"),
            &[],
            Duration::days(365),
        )?;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.use_authority_key_identifier_extension = true;
        params.serial_number = Some(uuid::Uuid::new_v4().as_bytes().to_vec().into());
        csr.params = params;
        Ok(csr.signed_by(&self.issuer()?)?.pem())
    }
    /// Kubelet leaves use a dedicated name space, never a submitted node/IP SAN.
    pub fn sign_kubelet_csr(&self, node: &str, csr_pem: &str) -> Result<String> {
        if !h3s_api::valid_node_name(node) || csr_pem.len() > 8192 {
            return Err(Error::Invalid("invalid kubelet CSR/name".into()));
        }
        let mut csr = rcgen::CertificateSigningRequestParams::from_pem(csr_pem)?;
        let mut params = parameters(
            &format!("system:node:{node}"),
            None,
            &[kubelet_dns_name(node)],
            Duration::days(365),
        )?;
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.use_authority_key_identifier_extension = true;
        params.serial_number = Some(uuid::Uuid::new_v4().as_bytes().to_vec().into());
        csr.params = params;
        Ok(csr.signed_by(&self.issuer()?)?.pem())
    }
    pub fn issue_serving(&self, name: &str, sans: &[String]) -> Result<Identity> {
        if sans.is_empty() {
            return Err(Error::Invalid("serving SAN required".into()));
        }
        issue(
            &self.issuer()?,
            name,
            None,
            sans,
            ExtendedKeyUsagePurpose::ServerAuth,
        )
    }
    /// Missing certificates are allowed for health/version and bearer auth.
    /// Invalid presented certificates still fail TLS. HTTP must authorize requests.
    pub fn server_config(&self) -> Result<rustls::ServerConfig> {
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(self.roots()?), provider())
                .allow_unauthenticated()
                .build()
                .map_err(|e| Error::Invalid(e.to_string()))?;
        let mut config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_client_cert_verifier(verifier)
            .with_single_cert(
                vec![self.server.certificate_der()?],
                self.server.private_key_der()?,
            )?;
        config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(config)
    }
    pub fn client_config(&self, identity: Option<&Identity>) -> Result<rustls::ClientConfig> {
        let builder = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_root_certificates(self.roots()?);
        Ok(match identity {
            Some(id) => {
                builder.with_client_auth_cert(vec![id.certificate_der()?], id.private_key_der()?)?
            }
            None => builder.with_no_client_auth(),
        })
    }
    /// JSON is valid kubeconfig YAML. Caller must install this secret with 0600.
    pub fn kubeconfig(&self, endpoint: &str, identity: &Identity) -> Result<String> {
        if !endpoint.starts_with("https://") || endpoint.contains(['\n', '\r', '#', '@']) {
            return Err(Error::Invalid(
                "kubeconfig requires HTTPS without userinfo/fragment".into(),
            ));
        }
        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "apiVersion":"v1", "kind":"Config",
            "clusters":[{"name":"h3s","cluster":{"server":endpoint,"certificate-authority-data":STANDARD.encode(self.ca_pem())}}],
            "users":[{"name":"h3s-user","user":{"client-certificate-data":STANDARD.encode(identity.certificate_pem()),"client-key-data":STANDARD.encode(&identity.private_key_pem)}}],
            "contexts":[{"name":"h3s","context":{"cluster":"h3s","user":"h3s-user","namespace":"default"}}],
            "current-context":"h3s"
        }))?)
    }
}
fn parameters(
    name: &str,
    group: Option<&str>,
    sans: &[String],
    lifetime: Duration,
) -> Result<CertificateParams> {
    if name.is_empty()
        || name.len() > 1024
        || name.chars().any(char::is_control)
        || group.is_some_and(|g| g.is_empty() || g.len() > 253 || g.chars().any(char::is_control))
    {
        return Err(Error::Invalid("certificate subject".into()));
    }
    let mut p = CertificateParams::new(sans.to_vec())?;
    p.distinguished_name = rcgen::DistinguishedName::new();
    p.distinguished_name.push(DnType::CommonName, name);
    if let Some(group) = group {
        p.distinguished_name.push(DnType::OrganizationName, group);
    }
    p.not_before = OffsetDateTime::now_utc() - Duration::minutes(5);
    p.not_after = OffsetDateTime::now_utc() + lifetime;
    p.key_usages = vec![KeyUsagePurpose::DigitalSignature];
    Ok(p)
}
fn issue(
    issuer: &Issuer<impl rcgen::SigningKey>,
    name: &str,
    group: Option<&str>,
    sans: &[String],
    usage: ExtendedKeyUsagePurpose,
) -> Result<Identity> {
    let mut params = parameters(name, group, sans, Duration::days(365))?;
    params.extended_key_usages = vec![usage];
    params.use_authority_key_identifier_extension = true;
    let key = KeyPair::generate()?;
    let cert = params.signed_by(&key, issuer)?;
    Ok(Identity {
        certificate_pem: cert.pem(),
        private_key_pem: key.serialize_pem(),
    })
}
fn private_directory(path: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.file_type().is_symlink() {
        return Err(Error::Invalid("PKI directory must not be a symlink".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if m.permissions().mode() & 0o077 != 0 {
            return Err(Error::Invalid("PKI directory requires mode 0700".into()));
        }
    }
    Ok(())
}

fn bundle_lock(directory: &Path) -> Result<fs::File> {
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(directory.join(".pki.lock"))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(Error::Invalid("PKI lock must be a regular file".into()));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(Error::Invalid("PKI lock requires mode 0600".into()));
        }
    }
    fs2::FileExt::lock_exclusive(&file)?;
    Ok(file)
}

/// Generate a worker private key and signed CSR. Never log the returned key.
pub fn node_key_and_csr() -> Result<(String, String)> {
    let key = KeyPair::generate()?;
    let params = CertificateParams::new(Vec::<String>::new())?;
    Ok((key.serialize_pem(), params.serialize_request(&key)?.pem()?))
}

/// Validate a worker identity against the configured CA and exact node subject.
pub fn node_client_config(
    ca_pem: &str,
    identity: &Identity,
    node: &str,
) -> Result<rustls::ClientConfig> {
    let mut roots = RootCertStore::empty();
    let ca = CertificateDer::from_pem_slice(ca_pem.as_bytes())
        .map_err(|_| Error::Invalid("node CA PEM".into()))?;
    roots.add(ca)?;
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots.clone()), provider())
        .build()
        .map_err(|_| Error::Invalid("node CA".into()))?;
    let der = identity.certificate_der()?;
    verifier.verify_client_cert(&der, &[], UnixTime::now())?;
    let (_, cert) = X509Certificate::from_der(der.as_ref())
        .map_err(|_| Error::Invalid("node certificate DER".into()))?;
    let cn: Vec<_> = cert.subject().iter_common_name().collect();
    let groups: Vec<_> = cert.subject().iter_organization().collect();
    if cn.len() != 1
        || cn[0].as_str().ok() != Some(format!("system:node:{node}").as_str())
        || groups.len() != 1
        || groups[0].as_str().ok() != Some("system:nodes")
    {
        return Err(Error::Invalid("unexpected node certificate subject".into()));
    }
    Ok(rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_client_auth_cert(vec![der], identity.private_key_der()?)?)
}

/// Stable, non-resolving private TLS name. Hash labels fit DNS limits even for a
/// maximum-length Node name and cannot collide with ordinary control-plane SANs.
pub fn kubelet_dns_name(node: &str) -> String {
    use sha2::{Digest, Sha256};
    let hex: String = Sha256::digest(node.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("node-{}.{}.h3s.invalid", &hex[..32], &hex[32..])
}
pub fn kubelet_server_name(node: &str) -> ServerName<'static> {
    ServerName::try_from(kubelet_dns_name(node)).expect("fixed hashed DNS name")
}
/// Validate the worker-held serving key/leaf against cluster trust and the
/// expected node-specific TLS name before starting its localhost listener.
pub fn kubelet_server_config(
    ca_pem: &str,
    identity: &Identity,
    node: &str,
) -> Result<rustls::ServerConfig> {
    use rustls::client::danger::ServerCertVerifier;
    let mut roots = RootCertStore::empty();
    roots.add(
        CertificateDer::from_pem_slice(ca_pem.as_bytes())
            .map_err(|_| Error::Invalid("kubelet CA PEM".into()))?,
    )?;
    let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
        Arc::new(roots.clone()),
        provider(),
    )
    .build()
    .map_err(|_| Error::Invalid("kubelet CA".into()))?;
    let der = identity.certificate_der()?;
    verifier.verify_server_cert(&der, &[], &kubelet_server_name(node), &[], UnixTime::now())?;
    let client_verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider())
        .build()
        .map_err(|_| Error::Invalid("kubelet client trust".into()))?;
    Ok(rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(client_verifier)
        .with_single_cert(vec![der], identity.private_key_der()?)?)
}
