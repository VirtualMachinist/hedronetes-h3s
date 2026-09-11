//! Authorization: the RBAC snapshot, then node-scoped grants for resource
//! requests that RBAC alone does not allow.
use crate::{nodes, resources::Target, selectors::Selection, Api, Failure, Result};
use h3s_auth::{Rbac, Request as AuthRequest, ResourceRequest, User};
use h3s_storage::ListSelect;

impl Api {
    pub(crate) async fn rbac(&self) -> Result<Rbac> {
        let mut r = Rbac::default();
        let mut snapshot = None;
        for kind in [
            "roles",
            "rolebindings",
            "clusterroles",
            "clusterrolebindings",
        ] {
            let mut sel = ListSelect::new(format!("/registry/{kind}/"));
            sel.at_revision = snapshot;
            loop {
                let page = self.store.list(sel.clone()).await?;
                snapshot.get_or_insert(page.revision);
                for obj in page.items {
                    match kind {
                        "roles" => r.roles.push(serde_json::from_slice(&obj.value)?),
                        "rolebindings" => r.role_bindings.push(serde_json::from_slice(&obj.value)?),
                        "clusterroles" => r.cluster_roles.push(serde_json::from_slice(&obj.value)?),
                        _ => r
                            .cluster_role_bindings
                            .push(serde_json::from_slice(&obj.value)?),
                    }
                }
                let Some(next) = page.next_after else {
                    break;
                };
                sel.at_revision = Some(page.revision);
                sel.start_after = Some(next);
            }
        }
        Ok(r)
    }
}

/// Non-resource URLs are readable by whoever RBAC grants `get` on the path.
pub(crate) async fn non_resource(
    api: &Api,
    user: &User,
    path: &str,
    denial: &'static str,
) -> Result<()> {
    if !api
        .rbac()
        .await?
        .allows(user, &AuthRequest::NonResource { verb: "get", path })
    {
        return Err(Failure::new(403, "Forbidden", denial));
    }
    Ok(())
}

/// What an authorized resource request may see: its selection, narrowed to
/// the node's own Pods for node grants, and a relationship guard that keeps
/// node reads of Secrets and ConfigMaps honest for the life of a stream.
pub(crate) struct Grant {
    pub selection: Selection,
    pub read_guard: Option<nodes::ReadGuard>,
}

pub(crate) async fn resource(
    api: &Api,
    user: &User,
    target: &Target,
    verb: &'static str,
    mut selection: Selection,
) -> Result<Grant> {
    let selected_name = selection.exact_name().map(str::to_owned);
    let attrs = ResourceRequest {
        verb,
        group: target.resource.group,
        resource: target.resource.plural,
        subresource: target.subresource,
        namespace: target.namespace.as_deref(),
        name: target.name.as_deref().or_else(|| {
            matches!(verb, "list" | "watch")
                .then(|| selected_name.as_deref())
                .flatten()
        }),
    };
    let rbac_allowed = api
        .rbac()
        .await?
        .allows(user, &AuthRequest::Resource(attrs.clone()));
    let node_allowed = !rbac_allowed
        && match user.node_name() {
            Some(node) => {
                let related = nodes::related(api, node, target, attrs.name).await?;
                h3s_auth::node_allows(
                    user,
                    &attrs,
                    selection.exact_field("spec.nodeName"),
                    related,
                )
            }
            None => false,
        };
    if !rbac_allowed && !node_allowed {
        return Err(Failure::new(
            403,
            "Forbidden",
            format!(
                "user {} cannot {verb} {}",
                user.name, target.resource.plural
            ),
        ));
    }
    let mut read_guard = None;
    if node_allowed {
        let node = user.node_name().expect("node grant requires node identity");
        if target.resource.kind == "Pod" && matches!(verb, "list" | "watch") {
            // Also constrain name-only watches: a recreated Pod assigned to
            // another worker must not enter this node's stream.
            selection = selection.with_field("spec.nodeName", node);
        }
        if matches!(target.resource.kind, "Secret" | "ConfigMap") {
            read_guard = Some(nodes::ReadGuard::new(
                node,
                target,
                attrs.name.expect("relationship name"),
            ));
        }
    }
    if target.resource.group == "rbac.authorization.k8s.io"
        && ["create", "update", "patch", "delete"].contains(&verb)
        && !user.is_superuser()
    {
        return Err(Failure::new(403,"Forbidden","RBAC mutations require the bootstrap administrator until escalation checks are implemented"));
    }
    Ok(Grant {
        selection,
        read_guard,
    })
}
