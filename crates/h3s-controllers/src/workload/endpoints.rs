//! Service-selected Pod addresses, grouped by IP family and resolved port set.
//! API-only reconciliation; a complete bounded snapshot precedes every write.
use super::*;
use futures_util::StreamExt;
use k8s_openapi::api::{core::v1::Service, discovery::v1::EndpointSlice};
use kube::runtime::{controller::Action, watcher, Controller};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::IpAddr,
    sync::Arc,
};

pub const ENDPOINT_CONTROLLER_ID: &str = "system:h3s:endpointslice-controller";
const MANAGER: &str = "hedronetes.io/endpointslice-controller";
const MANAGED_BY: &str = "endpointslice.kubernetes.io/managed-by";
const SERVICE_NAME: &str = "kubernetes.io/service-name";
const SLICE_SIZE: usize = 100;

pub async fn run_endpoint_controller(client: Client) -> Result<(), Error> {
    let controller = Controller::new(
        Api::<Service>::all(client.clone()),
        watcher::Config::default(),
    )
    .owns(
        Api::<EndpointSlice>::all(client.clone()),
        watcher::Config::default(),
    )
    .run(
        |service, client: Arc<Client>| async move {
            endpoints_once(
                (*client).clone(),
                &service
                    .namespace()
                    .ok_or(Error::Invalid("Service namespace missing"))?,
                &service.name_any(),
            )
            .await?;
            Ok(Action::requeue(Duration::from_secs(2)))
        },
        |_, _: &Error, _| Action::requeue(Duration::from_secs(5)),
        Arc::new(client.clone()),
    )
    .for_each(|result| async move {
        if let Err(error) = result {
            eprintln!("EndpointSlice controller: {error}");
        }
    });
    let gc = async move {
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match tokio::time::timeout(Duration::from_secs(60), endpoint_gc_once(client.clone()))
                .await
            {
                Err(error) => eprintln!("EndpointSlice cleanup timed out: {error}"),
                Ok(Err(error)) => eprintln!("EndpointSlice cleanup: {}", failure(&error)),
                Ok(Ok(_)) => {}
            }
        }
    };
    tokio::select! { _ = controller => Err(Error::Stopped), _ = gc => Err(Error::Stopped) }
}

fn managed(slice: &Value) -> bool {
    slice["metadata"]["labels"][MANAGED_BY] == MANAGER
}
fn ours(slice: &Value, service: &Value) -> bool {
    managed(slice)
        && values(&slice["metadata"]["ownerReferences"]).count() == 1
        && owned(slice, service)
}
async fn unchanged(api: &Api<Service>, old: &Value) -> Result<bool, Error> {
    let (_, name, uid) = identity(old)?;
    Ok(api.get_opt(name).await?.is_some_and(|s| {
        s.uid().as_deref() == Some(uid)
            && s.metadata.deletion_timestamp.is_none()
            && s.resource_version().as_deref() == old["metadata"]["resourceVersion"].as_str()
    }))
}
pub async fn endpoints_once(client: Client, namespace: &str, name: &str) -> Result<(), Error> {
    let services = Api::<Service>::namespaced(client.clone(), namespace);
    let Some(service) = services.get_opt(name).await? else {
        return Ok(());
    };
    let service = serde_json::to_value(service)?;
    if !service["metadata"]["deletionTimestamp"].is_null() {
        return Ok(());
    }
    let pods = list(&Api::<Pod>::namespaced(client.clone(), namespace)).await?;
    let slices_api = Api::<EndpointSlice>::namespaced(client, namespace);
    let slices = list(&slices_api).await?;
    let desired = plan(&service, &pods)?;
    let names: BTreeSet<_> = desired
        .iter()
        .map(|s| text(&s["metadata"], "name"))
        .collect::<Result<_, _>>()?;
    // Publish replacements before collecting obsolete port groups/slices.
    for value in &desired {
        let name = text(&value["metadata"], "name")?;
        if let Some(existing) = slices.iter().find(|s| s["metadata"]["name"] == name) {
            if !ours(existing, &service) {
                return Err(Error::Invalid(
                    "EndpointSlice name is owned by another manager or Service",
                ));
            }
            let mut next = existing.clone();
            for field in ["addressType", "endpoints", "ports"] {
                next[field] = value[field].clone();
            }
            for label in [MANAGED_BY, SERVICE_NAME] {
                next["metadata"]["labels"][label] = value["metadata"]["labels"][label].clone();
            }
            if next != *existing {
                if !unchanged(&services, &service).await? {
                    return Ok(());
                }
                replace(&slices_api, &next).await?;
            }
        } else {
            if !unchanged(&services, &service).await? {
                return Ok(());
            }
            let slice: EndpointSlice = serde_json::from_value(value.clone())?;
            // An ambiguous/create-conflict response ends this cycle. A new LIST
            // resolves it before retry, instead of guessing ownership or counts.
            slices_api.create(&PostParams::default(), &slice).await?;
        }
    }
    for slice in &slices {
        if ours(slice, &service) && !names.contains(text(&slice["metadata"], "name")?) {
            if !unchanged(&services, &service).await? {
                return Ok(());
            }
            delete(&slices_api, slice).await?;
        }
    }
    Ok(())
}
pub async fn endpoint_gc_once(client: Client) -> Result<usize, Error> {
    let slices = list(&Api::<EndpointSlice>::all(client.clone())).await?;
    let mut removed = 0;
    for slice in slices {
        if !managed(&slice) || values(&slice["metadata"]["ownerReferences"]).count() != 1 {
            continue;
        }
        let Some(reference) = controller(&slice) else {
            continue;
        };
        if reference["kind"] != "Service" || reference["apiVersion"] != "v1" {
            continue;
        }
        let (namespace, _, _) = identity(&slice)?;
        let service = Api::<Service>::namespaced(client.clone(), namespace)
            .get_opt(text(reference, "name")?)
            .await?;
        if service.is_some_and(|s| s.uid().as_deref() == reference["uid"].as_str()) {
            continue;
        }
        removed += usize::from(
            delete(
                &Api::<EndpointSlice>::namespaced(client.clone(), namespace),
                &slice,
            )
            .await?,
        );
    }
    Ok(removed)
}

