//! X.509 identity extraction and Kubernetes v1.34 RBAC evaluation.
//!
//! The API supplies an authorization snapshot read through its storage boundary.
//! This crate neither reads the database nor grants permissions from HTTP headers.
mod node;
pub use node::{node_allows, node_label_allowed};

use k8s_openapi::api::rbac::v1::{
    ClusterRole, ClusterRoleBinding, PolicyRule, Role, RoleBinding, Subject,
};
use x509_parser::prelude::{FromDer, X509Certificate};

pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
const RBAC_GROUP: &str = "rbac.authorization.k8s.io";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct User {
    pub name: String,
    pub groups: Vec<String>,
}
#[derive(Debug, thiserror::Error)]
#[error("client certificate must contain one nonempty CN and valid organization names")]
pub struct InvalidIdentity;
impl User {
    /// Only call after rustls has verified the certificate chain, validity,
    /// ClientAuth EKU, and proof of possession during the TLS handshake.
    pub fn from_verified_certificate(der: &[u8]) -> Result<Self, InvalidIdentity> {
        let (remaining, certificate) =
            X509Certificate::from_der(der).map_err(|_| InvalidIdentity)?;
        if !remaining.is_empty() {
            return Err(InvalidIdentity);
        }
        let names: Vec<_> = certificate.subject().iter_common_name().collect();
        if names.len() != 1 {
            return Err(InvalidIdentity);
        }
        let name = names[0].as_str().map_err(|_| InvalidIdentity)?;
        if name.is_empty() || name.chars().any(char::is_control) {
            return Err(InvalidIdentity);
        }
        let mut groups = Vec::new();
        for org in certificate.subject().iter_organization() {
            let org = org.as_str().map_err(|_| InvalidIdentity)?;
            if org.is_empty() || org.chars().any(char::is_control) {
                return Err(InvalidIdentity);
            }
            if !groups.iter().any(|g| g == org) {
                groups.push(org.to_owned());
            }
        }
        if !groups.iter().any(|g| g == "system:authenticated") {
            groups.push("system:authenticated".into());
        }
        Ok(Self {
            name: name.into(),
            groups,
        })
    }
    pub fn is_superuser(&self) -> bool {
        self.groups.iter().any(|g| g == "system:masters")
    }
    pub fn node_name(&self) -> Option<&str> {
        self.groups
            .iter()
            .any(|g| g == "system:nodes")
            .then(|| self.name.strip_prefix("system:node:"))
            .flatten()
            .filter(|s| !s.is_empty())
    }
}

#[derive(Clone, Debug)]
pub struct ResourceRequest<'a> {
    pub verb: &'a str,
    pub group: &'a str,
    pub resource: &'a str,
    pub subresource: Option<&'a str>,
    pub namespace: Option<&'a str>,
    pub name: Option<&'a str>,
}
#[derive(Clone, Debug)]
pub enum Request<'a> {
    Resource(ResourceRequest<'a>),
    NonResource { verb: &'a str, path: &'a str },
}

