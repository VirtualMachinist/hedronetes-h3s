//! In-tree kube-rs reconciliation. This crate has no registry storage dependency.
mod node_cidrs;
mod workload;
use futures_util::StreamExt;
pub use h3s_api::network::NODE_CIDR_CONTROLLER_ID;
use k8s_openapi::api::{
    apps::v1::{Deployment, ReplicaSet},
    coordination::v1::Lease,
    core::v1::{ConfigMap, Namespace, Pod, Secret, Service, ServiceAccount},
    discovery::v1::EndpointSlice,
    rbac::v1::{Role, RoleBinding},
};
use kube::core::NamespaceResourceScope;
use kube::{
    api::{DeleteParams, ListParams, ObjectMeta, PostParams},
    config::{KubeConfigOptions, Kubeconfig},
    runtime::{controller::Action, reflector::ObjectRef, watcher, Controller},
    Api, Client, Config, Resource, ResourceExt,
};
pub use node_cidrs::{node_cidrs_once, run_node_cidr_controller};
use serde::de::DeserializeOwned;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
pub use workload::{
    deployment_once, endpoint_gc_once, endpoints_once, gc_once, replicaset_once,
    run_deployment_controller, run_endpoint_controller, run_replicaset_controller, run_workload_gc,
    DEPLOYMENT_CONTROLLER_ID, ENDPOINT_CONTROLLER_ID, REPLICASET_CONTROLLER_ID, WORKLOAD_GC_ID,
};

pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
pub const NAMESPACE_CONTROLLER_ID: &str = "system:h3s:namespace-controller";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("controller API request: {0}")]
    Api(#[from] kube::Error),
    #[error("controller serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Invalid(&'static str),
    #[error("controller stream ended unexpectedly")]
    Stopped,
}

/// Uses only the supplied in-memory kubeconfig; never discovers host credentials.
pub async fn client_from_kubeconfig(
    contents: &str,
) -> Result<Client, Box<dyn std::error::Error + Send + Sync>> {
    let mut config = Config::from_custom_kubeconfig(
        Kubeconfig::from_yaml(contents)?,
        &KubeConfigOptions::default(),
    )
    .await?;
    config.connect_timeout = Some(Duration::from_secs(10));
    config.read_timeout = Some(Duration::from_secs(330));
    Ok(Client::try_from(config)?)
}
struct Context {
    client: Client,
    ca_pem: String,
}

/// Watches namespace lifecycle and child changes, restoring only the default
/// account and public root-CA ConfigMap. Existing account configuration is owned
/// by operators and is never overwritten. Cancellation drops the controller.
pub async fn run_namespace_controller(client: Client, ca_pem: String) -> Result<(), Error> {
    let ctx = Arc::new(Context {
        client: client.clone(),
        ca_pem,
    });
    Controller::new(
        Api::<Namespace>::all(client.clone()),
        watcher::Config::default(),
    )
    .watches(
        Api::<ServiceAccount>::all(client.clone()),
        watcher::Config::default().fields("metadata.name=default"),
        |sa| sa.namespace().map(|ns| ObjectRef::<Namespace>::new(&ns)),
    )
    .watches(
        Api::<ConfigMap>::all(client),
        watcher::Config::default().fields("metadata.name=kube-root-ca.crt"),
        |cm| cm.namespace().map(|ns| ObjectRef::<Namespace>::new(&ns)),
    )
    .run(
        reconcile_namespace,
        |_, _, _| Action::requeue(Duration::from_secs(5)),
        ctx,
    )
    .for_each(|result| async move {
        if let Err(error) = result {
            eprintln!("namespace controller: {error}");
        }
    })
    .await;
    Err(Error::Stopped)
}