fn families(service: &Value) -> Result<Vec<&'static str>, Error> {
    let mut out = BTreeSet::new();
    for family in values(&service["spec"]["ipFamilies"]) {
        out.insert(match family.as_str() {
            Some("IPv4") => "IPv4",
            Some("IPv6") => "IPv6",
            _ => return Err(Error::Invalid("invalid Service IP family")),
        });
    }
    if out.is_empty() {
        match service["spec"]["clusterIP"].as_str() {
            Some("None") | None => {
                out.insert("IPv4");
            }
            Some(ip) => {
                out.insert(
                    if ip
                        .parse::<IpAddr>()
                        .map_err(|_| Error::Invalid("invalid Service IP"))?
                        .is_ipv4()
                    {
                        "IPv4"
                    } else {
                        "IPv6"
                    },
                );
            }
        }
    }
    Ok(out.into_iter().collect())
}
fn selected(service: &Value, p: &Value) -> bool {
    p["metadata"]["namespace"] == service["metadata"]["namespace"]
        && service["spec"]["selector"].as_object().is_some_and(|s| {
            !s.is_empty() && s.iter().all(|(k, v)| p["metadata"]["labels"][k] == *v)
        })
        && !matches!(p["status"]["phase"].as_str(), Some("Succeeded" | "Failed"))
        && p["spec"]["nodeName"]
            .as_str()
            .is_some_and(|s| !s.is_empty())
}
fn port_set(service: &Value, pod: Option<&Value>) -> Result<Vec<Value>, Error> {
    let mut ports = vec![];
    for p in values(&service["spec"]["ports"]) {
        let protocol = p["protocol"].as_str().unwrap_or("TCP");
        if !matches!(protocol, "TCP" | "UDP" | "SCTP") {
            return Err(Error::Invalid("invalid Service port protocol"));
        }
        let target = &p["targetPort"];
        let port = if let Some(name) = target.as_str() {
            if let Some(pod) = pod {
                let Some(port) = values(&pod["spec"]["containers"])
                    .flat_map(|c| values(&c["ports"]))
                    .find(|v| {
                        v["name"] == name && v["protocol"].as_str().unwrap_or("TCP") == protocol
                    })
                    .and_then(|p| p["containerPort"].as_i64())
                else {
                    continue;
                };
                Some(port)
            } else {
                None
            }
        } else {
            Some(
                target
                    .as_i64()
                    .or(p["port"].as_i64())
                    .ok_or(Error::Invalid("invalid Service target port"))?,
            )
        };
        if port.is_some_and(|p| !(1..=65535).contains(&p)) {
            return Err(Error::Invalid("invalid endpoint port"));
        }
        let mut v =
            json!({"name":p["name"].as_str().unwrap_or(""),"protocol":protocol,"port":port});
        if !p["appProtocol"].is_null() {
            v["appProtocol"] = p["appProtocol"].clone();
        }
        ports.push(v);
    }
    ports.sort_by_key(Value::to_string);
    Ok(ports)
}
fn address(p: &Value, family: &str) -> Option<IpAddr> {
    values(&p["status"]["podIPs"]).filter_map(|p| p["ip"].as_str()).chain(p["status"]["podIP"].as_str())
        .filter_map(|s| s.parse::<IpAddr>().ok())
        .find(|ip| (ip.is_ipv4() == (family == "IPv4")) && !ip.is_loopback() && !ip.is_unspecified() && !ip.is_multicast()
            && !matches!(ip, IpAddr::V4(ip) if ip.is_broadcast() || ip.is_link_local())
            && !matches!(ip, IpAddr::V6(ip) if ip.is_unicast_link_local() || ip.to_ipv4_mapped().is_some()))
}
fn plan(service: &Value, pods: &[Value]) -> Result<Vec<Value>, Error> {
    let (namespace, name, uid) = identity(service)?;
    if service["spec"]["type"] == "ExternalName"
        || service["spec"]["selector"]
            .as_object()
            .is_none_or(|s| s.is_empty())
    {
        return Ok(vec![]);
    }
    let mut desired = vec![];
    for family in families(service)? {
        let mut groups: BTreeMap<String, (Vec<Value>, Vec<Value>)> = BTreeMap::new();
        for p in pods.iter().filter(|p| selected(service, p)) {
            let Some(ip) = address(p, family) else {
                continue;
            };
            let ports = port_set(service, Some(p))?;
            if ports.is_empty() {
                continue;
            }
            let group = serde_json::to_string(&ports)?;
            let (_, pod_name, pod_uid) = identity(p)?;
            let serving = values(&p["status"]["conditions"])
                .any(|c| c["type"] == "Ready" && c["status"] == "True");
            let terminating = !p["metadata"]["deletionTimestamp"].is_null();
            let ready =
                service["spec"]["publishNotReadyAddresses"] == true || (serving && !terminating);
            let mut endpoint = json!({"addresses":[ip.to_string()],"conditions":{"ready":ready,"serving":serving,"terminating":terminating},"nodeName":p["spec"]["nodeName"],"targetRef":{"apiVersion":"v1","kind":"Pod","namespace":namespace,"name":pod_name,"uid":pod_uid}});
            if p["spec"]["subdomain"] == name
                && p["spec"]["hostname"]
                    .as_str()
                    .is_some_and(|s| !s.is_empty())
            {
                endpoint["hostname"] = p["spec"]["hostname"].clone();
            }
            groups
                .entry(group)
                .or_insert_with(|| (ports, vec![]))
                .1
                .push(endpoint);
        }
        if groups.is_empty() {
            let ports = port_set(service, None)?;
            groups.insert(serde_json::to_string(&ports)?, (ports, vec![]));
        }
        for (key, (ports, mut endpoints)) in groups {
            endpoints.sort_by_key(|e| {
                (
                    e["targetRef"]["uid"].to_string(),
                    e["addresses"].to_string(),
                )
            });
            // Keep one empty slice to advertise the Service's family/ports.
            for chunk in 0..endpoints.len().div_ceil(SLICE_SIZE).max(1) {
                let hash = format!(
                    "{:x}",
                    Sha256::digest(serde_json::to_vec(&json!([uid, family, key, chunk]))?)
                );
                let prefix = name
                    .get(..name.len().min(34))
                    .unwrap_or("service")
                    .trim_end_matches(['-', '.']);
                let slice_name = format!("{prefix}-{}", &hash[..20]);
                let start = chunk * SLICE_SIZE;
                desired.push(json!({"apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice","metadata":{"namespace":namespace,"name":slice_name,"labels":{SERVICE_NAME:name,MANAGED_BY:MANAGER},"ownerReferences":[owner(service)]},"addressType":family,"ports":ports,"endpoints":endpoints[start..endpoints.len().min(start+SLICE_SIZE)]}));
            }
        }
    }
    if desired.len() > LIMIT as usize {
        return Err(Error::Invalid("too many desired EndpointSlices"));
    }
    Ok(desired)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn service() -> Value {
        json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"web","namespace":"default","uid":"service-uid"},"spec":{"clusterIP":"10.43.0.5","selector":{"app":"web"},"ports":[{"name":"http","port":80,"targetPort":"http"}]}})
    }
    fn pod(n: usize) -> Value {
        json!({"apiVersion":"v1","kind":"Pod","metadata":{"name":format!("web-{n}"),"namespace":"default","uid":format!("pod-{n:04}"),"labels":{"app":"web"}},"spec":{"nodeName":"worker","containers":[{"ports":[{"name":"http","containerPort":8080}]}]},"status":{"phase":"Running","podIP":format!("10.42.2.{}",n%250+1),"conditions":[{"type":"Ready","status":"True"}]}})
    }
    #[test]
    fn endpoint_slices_partition_stably_at_one_hundred_and_preserve_ownership() {
        let service = service();
        let pods: Vec<_> = (0..201).map(pod).collect();
        let planned = plan(&service, &pods).unwrap();
        assert_eq!(
            planned
                .iter()
                .map(|s| s["endpoints"].as_array().unwrap().len())
                .collect::<Vec<_>>(),
            [100, 100, 1]
        );
        assert!(planned.iter().all(|s| ours(s, &service)
            && s["metadata"]["labels"][SERVICE_NAME] == "web"
            && s["ports"][0]["port"] == 8080));
        let mut shuffled = pods.clone();
        shuffled.reverse();
        assert_eq!(plan(&service, &shuffled).unwrap(), planned);
        let empty = plan(&service, &[]).unwrap();
        assert_eq!(empty.len(), 1);
        assert_eq!(empty[0]["endpoints"], json!([]));
        assert!(empty[0]["ports"][0]["port"].is_null());
        let mut changed = service.clone();
        changed["metadata"]["uid"] = json!("replacement-service");
        assert_ne!(
            plan(&changed, &pods).unwrap()[0]["metadata"]["name"],
            planned[0]["metadata"]["name"]
        );
        let mut stolen = planned[0].clone();
        stolen["metadata"]["labels"][MANAGED_BY] = json!("another-controller");
        assert!(!ours(&stolen, &service));
    }
    #[test]
    fn endpoint_conditions_ports_families_and_selection_follow_pod_state() {
        let mut s = service();
        s["spec"]["ipFamilies"] = json!(["IPv4", "IPv6"]);
        let mut p = pod(0);
        p["status"]["podIPs"] = json!([{"ip":"10.42.2.1"},{"ip":"fd00::1"}]);
        p["metadata"]["deletionTimestamp"] = json!("2026-09-08T00:00:00Z");
        let slices = plan(&s, &[p.clone()]).unwrap();
        assert_eq!(slices.len(), 2);
        for slice in &slices {
            assert_eq!(
                slice["endpoints"][0]["conditions"],
                json!({"ready":false,"serving":true,"terminating":true})
            );
        }
        s["spec"]["publishNotReadyAddresses"] = json!(true);
        p["status"]["conditions"][0]["status"] = json!("False");
        assert_eq!(
            plan(&s, &[p.clone()]).unwrap()[0]["endpoints"][0]["conditions"],
            json!({"ready":true,"serving":false,"terminating":true})
        );
        let mut other = pod(1);
        other["spec"]["containers"][0]["ports"][0]["containerPort"] = json!(9090);
        assert_eq!(
            plan(&s, &[pod(0), other]).unwrap().len(),
            3,
            "two IPv4 port groups and one empty IPv6 slice"
        );
        for (field, value) in [("phase", json!("Succeeded")), ("phase", json!("Failed"))] {
            let mut p = pod(0);
            p["status"][field] = value;
            assert!(plan(&service(), &[p]).unwrap()[0]["endpoints"]
                .as_array()
                .unwrap()
                .is_empty());
        }
        for ip in [
            "127.0.0.1",
            "0.0.0.0",
            "224.1.1.1",
            "169.254.1.1",
            "255.255.255.255",
        ] {
            let mut p = pod(0);
            p["status"]["podIP"] = json!(ip);
            assert!(plan(&service(), &[p]).unwrap()[0]["endpoints"]
                .as_array()
                .unwrap()
                .is_empty());
        }
        let mut p = pod(0);
        p["metadata"]["namespace"] = json!("other");
        assert!(plan(&service(), &[p]).unwrap()[0]["endpoints"]
            .as_array()
            .unwrap()
            .is_empty());
        let mut s = service();
        s["spec"]["selector"] = json!({});
        assert!(plan(&s, &[pod(0)]).unwrap().is_empty());
        s["spec"]["selector"] = json!({"app":"web"});
        s["spec"]["type"] = json!("ExternalName");
        assert!(plan(&s, &[pod(0)]).unwrap().is_empty());
    }
}
