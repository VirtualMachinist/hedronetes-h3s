//! Shared wire types for the native h3s bootstrap protocol.
use serde::{Deserialize, Serialize};
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
/// Contains a node password. Deliberately does not implement Debug.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinRequest {
    pub node_name: String,
    pub password: String,
    pub csr_pem: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinResponse {
    pub certificate_pem: String,
}

pub fn valid_node_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label.as_bytes()[0].is_ascii_alphanumeric()
                && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

/// Serving certificate request authenticated by the existing node client identity.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServingRequest {
    pub csr_pem: String,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServingResponse {
    pub certificate_pem: String,
}
/// Only this API-server client identity may call the private kubelet HTTP API.
pub const KUBELET_CLIENT_ID: &str = "system:h3s:kubelet-client";
