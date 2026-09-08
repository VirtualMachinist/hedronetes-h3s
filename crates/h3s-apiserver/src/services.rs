//! Single-server ClusterIP allocation. The Service write is the allocation commit.
//! Callers hold Api::service_writes through allocation and the registry mutation.
use super::{object, Failure, Result};
use h3s_storage::{ListSelect, Storage};
use serde_json::{json, Value};
use std::{collections::BTreeSet, net::Ipv4Addr, sync::Arc};

fn invalid(message: &str) -> Failure {
    Failure::new(422, "Invalid", message)
}
pub(crate) async fn assign(
    store: &Arc<dyn Storage>,
    value: &mut Value,
    old: Option<&Value>,
) -> Result<()> {
    let spec = &mut value["spec"];
    if let Some(old) = old {
        if old["spec"]["type"] != spec["type"] {
            return Err(invalid("Service type transitions are not yet supported"));
        }
        for field in ["clusterIP", "clusterIPs", "ipFamilies", "ipFamilyPolicy"] {
            if spec.get(field).is_none_or(Value::is_null) {
                spec[field] = old["spec"][field].clone();
            }
            if spec[field] != old["spec"][field] {
                return Err(invalid("Service IP allocation is immutable"));
            }
        }
        return Ok(());
    }
    if spec["type"] == "ExternalName" {
        if spec["clusterIP"].as_str().is_some_and(|s| !s.is_empty())
            || spec["clusterIPs"].as_array().is_some_and(|v| !v.is_empty())
        {
            return Err(invalid("ExternalName cannot allocate a ClusterIP"));
        }
        return Ok(());
    }
    if spec["ipFamilyPolicy"]
        .as_str()
        .is_some_and(|v| v != "SingleStack")
        || spec["ipFamilies"]
            .as_array()
            .is_some_and(|v| v != &vec![json!("IPv4")])
    {
        return Err(invalid(
            "the M1 Service allocator supports IPv4 SingleStack",
        ));
    }
    let requested = spec["clusterIP"].as_str().unwrap_or("");
    let selected = if requested == "None" {
        "None".to_owned()
    } else {
        let mut used = BTreeSet::new();
        let mut selection = ListSelect::new("/registry/services/");
        loop {
            let page = store.list(selection.clone()).await?;
            selection.at_revision = Some(page.revision);
            for stored in page.items {
                let current = object(stored)?;
                if let Some(ip) = current["spec"]["clusterIP"]
                    .as_str()
                    .and_then(|s| s.parse::<Ipv4Addr>().ok())
                {
                    used.insert(ip);
                }
            }
            let Some(cursor) = page.next_after else {
                break;
            };
            selection.start_after = Some(cursor);
        }
        // Matches the parent single-stack default. .1 is reserved for the API
        // Service; .0 and .255.255 are the /16 network and broadcast addresses.
        let base = u32::from(Ipv4Addr::new(10, 43, 0, 0));
        if requested.is_empty() {
            (2..65535)
                .map(|offset| Ipv4Addr::from(base + offset))
                .find(|ip| !used.contains(ip))
                .ok_or_else(|| Failure::new(503, "ServiceUnavailable", "Service CIDR exhausted"))?
                .to_string()
        } else {
            let ip = requested
                .parse::<Ipv4Addr>()
                .map_err(|_| invalid("invalid ClusterIP"))?;
            let number = u32::from(ip);
            if !(base + 2..base + 65535).contains(&number) {
                return Err(invalid("ClusterIP outside allocatable 10.43.0.0/16 range"));
            }
            if used.contains(&ip) {
                return Err(invalid("ClusterIP already allocated"));
            }
            requested.to_owned()
        }
    };
    if spec["clusterIPs"]
        .as_array()
        .is_some_and(|v| v != &vec![json!(selected)])
    {
        return Err(invalid("clusterIPs must match clusterIP"));
    }
    spec["clusterIP"] = json!(selected);
    spec["clusterIPs"] = json!([selected]);
    spec["ipFamilies"] = json!(["IPv4"]);
    spec["ipFamilyPolicy"] = json!("SingleStack");
    Ok(())
}
