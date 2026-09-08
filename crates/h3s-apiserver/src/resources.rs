use k8s_openapi::api::{
    apps::v1::{Deployment, ReplicaSet},
    coordination::v1::Lease,
    core::v1::{ConfigMap, Namespace, Node, Pod, Secret, Service, ServiceAccount},
    discovery::v1::EndpointSlice,
    rbac::v1::{ClusterRole, ClusterRoleBinding, Role, RoleBinding},
};
use serde_json::Value;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Resource {
    pub group: &'static str,
    pub version: &'static str,
    pub plural: &'static str,
    pub kind: &'static str,
    pub namespaced: bool,
}
pub(crate) const RESOURCES: &[Resource] = &[
    Resource {
        group: "",
        version: "v1",
        plural: "namespaces",
        kind: "Namespace",
        namespaced: false,
    },
    Resource {
        group: "",
        version: "v1",
        plural: "configmaps",
        kind: "ConfigMap",
        namespaced: true,
    },
    Resource {
        group: "",
        version: "v1",
        plural: "secrets",
        kind: "Secret",
        namespaced: true,
    },
    Resource {
        group: "",
        version: "v1",
        plural: "pods",
        kind: "Pod",
        namespaced: true,
    },
    Resource {
        group: "",
        version: "v1",
        plural: "nodes",
        kind: "Node",
        namespaced: false,
    },
    Resource {
        group: "",
        version: "v1",
        plural: "services",
        kind: "Service",
        namespaced: true,
    },
    Resource {
        group: "",
        version: "v1",
        plural: "serviceaccounts",
        kind: "ServiceAccount",
        namespaced: true,
    },
    Resource {
        group: "apps",
        version: "v1",
        plural: "deployments",
        kind: "Deployment",
        namespaced: true,
    },
    Resource {
        group: "apps",
        version: "v1",
        plural: "replicasets",
        kind: "ReplicaSet",
        namespaced: true,
    },
    Resource {
        group: "discovery.k8s.io",
        version: "v1",
        plural: "endpointslices",
        kind: "EndpointSlice",
        namespaced: true,
    },
    Resource {
        group: "coordination.k8s.io",
        version: "v1",
        plural: "leases",
        kind: "Lease",
        namespaced: true,
    },
    Resource {
        group: "rbac.authorization.k8s.io",
        version: "v1",
        plural: "roles",
        kind: "Role",
        namespaced: true,
    },
    Resource {
        group: "rbac.authorization.k8s.io",
        version: "v1",
        plural: "rolebindings",
        kind: "RoleBinding",
        namespaced: true,
    },
    Resource {
        group: "rbac.authorization.k8s.io",
        version: "v1",
        plural: "clusterroles",
        kind: "ClusterRole",
        namespaced: false,
    },
    Resource {
        group: "rbac.authorization.k8s.io",
        version: "v1",
        plural: "clusterrolebindings",
        kind: "ClusterRoleBinding",
        namespaced: false,
    },
];
impl Resource {
    pub fn valid_name(&self, name: &str) -> bool {
        if self.group == "rbac.authorization.k8s.io" {
            !name.is_empty()
                && name.len() <= 253
                && name != "."
                && name != ".."
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"-._:".contains(&c))
        } else if matches!(self.kind, "Service" | "Namespace") {
            valid_label_name(name)
        } else {
            valid_name(name)
        }
    }
    pub fn has_status(&self) -> bool {
        matches!(
            self.kind,
            "Pod" | "Node" | "Service" | "Namespace" | "Deployment" | "ReplicaSet"
        )
    }
    pub fn has_generation(&self) -> bool {
        matches!(self.kind, "Pod" | "Deployment" | "ReplicaSet")
    }
    pub fn api_version(&self) -> String {
        if self.group.is_empty() {
            self.version.into()
        } else {
            format!("{}/{}", self.group, self.version)
        }
    }
    pub fn normalize(&self, value: Value) -> Result<Value, serde_json::Error> {
        fn convert<T: serde::de::DeserializeOwned + serde::Serialize>(
            v: Value,
        ) -> Result<Value, serde_json::Error> {
            serde_json::to_value(serde_json::from_value::<T>(v)?)
        }
        match self.kind {
            "Pod" => convert::<Pod>(value),
            "Node" => convert::<Node>(value),
            "Service" => convert::<Service>(value),
            "ServiceAccount" => convert::<ServiceAccount>(value),
            "Deployment" => convert::<Deployment>(value),
            "ReplicaSet" => convert::<ReplicaSet>(value),
            "EndpointSlice" => convert::<EndpointSlice>(value),
            "Lease" => convert::<Lease>(value),
            "Namespace" => convert::<Namespace>(value),
            "ConfigMap" => convert::<ConfigMap>(value),
            "Secret" => convert::<Secret>(value),
            "Role" => convert::<Role>(value),
            "RoleBinding" => convert::<RoleBinding>(value),
            "ClusterRole" => convert::<ClusterRole>(value),
            "ClusterRoleBinding" => convert::<ClusterRoleBinding>(value),
            _ => unreachable!("static resource table"),
        }
    }
}
#[derive(Clone, Debug)]
pub(crate) struct Target {
    pub resource: Resource,
    pub namespace: Option<String>,
    pub name: Option<String>,
    pub subresource: Option<&'static str>,
}
impl Target {
    pub fn parse(path: &str) -> Option<Self> {
        Self::parse_resource(path, None)
            .or_else(|| Self::parse_resource(path.strip_suffix("/status")?, Some("status")))
            .or_else(|| Self::parse_resource(path.strip_suffix("/binding")?, Some("binding")))
    }
    fn parse_resource(path: &str, subresource: Option<&'static str>) -> Option<Self> {
        let parts: Vec<_> = path.strip_prefix('/')?.split('/').collect();
        let (group, version, tail) = match parts.as_slice() {
            ["api", version, tail @ ..] => ("", *version, tail),
            ["apis", group, version, tail @ ..] => (*group, *version, tail),
            _ => return None,
        };
        let (namespace, plural, name) = match tail {
            ["namespaces", ns, plural] => (Some((*ns).to_owned()), *plural, None),
            ["namespaces", ns, plural, name] => {
                (Some((*ns).to_owned()), *plural, Some((*name).to_owned()))
            }
            [plural] => (None, *plural, None),
            [plural, name] => (None, *plural, Some((*name).to_owned())),
            _ => return None,
        };
        let resource = *RESOURCES
            .iter()
            .find(|r| r.group == group && r.version == version && r.plural == plural)?;
        if namespace.is_some() && !resource.namespaced {
            return None;
        }
        if name.as_ref().is_some_and(|n| !resource.valid_name(n))
            || namespace.as_ref().is_some_and(|n| !valid_label_name(n))
        {
            return None;
        }
        if subresource.is_some()
            && (name.is_none()
                || match subresource {
                    Some("status") => !resource.has_status(),
                    Some("binding") => resource.kind != "Pod",
                    _ => true,
                })
        {
            return None;
        }
        Some(Self {
            subresource,
            resource,
            namespace,
            name,
        })
    }
    pub fn prefix(&self) -> String {
        match &self.namespace {
            Some(ns) => format!("/registry/{}/{ns}/", self.resource.plural),
            None => format!("/registry/{}/", self.resource.plural),
        }
    }
}
pub(crate) fn valid_label_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name.as_bytes()[name.len() - 1].is_ascii_alphanumeric()
}
pub(crate) fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 253 && name.split('.').all(valid_label_name)
}