async fn reconcile_namespace(
    namespace: Arc<Namespace>,
    ctx: Arc<Context>,
) -> Result<Action, Error> {
    let name = namespace.name_any();
    // Re-read through the API before creating content in a possibly terminating
    // namespace. Admission is the final guard against a concurrent transition.
    let Some(current) = Api::<Namespace>::all(ctx.client.clone())
        .get_opt(&name)
        .await?
    else {
        return Ok(Action::await_change());
    };
    if current.metadata.deletion_timestamp.is_some()
        || current.status.as_ref().and_then(|s| s.phase.as_deref()) == Some("Terminating")
    {
        return collect_terminating_namespace(&name, ctx).await;
    }
    let accounts = Api::<ServiceAccount>::namespaced(ctx.client.clone(), &name);
    if accounts.get_opt("default").await?.is_none() {
        let account = ServiceAccount {
            metadata: ObjectMeta {
                name: Some("default".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        if let Err(error) = accounts.create(&PostParams::default(), &account).await {
            if !matches!(&error,kube::Error::Api(response) if response.code==409) {
                return Err(error.into());
            }
        }
    }
    let maps = Api::<ConfigMap>::namespaced(ctx.client.clone(), &name);
    match maps.get_opt("kube-root-ca.crt").await? {
        None => {
            let cm = ConfigMap {
                metadata: ObjectMeta {
                    name: Some("kube-root-ca.crt".into()),
                    ..Default::default()
                },
                data: Some(BTreeMap::from([("ca.crt".into(), ctx.ca_pem.clone())])),
                ..Default::default()
            };
            if let Err(error) = maps.create(&PostParams::default(), &cm).await {
                if !matches!(&error,kube::Error::Api(response) if response.code==409) {
                    return Err(error.into());
                }
            }
        }
        Some(mut cm) => {
            if cm.data.as_ref().and_then(|data| data.get("ca.crt")) != Some(&ctx.ca_pem) {
                cm.data
                    .get_or_insert_with(BTreeMap::new)
                    .insert("ca.crt".into(), ctx.ca_pem.clone());
                maps.replace("kube-root-ca.crt", &PostParams::default(), &cm)
                    .await?;
            }
        }
    }
    Ok(Action::requeue(Duration::from_secs(300)))
}

async fn collect_terminating_namespace(name: &str, ctx: Arc<Context>) -> Result<Action, Error> {
    let remaining = gc_namespaced::<Deployment>(&ctx.client, name).await?
        + gc_namespaced::<ReplicaSet>(&ctx.client, name).await?
        + gc_namespaced::<Pod>(&ctx.client, name).await?
        + gc_namespaced::<Service>(&ctx.client, name).await?
        + gc_namespaced::<EndpointSlice>(&ctx.client, name).await?
        + gc_namespaced::<ConfigMap>(&ctx.client, name).await?
        + gc_namespaced::<Secret>(&ctx.client, name).await?
        + gc_namespaced::<ServiceAccount>(&ctx.client, name).await?
        + gc_namespaced::<Role>(&ctx.client, name).await?
        + gc_namespaced::<RoleBinding>(&ctx.client, name).await?
        + gc_namespaced::<Lease>(&ctx.client, name).await?;
    if remaining > 0 {
        return Ok(Action::requeue(Duration::from_secs(2)));
    }
    let namespaces = Api::<Namespace>::all(ctx.client.clone());
    let Some(mut current) = namespaces.get_opt(name).await? else {
        return Ok(Action::await_change());
    };
    if current
        .metadata
        .finalizers
        .as_ref()
        .is_none_or(|f| f.is_empty())
    {
        return Ok(Action::await_change());
    }
    current.metadata.finalizers = Some(Vec::new());
    namespaces
        .replace(name, &PostParams::default(), &current)
        .await?;
    Ok(Action::await_change())
}

async fn gc_namespaced<K>(client: &Client, ns: &str) -> Result<usize, Error>
where
    K: Clone + std::fmt::Debug + DeserializeOwned + Resource<Scope = NamespaceResourceScope>,
    <K as Resource>::DynamicType: Default,
{
    let api = Api::<K>::namespaced(client.clone(), ns);
    let list = api.list(&ListParams::default()).await?;
    let mut deleted = 0;
    for item in list.items {
        match api.delete(&item.name_any(), &DeleteParams::default()).await {
            Ok(_) => deleted += 1,
            Err(kube::Error::Api(error)) if error.code == 404 => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(deleted)
}
