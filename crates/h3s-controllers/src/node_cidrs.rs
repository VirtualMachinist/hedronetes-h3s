//! Native allocator: all reads and mutations use its scoped authenticated client.
use crate::Error;
use h3s_api::network::{NodeCidrAllocations, NODE_CIDR_PATH};
use k8s_openapi::api::core::v1::Node;
use kube::{
    api::{ListParams, Patch, PatchParams},
    Api, Client,
};
use serde_json::json;
use std::time::Duration;

pub async fn node_cidrs_once(client: Client) -> Result<(), Error> {
    let request = http::Request::get(NODE_CIDR_PATH)
        .body(Vec::new())
        .expect("constant request");
    let mut allocation: NodeCidrAllocations = client.request(request).await?;
    allocation.validate().map_err(Error::Invalid)?;
    let nodes = Api::<Node>::all(client);
    let current = nodes.list(&ListParams::default().limit(4096)).await?;
    // Reject an incomplete topology before choosing or writing any subnet.
    if current
        .metadata
        .continue_
        .as_ref()
        .is_some_and(|v| !v.is_empty())
        || current.items.len() > 4096
    {
        return Err(Error::Invalid(
            "node CIDR controller requires a complete bounded Node snapshot",
        ));
    }
    for node in current.items {
        if node.metadata.deletion_timestamp.is_some() {
            continue;
        }
        let name = node
            .metadata
            .name
            .as_deref()
            .ok_or(Error::Invalid("Node name missing"))?;
        let uid = node
            .metadata
            .uid
            .as_deref()
            .ok_or(Error::Invalid("Node UID missing"))?;
        let rv = node
            .metadata
            .resource_version
            .as_deref()
            .ok_or(Error::Invalid("Node resourceVersion missing"))?;
        let spec = node.spec.as_ref();
        let existing = spec
            .and_then(|s| s.pod_cidr.as_deref())
            .filter(|s| !s.is_empty());
        let cidr = match existing {
            Some(cidr) => {
                allocation.reserve(name, cidr).map_err(Error::Invalid)?;
                cidr.to_owned()
            }
            None => allocation.next(name).map_err(Error::Invalid)?,
        };
        if existing == Some(cidr.as_str())
            && spec.and_then(|s| s.pod_cidrs.as_ref()) == Some(&vec![cidr.clone()])
        {
            continue;
        }
        nodes
            .patch(
                name,
                &PatchParams::default(),
                &Patch::Merge(json!({
                    "metadata":{"uid":uid,"resourceVersion":rv},
                    "spec":{"podCIDR":cidr,"podCIDRs":[cidr]}
                })),
            )
            .await?;
        allocation.reserve(name, &cidr).map_err(Error::Invalid)?;
    }
    Ok(())
}
pub async fn run_node_cidr_controller(client: Client) -> Result<(), Error> {
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        match tokio::time::timeout(Duration::from_secs(60), node_cidrs_once(client.clone())).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("node CIDR controller: {error}"),
            Err(_) => eprintln!("node CIDR controller: reconciliation timed out"),
        }
    }
}
