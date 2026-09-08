//! High-entropy bootstrap secrets. Only fixed-size digests are compared.
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

pub fn random_secret() -> Result<String, getrandom::Error> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
pub fn digest(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}
pub fn matches(expected: &[u8; 32], supplied: &str) -> bool {
    bool::from(expected.ct_eq(&digest(supplied)))
}
pub fn valid_token(token: &str) -> bool {
    (32..=256).contains(&token.len()) && token.bytes().all(|b| b.is_ascii_graphic())
}
pub fn valid_password(password: &str) -> bool {
    password.len() == 64 && password.bytes().all(|b| b.is_ascii_hexdigit())
}

/// CLI/environment token wrapper. Debug and Display never reveal its value.
#[derive(Clone)]
pub struct Token(String);
impl Token {
    pub fn expose(&self) -> &str {
        &self.0
    }
}
impl std::str::FromStr for Token {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.into()))
    }
}
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted token]")
    }
}
impl std::fmt::Display for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[redacted token]")
    }
}
