//! Single-server scheduler. All reads, status writes and bindings use the scoped API.
mod placement;
use chrono::Utc;
use k8s_openapi::api::{
    coordination::v1::Lease,
    core::v1::{Binding, Node, Pod},
};
use kube::{
    api::{ListParams, PostParams},
    Api, Client, ResourceExt,
};
use serde_json::{json, Value};
use std::time::Duration;
pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
pub const SCHEDULER_ID: &str = "system:h3s:scheduler";
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("scheduler API request: {0}")]
    Api(#[from] kube::Error),
    #[error("scheduler serialization: {0}")]
    Json(#[from] serde_json::Error),
    #[error("scheduler requires complete bounded API snapshots")]
    Snapshot,
}

/// One sequential scheduling loop per standalone server. Cancellation drops requests.
pub async fn run(client: Client) -> Result<(), Error> {
    loop {
        match tokio::time::timeout(Duration::from_secs(60), reconcile_once(client.clone())).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => eprintln!("scheduler: {error}"),
            Err(_) => eprintln!("scheduler: API cycle deadline exceeded"),
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Full LISTs avoid stale cache assumptions after restart or watch compaction.
/// A successful bind updates this cycle's reservations before the next candidate.
pub async fn reconcile_once(client: Client) -> Result<usize, Error> {
    let lp = ListParams::default().limit(4096);
    let nodes = Api::<Node>::all(client.clone()).list(&lp).await?;
    let leases = Api::<Lease>::namespaced(client.clone(), "kube-node-lease")
        .list(&lp)
        .await?;
    let pods = Api::<Pod>::all(client.clone()).list(&lp).await?;
    for meta in [&nodes.metadata, &leases.metadata, &pods.metadata] {
        if meta.continue_.as_deref().is_some_and(|s| !s.is_empty()) {
            return Err(Error::Snapshot);
        }
    }
    if nodes.items.len() > 4096 || leases.items.len() > 4096 || pods.items.len() > 4096 {
        return Err(Error::Snapshot);
    }
    let nodes: Vec<Value> = nodes
        .items
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()?;
    let leases: Vec<Value> = leases
        .items
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()?;
    let mut objects: Vec<Value> = pods
        .items
        .iter()
        .map(serde_json::to_value)
        .collect::<Result<_, _>>()?;
    let mut candidates: Vec<_> = (0..objects.len())
        .filter(|i| placement::pending(&objects[*i]))
        .collect();
    candidates.sort_by_key(|i| {
        (
            std::cmp::Reverse(objects[*i]["spec"]["priority"].as_i64().unwrap_or(0)),
            objects[*i]["metadata"]["creationTimestamp"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
            objects[*i]["metadata"]["uid"]
                .as_str()
                .unwrap_or("")
                .to_owned(),
        )
    });
    let mut bound = 0;
    for index in candidates {
        let pod = objects[index].clone();
        let namespace = pod["metadata"]["namespace"]
            .as_str()
            .ok_or(Error::Snapshot)?;
        let name = pod["metadata"]["name"].as_str().ok_or(Error::Snapshot)?;
        let api = Api::<Pod>::namespaced(client.clone(), namespace);
        match placement::select(
            &pod,
            &nodes,
            &objects,
            &leases,
            chrono::DateTime::<Utc>::from(std::time::SystemTime::now()),
        ) {
            Ok(node) => {
                let binding: Binding = serde_json::from_value(
                    json!({"metadata":{"name":name,"namespace":namespace,"uid":pod["metadata"]["uid"],"resourceVersion":pod["metadata"]["resourceVersion"]},"target":{"apiVersion":"v1","kind":"Node","name":node}}),
                )?;
                match api
                    .create_subresource::<Value>(
                        "binding",
                        name,
                        &PostParams::default(),
                        serde_json::to_vec(&binding)?,
                    )
                    .await
                {
                    Ok(_) => {
                        objects[index]["spec"]["nodeName"] = node.into();
                        bound += 1;
                        // A fresh CAS status preserves kubelet updates and never targets a replacement UID.
                        if let Some(current) = api.get_opt(name).await? {
                            if current.metadata.uid.as_deref() == pod["metadata"]["uid"].as_str() {
                                condition(&api, serde_json::to_value(current)?, true, "Scheduled")
                                    .await?;
                            }
                        }
                    }
                    Err(kube::Error::Api(response)) if matches!(response.code, 404 | 409) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(reason) => condition(&api, pod, false, reason).await?,
        }
    }
    Ok(bound)
}
async fn condition(
    api: &Api<Pod>,
    mut pod: Value,
    scheduled: bool,
    reason: &str,
) -> Result<(), Error> {
    let status = if scheduled { "True" } else { "False" };
    let mut conditions = pod["status"]["conditions"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if conditions
        .iter()
        .any(|c| c["type"] == "PodScheduled" && c["status"] == status && c["reason"] == reason)
    {
        return Ok(());
    }
    let now = chrono::DateTime::<Utc>::from(std::time::SystemTime::now())
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let transition = conditions
        .iter()
        .find(|c| c["type"] == "PodScheduled" && c["status"] == status)
        .and_then(|c| c["lastTransitionTime"].as_str())
        .unwrap_or(&now)
        .to_owned();
    conditions.retain(|c| c["type"] != "PodScheduled");
    conditions.push(json!({"type":"PodScheduled","status":status,"reason":reason,"message":if scheduled{"bound by native h3s scheduler"}else{"no placement satisfying the supported scheduling constraints is available"},"lastTransitionTime":transition}));
    pod["status"]["conditions"] = conditions.into();
    let object: Pod = serde_json::from_value(pod)?;
    match api
        .replace_status(
            &object.name_any(),
            &PostParams::default(),
            serde_json::to_vec(&object)?,
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(response)) if matches!(response.code, 404 | 409) => Ok(()),
        Err(error) => Err(error.into()),
    }
}