/// Immutable per-request view. Missing/invalid role references yield no grant.
#[derive(Default)]
pub struct Rbac {
    pub roles: Vec<Role>,
    pub cluster_roles: Vec<ClusterRole>,
    pub role_bindings: Vec<RoleBinding>,
    pub cluster_role_bindings: Vec<ClusterRoleBinding>,
}
impl Rbac {
    pub fn allows(&self, user: &User, request: &Request<'_>) -> bool {
        if user.is_superuser() {
            return true;
        }
        for binding in &self.cluster_role_bindings {
            if binding.role_ref.api_group != RBAC_GROUP
                || binding.role_ref.kind != "ClusterRole"
                || !subjects_match(binding.subjects.as_deref().unwrap_or_default(), user, None)
            {
                continue;
            }
            if let Some(role) = self
                .cluster_roles
                .iter()
                .find(|r| r.metadata.name.as_deref() == Some(binding.role_ref.name.as_str()))
            {
                if rules_allow(role.rules.as_deref(), request) {
                    return true;
                }
            }
        }
        // RoleBindings cannot authorize cluster-scoped resources/nonresource URLs.
        let namespace = match request {
            Request::Resource(r) => r.namespace,
            _ => None,
        };
        let Some(namespace) = namespace.filter(|s| !s.is_empty()) else {
            return false;
        };
        for binding in &self.role_bindings {
            if binding.metadata.namespace.as_deref() != Some(namespace)
                || binding.role_ref.api_group != RBAC_GROUP
                || !subjects_match(
                    binding.subjects.as_deref().unwrap_or_default(),
                    user,
                    Some(namespace),
                )
            {
                continue;
            }
            let rules = match binding.role_ref.kind.as_str() {
                "Role" => self
                    .roles
                    .iter()
                    .find(|r| {
                        r.metadata.namespace.as_deref() == Some(namespace)
                            && r.metadata.name.as_deref() == Some(binding.role_ref.name.as_str())
                    })
                    .and_then(|r| r.rules.as_deref()),
                "ClusterRole" => self
                    .cluster_roles
                    .iter()
                    .find(|r| r.metadata.name.as_deref() == Some(binding.role_ref.name.as_str()))
                    .and_then(|r| r.rules.as_deref()),
                _ => None,
            };
            if rules_allow(rules, request) {
                return true;
            }
        }
        false
    }
}
fn subjects_match(subjects: &[Subject], user: &User, binding_ns: Option<&str>) -> bool {
    subjects.iter().any(|s| match s.kind.as_str() {
        "User" => s.api_group.as_deref() == Some(RBAC_GROUP) && s.name == user.name,
        "Group" => s.api_group.as_deref() == Some(RBAC_GROUP) && user.groups.contains(&s.name),
        "ServiceAccount" => {
            s.api_group.as_deref().unwrap_or("").is_empty()
                && s.namespace
                    .as_deref()
                    .or(binding_ns)
                    .filter(|ns| !ns.is_empty())
                    .is_some_and(|ns| user.name == format!("system:serviceaccount:{ns}:{}", s.name))
        }
        _ => false,
    })
}
fn rules_allow(rules: Option<&[PolicyRule]>, request: &Request<'_>) -> bool {
    rules
        .unwrap_or_default()
        .iter()
        .any(|r| rule_allows(r, request))
}
fn contains(values: Option<&[String]>, value: &str) -> bool {
    values
        .unwrap_or_default()
        .iter()
        .any(|v| v == "*" || v == value)
}
/// Mirrors Kubernetes v1.34 RBAC matching: wildcard group/verb/resource,
/// exact resource names, */subresource, and trailing-star nonresource URLs.
/// No inferred "pods/*" wildcard and no resourceName wildcard.
pub fn rule_allows(rule: &PolicyRule, request: &Request<'_>) -> bool {
    let verb = match request {
        Request::Resource(r) => r.verb,
        Request::NonResource { verb, .. } => verb,
    };
    if !contains(Some(&rule.verbs), verb) {
        return false;
    }
    match request {
        Request::NonResource { path, .. } => rule
            .non_resource_urls
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|url| {
                url == "*"
                    || url == path
                    || url
                        .strip_suffix('*')
                        .is_some_and(|prefix| path.starts_with(prefix))
            }),
        Request::Resource(r) => {
            if !contains(rule.api_groups.as_deref(), r.group) {
                return false;
            }
            let resource = match r.subresource.filter(|s| !s.is_empty()) {
                Some(sub) => format!("{}/{sub}", r.resource),
                None => r.resource.into(),
            };
            let matches = rule
                .resources
                .as_deref()
                .unwrap_or_default()
                .iter()
                .any(|v| {
                    v == "*"
                        || v == &resource
                        || r.subresource
                            .filter(|s| !s.is_empty())
                            .is_some_and(|sub| v == &format!("*/{sub}"))
                });
            matches
                && rule
                    .resource_names
                    .as_deref()
                    .filter(|names| !names.is_empty())
                    .is_none_or(|names| r.name.is_some_and(|name| names.iter().any(|n| n == name)))
        }
    }
}
