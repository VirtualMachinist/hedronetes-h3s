//! API-owned durable reservations. Every caller holds admission_writes.
//! Reserve before publishing Node spec; failed writes may leave safe reservations.
use super::{key, object, stored, Failure, Result};
use h3s_api::network::NodeCidrAllocations;
use h3s_storage::{ListSelect, Storage, StoredObject};
use serde_json::Value;
use std::sync::Arc;

const KEY: &str = "/registry/h3s-node-cidrs/allocations";
fn invalid(message: &str) -> Failure {
    Failure::new(422, "Invalid", message)
}
fn corrupt(message: &str) -> Failure {
    Failure::new(500, "InternalError", message)
}

fn node_cidr(node: &Value) -> Result<Option<&str>> {
    let cidr = node["spec"]["podCIDR"].as_str().filter(|v| !v.is_empty());
    if let Some(list) = node["spec"].get("podCIDRs").filter(|v| !v.is_null()) {
        let list = list
            .as_array()
            .ok_or_else(|| invalid("podCIDRs must be an array"))?;
        if match cidr {
            Some(cidr) => list.len() != 1 || list[0].as_str() != Some(cidr),
            None => !list.is_empty(),
        } {
            return Err(invalid("podCIDRs must contain exactly podCIDR; only IPv4 single-stack allocation is supported"));
        }
    }
    Ok(cidr)
}
async fn load(
    store: &Arc<dyn Storage>,
    configured: &NodeCidrAllocations,
) -> Result<(StoredObject, NodeCidrAllocations)> {
    let saved = store
        .get(&key(KEY.into())?)
        .await?
        .ok_or_else(|| corrupt("node CIDR ledger is missing"))?;
    let state: NodeCidrAllocations =
        serde_json::from_slice(&saved.value).map_err(|_| corrupt("invalid node CIDR ledger"))?;
    state.validate().map_err(corrupt)?;
    if state.cluster_cidr != configured.cluster_cidr || state.node_prefix != configured.node_prefix
    {
        return Err(corrupt(
            "configured node CIDR pool differs from durable ledger",
        ));
    }
    Ok((saved, state))
}
pub(crate) async fn snapshot(
    store: &Arc<dyn Storage>,
    configured: &NodeCidrAllocations,
) -> Result<NodeCidrAllocations> {
    Ok(load(store, configured).await?.1)
}
pub(crate) async fn initialize(
    store: &Arc<dyn Storage>,
    configured: &NodeCidrAllocations,
) -> Result<()> {
    configured.validate().map_err(invalid)?;
    let existing = store.get(&key(KEY.into())?).await?;
    let mut state = if existing.is_some() {
        load(store, configured).await?.1
    } else {
        configured.clone()
    };
    // Import the complete fixed registry snapshot before any reservation commit.
    let mut selection = ListSelect::new("/registry/nodes/");
    loop {
        let page = store.list(selection.clone()).await?;
        selection.at_revision = Some(page.revision);
        for saved in page.items {
            let node = object(saved)?;
            if let Some(cidr) = node_cidr(&node)? {
                let name = node["metadata"]["name"]
                    .as_str()
                    .ok_or_else(|| corrupt("stored Node name missing"))?;
                state.reserve(name, cidr).map_err(invalid)?;
            }
        }
        let Some(cursor) = page.next_after else {
            break;
        };
        selection.start_after = Some(cursor);
    }
    let value = serde_json::to_value(&state)?;
    let obj = stored(key(KEY.into())?, &value)?;
    match existing {
        Some(old) if old.value != obj.value => {
            store.update(obj, old.revision).await?;
        }
        None => {
            store.create(obj).await?;
        }
        _ => {}
    }
    Ok(())
}
pub(crate) async fn admit(
    store: &Arc<dyn Storage>,
    configured: &NodeCidrAllocations,
    value: &Value,
    old: Option<&Value>,
) -> Result<()> {
    let cidr = node_cidr(value)?;
    if let Some(old) = old {
        if let Some(previous) = node_cidr(old)? {
            if cidr != Some(previous) {
                return Err(invalid("allocated node CIDR is immutable"));
            }
        }
    }
    let Some(cidr) = cidr else {
        return Ok(());
    };
    let (saved, mut state) = load(store, configured).await?;
    let name = value["metadata"]["name"]
        .as_str()
        .expect("validated Node name");
    if state.reserve(name, cidr).map_err(invalid)? {
        store
            .update(
                stored(saved.key, &serde_json::to_value(state)?)?,
                saved.revision,
            )
            .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use h3s_storage::SqliteStore;
    use serde_json::json;

    #[tokio::test]
    async fn reserved_before_node_write_survives_reopen_and_rejects_pool_drift() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.db");
        let store: Arc<dyn Storage> = Arc::new(SqliteStore::open(&path).await.unwrap());
        let config = NodeCidrAllocations::new("10.42.0.0/16", 24).unwrap();
        initialize(&store, &config).await.unwrap();
        // Simulate interruption after reservation commit but before Node commit.
        let value = json!({"metadata":{"name":"a"},"spec":{"podCIDR":"10.42.2.0/24","podCIDRs":["10.42.2.0/24"]}});
        admit(&store, &config, &value, None).await.unwrap();
        assert!(store
            .get(&key("/registry/nodes/a".into()).unwrap())
            .await
            .unwrap()
            .is_none());
        drop(store);
        let store: Arc<dyn Storage> = Arc::new(SqliteStore::open(&path).await.unwrap());
        initialize(&store, &config).await.unwrap();
        let state = snapshot(&store, &config).await.unwrap();
        assert_eq!(state.next("a").unwrap(), "10.42.2.0/24");
        let mut other = value.clone();
        other["metadata"]["name"] = "b".into();
        assert_eq!(
            admit(&store, &config, &other, None).await.unwrap_err().code,
            422
        );
        let changed = NodeCidrAllocations::new("10.44.0.0/16", 24).unwrap();
        assert!(initialize(&store, &changed).await.is_err());
        assert_eq!(snapshot(&store, &config).await.unwrap(), state);
        let saved = store.get(&key(KEY.into()).unwrap()).await.unwrap().unwrap();
        let mut malformed = serde_json::to_value(&state).unwrap();
        malformed["reservations"]["b"] = "10.42.2.0/24".into();
        store
            .update(stored(saved.key, &malformed).unwrap(), saved.revision)
            .await
            .unwrap();
        assert!(initialize(&store, &config).await.is_err());
        assert!(snapshot(&store, &config).await.is_err());
    }

    #[tokio::test]
    async fn imports_all_pages_and_rejects_ambiguous_registry_without_partial_commit() {
        let dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn Storage> = Arc::new(
            SqliteStore::open(dir.path().join("registry.db"))
                .await
                .unwrap(),
        );
        let config = NodeCidrAllocations::new("172.16.0.0/12", 24).unwrap();
        for index in 0..257 {
            let name = format!("node-{index:04}");
            let cidr = format!("172.{}.{}.0/24", 16 + index / 256, index % 256);
            let value = json!({"metadata":{"name":name},"spec":{"podCIDR":cidr}});
            store
                .create(stored(key(format!("/registry/nodes/{name}")).unwrap(), &value).unwrap())
                .await
                .unwrap();
        }
        let duplicate =
            json!({"metadata":{"name":"zz-collision"},"spec":{"podCIDR":"172.17.0.0/24"}});
        let saved = store
            .create(
                stored(
                    key("/registry/nodes/zz-collision".into()).unwrap(),
                    &duplicate,
                )
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(initialize(&store, &config).await.is_err());
        assert!(store
            .get(&key(KEY.into()).unwrap())
            .await
            .unwrap()
            .is_none());
        store.delete(&saved.key, saved.revision).await.unwrap();
        initialize(&store, &config).await.unwrap();
        let state = snapshot(&store, &config).await.unwrap();
        assert_eq!(state.reservations.len(), 257);
        assert_eq!(state.next("node-0256").unwrap(), "172.17.0.0/24");
        assert_eq!(state.next("fresh").unwrap(), "172.17.1.0/24");
    }
}
