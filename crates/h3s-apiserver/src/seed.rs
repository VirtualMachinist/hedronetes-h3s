//! Namespaces and RBAC objects the control plane needs before it serves.
//! Every object is created once; an existing object is left untouched.
use crate::Api;
use h3s_api::network::{NODE_CIDR_CONTROLLER_ID, NODE_CIDR_PATH};
use serde_json::{json, Value};

type Result = std::result::Result<(), h3s_storage::Error>;

async fn cluster_role(api: &Api, name: &str, rules: Value, subject: Value) -> Result {
    api.bootstrap(format!("/registry/clusterroles/{name}"),json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRole","metadata":{"name":name},"rules":rules})).await?;
    api.bootstrap(format!("/registry/clusterrolebindings/{name}"),json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"ClusterRoleBinding","metadata":{"name":name},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"ClusterRole","name":name},"subjects":[subject]})).await
}
fn user(name: &str) -> Value {
    json!({"kind":"User","apiGroup":"rbac.authorization.k8s.io","name":name})
}
fn group(name: &str) -> Value {
    json!({"kind":"Group","apiGroup":"rbac.authorization.k8s.io","name":name})
}

/// System namespaces plus the discovery, controller and scheduler grants.
pub(crate) async fn cluster(api: &Api) -> Result {
    for namespace in ["default", "kube-system", "kube-public", "kube-node-lease"] {
        api.bootstrap(format!("/registry/namespaces/{namespace}"),json!({"apiVersion":"v1","kind":"Namespace","metadata":{"name":namespace},"status":{"phase":"Active"}})).await?;
    }
    cluster_role(
        api,
        "h3s-discovery",
        json!([{"verbs":["get"],"nonResourceURLs":["/api","/api/*","/apis","/apis/*","/openapi","/openapi/*"]}]),
        group("system:authenticated"),
    )
    .await?;
    for (name, identity, rules) in [
        (
            "h3s-namespace-controller",
            "system:h3s:namespace-controller",
            json!([
                {"apiGroups":[""],"resources":["namespaces"],"verbs":["get","list","watch","update"]},
                {"apiGroups":[""],"resources":["serviceaccounts"],"verbs":["get","watch"],"resourceNames":["default"]},
                {"apiGroups":[""],"resources":["configmaps"],"verbs":["get","watch","update"],"resourceNames":["kube-root-ca.crt"]},
                {"apiGroups":[""],"resources":["serviceaccounts","configmaps"],"verbs":["create"]},
                {"apiGroups":[""],"resources":["pods","services","configmaps","secrets","serviceaccounts"],"verbs":["list","delete"]},
                {"apiGroups":["apps"],"resources":["deployments","replicasets"],"verbs":["list","delete"]},
                {"apiGroups":["discovery.k8s.io"],"resources":["endpointslices"],"verbs":["list","delete"]},
                {"apiGroups":["rbac.authorization.k8s.io"],"resources":["roles","rolebindings"],"verbs":["list","delete"]},
                {"apiGroups":["coordination.k8s.io"],"resources":["leases"],"verbs":["list","delete"]}
            ]),
        ),
        (
            "h3s-scheduler",
            "system:h3s:scheduler",
            json!([
                {"apiGroups":[""],"resources":["nodes","pods"],"verbs":["get","list"]},
                {"apiGroups":[""],"resources":["pods/binding"],"verbs":["create"]},
                {"apiGroups":[""],"resources":["pods/status"],"verbs":["update"]},
                {"apiGroups":["coordination.k8s.io"],"resources":["leases"],"verbs":["list"]}
            ]),
        ),
        (
            "h3s-endpointslice-controller",
            "system:h3s:endpointslice-controller",
            json!([
                {"apiGroups":[""],"resources":["services"],"verbs":["get","list","watch"]},
                {"apiGroups":[""],"resources":["pods"],"verbs":["list"]},
                {"apiGroups":["discovery.k8s.io"],"resources":["endpointslices"],"verbs":["get","list","watch","create","update","delete"]}
            ]),
        ),
        (
            "h3s-deployment-controller",
            "system:h3s:deployment-controller",
            json!([
                {"apiGroups":["apps"],"resources":["deployments"],"verbs":["get","list","watch"]},
                {"apiGroups":["apps"],"resources":["deployments/status"],"verbs":["update"]},
                {"apiGroups":["apps"],"resources":["replicasets"],"verbs":["get","list","watch","create","update","delete"]},
                {"apiGroups":[""],"resources":["pods"],"verbs":["list"]}
            ]),
        ),
        (
            "h3s-replicaset-controller",
            "system:h3s:replicaset-controller",
            json!([
                {"apiGroups":["apps"],"resources":["replicasets"],"verbs":["get","list","watch"]},
                {"apiGroups":["apps"],"resources":["replicasets/status"],"verbs":["update"]},
                {"apiGroups":[""],"resources":["pods"],"verbs":["get","list","watch","create","update","delete"]}
            ]),
        ),
        (
            "h3s-workload-gc",
            "system:h3s:workload-gc",
            json!([
                {"apiGroups":["apps"],"resources":["deployments","replicasets"],"verbs":["get","list"]},
                {"apiGroups":["apps"],"resources":["replicasets"],"verbs":["delete"]},
                {"apiGroups":[""],"resources":["pods"],"verbs":["list","delete"]}
            ]),
        ),
    ] {
        cluster_role(api, name, rules, user(identity)).await?;
    }
    Ok(())
}

/// Grants that only exist once node CIDR allocation is configured.
pub(crate) async fn node_cidrs(api: &Api) -> Result {
    for (name, rules, subject) in [
        (
            "h3s-node-cidr-controller",
            json!([
                {"apiGroups":[""],"resources":["nodes"],"verbs":["get","list","patch"]},
                {"nonResourceURLs":[NODE_CIDR_PATH],"verbs":["get"]}
            ]),
            user(NODE_CIDR_CONTROLLER_ID),
        ),
        (
            "h3s-network-topology",
            json!([
                {"apiGroups":[""],"resources":["nodes"],"verbs":["get","list","watch"]}
            ]),
            group("system:nodes"),
        ),
        (
            "h3s-service-discovery",
            json!([
                {"apiGroups":[""],"resources":["services"],"verbs":["get","list","watch"]},
                {"apiGroups":["discovery.k8s.io"],"resources":["endpointslices"],"verbs":["get","list","watch"]}
            ]),
            group("system:nodes"),
        ),
    ] {
        cluster_role(api, name, rules, subject).await?;
    }
    Ok(())
}
