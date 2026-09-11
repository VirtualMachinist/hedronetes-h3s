//! Node authorizer policy for the implemented M1 resources. The API supplies
//! current Pod relationships; an unverified username is never an identity.
use crate::{ResourceRequest, User};

/// `related` means the API verified an assigned Pod or a namespaced reference
/// from one. `selected_node` is a parsed exact spec.nodeName field requirement.
/// A false decision grants nothing; explicit RBAC grants may still authorize.
pub fn node_allows(
    user: &User,
    request: &ResourceRequest<'_>,
    selected_node: Option<&str>,
    related: bool,
) -> bool {
    let Some(node) = user.node_name() else {
        return false;
    };
    let r = request;
    let read = matches!(r.verb, "get" | "list" | "watch");
    match (r.group, r.resource, r.subresource) {
        ("", "nodes", None) => {
            (read && r.name == Some(node)) || matches!(r.verb, "create" | "update" | "patch")
        }
        ("", "nodes" | "pods", Some("status")) => matches!(r.verb, "update" | "patch"),
        ("", "pods", None) => match r.verb {
            "get" => related,
            "list" | "watch" => selected_node == Some(node) || related,
            // Admission checks assignment. Mirror creation remains explicitly
            // unsupported by the M1 API, even if an RBAC role grants create.
            "delete" => related,
            _ => false,
        },
        ("", "configmaps" | "secrets", None) => read && related,
        ("", "services", None) | ("discovery.k8s.io", "endpointslices", None) => read,
        ("coordination.k8s.io", "leases", None) => {
            r.namespace == Some("kube-node-lease")
                && (r.verb == "create"
                    || (r.name == Some(node)
                        && matches!(r.verb, "get" | "update" | "patch" | "delete")))
        }
        _ => false,
    }
}

/// Kubernetes v1.34 kubelet label namespaces and well-known exceptions.
/// Administrative isolation labels cannot be set, changed, or removed by nodes.
pub fn node_label_allowed(key: &str) -> bool {
    let Some((domain, _)) = key.split_once('/') else {
        return true;
    };
    if domain == "node-restriction.kubernetes.io"
        || domain.ends_with(".node-restriction.kubernetes.io")
    {
        return false;
    }
    if ["kubelet.kubernetes.io", "node.kubernetes.io"]
        .iter()
        .any(|d| {
            domain == *d
                || domain
                    .strip_suffix(d)
                    .is_some_and(|prefix| prefix.ends_with('.'))
        })
    {
        return true;
    }
    if matches!(
        key,
        "kubernetes.io/hostname"
            | "kubernetes.io/arch"
            | "kubernetes.io/os"
            | "beta.kubernetes.io/arch"
            | "beta.kubernetes.io/os"
            | "beta.kubernetes.io/instance-type"
            | "node.kubernetes.io/instance-type"
            | "failure-domain.beta.kubernetes.io/zone"
            | "failure-domain.beta.kubernetes.io/region"
            | "topology.kubernetes.io/zone"
            | "topology.kubernetes.io/region"
    ) {
        return true;
    }
    !["kubernetes.io", "k8s.io"].iter().any(|d| {
        domain == *d
            || domain
                .strip_suffix(d)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}
