use crate::{Error, Result, MAX_RULESET, TABLE};
use ipnet::Ipv4Net;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write,
    net::Ipv4Addr,
};
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Protocol {
    Tcp,
    Udp,
}
impl Protocol {
    fn parse(v: &Value) -> Result<Self> {
        match v.as_str().unwrap_or("TCP") {
            "TCP" => Ok(Self::Tcp),
            "UDP" => Ok(Self::Udp),
            _ => Err(Error::Invalid("proxy supports TCP and UDP Service ports")),
        }
    }
    fn nft(&self) -> &'static str {
        match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Backend {
    ip: Ipv4Addr,
    port: u16,
}
#[derive(Debug)]
struct Frontend {
    id: String,
    ip: Ipv4Addr,
    port: u16,
    protocol: Protocol,
    local: bool,
    backends: BTreeSet<Backend>,
}
#[derive(Debug)]
pub struct Plan {
    pod_cidrs: BTreeSet<Ipv4Net>,
    services: Vec<Frontend>,
}
fn values(v: &Value) -> impl Iterator<Item = &Value> {
    v.as_array().into_iter().flatten()
}
fn text<'a>(v: &'a Value, k: &str) -> Result<&'a str> {
    v[k].as_str()
        .filter(|s| !s.is_empty() && s.len() <= 256)
        .ok_or(Error::Invalid("missing resource identity"))
}
fn port(v: &Value) -> Result<u16> {
    v.as_u64()
        .filter(|n| (1..=65535).contains(n))
        .map(|n| n as u16)
        .ok_or(Error::Invalid("invalid proxy port"))
}
fn ip(v: &Value) -> Result<Ipv4Addr> {
    let ip = v
        .as_str()
        .and_then(|s| s.parse::<Ipv4Addr>().ok())
        .ok_or(Error::Invalid("invalid IPv4 address"))?;
    if ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.octets()[0] == 0
        || ip.octets()[0] >= 240
    {
        return Err(Error::Invalid("endpoint must be a unicast IPv4 address"));
    }
    Ok(ip)
}
fn digest(s: &str) -> String {
    format!("{:x}", Sha256::digest(s.as_bytes()))
}
fn endpoint_chain(service: &Frontend, backend: &Backend) -> String {
    format!(
        "ep_{}",
        digest(&format!("{}:{}:{}", service.id, backend.ip, backend.port))
    )
}

/// Select current Service backends from complete API lists. A Service-owned
/// EndpointSlice from an older Service UID is ignored; manual slices without a
/// Service owner remain usable by namespace/name as in the Kubernetes API.
pub fn plan(
    services: &[Value],
    slices: &[Value],
    nodes: &[Value],
    node_name: &str,
) -> Result<Plan> {
    if !h3s_api::valid_node_name(node_name)
        || services.len() > 8192
        || slices.len() > 8192
        || nodes.len() > 8192
    {
        return Err(Error::Invalid("invalid or excessive proxy snapshot"));
    }
    let mut pod_cidrs = BTreeSet::<Ipv4Net>::new();
    let service_range = "10.43.0.0/16".parse::<Ipv4Net>().unwrap();
    let mut own = false;
    for node in nodes {
        if let Some(cidr) = node["spec"]["podCIDR"].as_str().filter(|s| !s.is_empty()) {
            let subnet = cidr
                .parse::<Ipv4Net>()
                .map_err(|_| Error::Invalid("node CIDR must be IPv4"))?;
            if subnet.addr() != subnet.network()
                || subnet.to_string() != cidr
                || !subnet.network().is_private()
                || !subnet.broadcast().is_private()
                || subnet.contains(&service_range.network())
                || service_range.contains(&subnet.network())
            {
                return Err(Error::Invalid("invalid node CIDR"));
            }
            if !pod_cidrs.insert(subnet) {
                return Err(Error::Invalid("duplicate node CIDR"));
            }
            if node["metadata"]["name"] == node_name {
                own = true;
            }
        }
    }
    let mut previous: Option<Ipv4Net> = None;
    for cidr in &pod_cidrs {
        if previous.is_some_and(|p| p.contains(&cidr.network())) {
            return Err(Error::Invalid("overlapping node CIDRs"));
        }
        previous = Some(*cidr);
    }
    if !own {
        return Err(Error::Invalid("local node has no allocated Pod CIDR yet"));
    }
    let mut by_service: BTreeMap<(&str, &str), Vec<&Value>> = BTreeMap::new();
    for slice in slices {
        if slice["addressType"] != "IPv4" || !slice["metadata"]["deletionTimestamp"].is_null() {
            continue;
        }
        let Some(name) = slice["metadata"]["labels"]["kubernetes.io/service-name"].as_str() else {
            continue;
        };
        let namespace = text(&slice["metadata"], "namespace")?;
        by_service.entry((namespace, name)).or_default().push(slice);
    }
    let mut frontends = Vec::new();
    let mut tuples = BTreeSet::new();
    let mut ids = BTreeSet::new();
    let mut total_backends = 0;
    for service in services {
        let spec = &service["spec"];
        if !service["metadata"]["deletionTimestamp"].is_null()
            || h3s_api::service_forwarding::ServiceForwarding::classify(spec).skip_in_plan()
        {
            continue;
        }
        let uid = text(&service["metadata"], "uid")?;
        let name = text(&service["metadata"], "name")?;
        let ns = text(&service["metadata"], "namespace")?;
        let cluster_ip = ip(&spec["clusterIP"])?;
        if !"10.43.0.0/16"
            .parse::<Ipv4Net>()
            .unwrap()
            .contains(&cluster_ip)
        {
            return Err(Error::Invalid("Service IP is outside the configured range"));
        }
        let local = match spec["internalTrafficPolicy"].as_str().unwrap_or("Cluster") {
            "Cluster" => false,
            "Local" => true,
            _ => return Err(Error::Invalid("unsupported internal traffic policy")),
        };
        for p in values(&spec["ports"]) {
            let service_port = port(&p["port"])?;
            let protocol = Protocol::parse(&p["protocol"])?;
            let port_name = p["name"].as_str().unwrap_or("");
            if !tuples.insert((cluster_ip, service_port, protocol.clone())) {
                return Err(Error::Invalid("duplicate Service frontend"));
            }
            let mut backends = BTreeSet::new();
            for slice in by_service.get(&(ns, name)).into_iter().flatten() {
                if values(&slice["metadata"]["ownerReferences"])
                    .any(|o| o["kind"] == "Service" && (o["uid"] != uid || o["name"] != name))
                {
                    continue;
                }
                for sp in values(&slice["ports"]) {
                    if sp["name"].as_str().unwrap_or("") != port_name
                        || sp["protocol"].as_str().unwrap_or("TCP") != protocol.nft().to_uppercase()
                        || sp["port"].is_null()
                    {
                        continue;
                    }
                    let target_port = port(&sp["port"])?;
                    for endpoint in values(&slice["endpoints"]) {
                        if endpoint["conditions"]["ready"] == false
                            || endpoint["conditions"]["terminating"] == true
                            || (local && endpoint["nodeName"] != node_name)
                        {
                            continue;
                        }
                        for address in values(&endpoint["addresses"]) {
                            let address = ip(address)?;
                            if "10.43.0.0/16"
                                .parse::<Ipv4Net>()
                                .unwrap()
                                .contains(&address)
                            {
                                return Err(Error::Invalid("Service IP cannot be an endpoint"));
                            }
                            if backends.insert(Backend {
                                ip: address,
                                port: target_port,
                            }) {
                                total_backends += 1;
                                if total_backends > 16384 {
                                    return Err(Error::Invalid("too many Service backends"));
                                }
                            }
                        }
                    }
                }
            }
            let id = format!(
                "svc_{}",
                digest(&format!(
                    "{ns}/{name}/{uid}/{cluster_ip}/{service_port}/{}",
                    protocol.nft()
                ))
            );
            if !ids.insert(id.clone()) {
                return Err(Error::Invalid("duplicate Service chain identity"));
            }
            frontends.push(Frontend {
                id,
                ip: cluster_ip,
                port: service_port,
                protocol,
                local,
                backends,
            });
            if frontends.len() > 4096 {
                return Err(Error::Invalid("too many Service ports"));
            }
        }
    }
    frontends.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(Plan {
        pod_cidrs,
        services: frontends,
    })
}
impl Plan {
    pub fn render(&self, owner: &str) -> Result<String> {
        if owner.len() != 64 || !owner.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::Invalid("invalid table ownership digest"));
        }
        let mut out=format!("add table ip {TABLE}\ndelete table ip {TABLE}\ntable ip {TABLE} {{\n comment \"hedronetes.io/service-proxy/v1:{owner}\"\n");
        for hook in ["prerouting", "output"] {
            writeln!(out," chain nat_{hook} {{ type nat hook {hook} priority -100; policy accept; jump services; }}").unwrap();
        }
        writeln!(out, " chain services {{").unwrap();
        for s in &self.services {
            if !s.backends.is_empty() {
                writeln!(
                    out,
                    "  ip daddr {} {} dport {} counter jump {}",
                    s.ip,
                    s.protocol.nft(),
                    s.port,
                    s.id
                )
                .unwrap();
            }
        }
        writeln!(out, " }}").unwrap();
        for hook in ["input", "forward", "output"] {
            writeln!(
                out,
                " chain reject_{hook} {{ type filter hook {hook} priority -10; policy accept;"
            )
            .unwrap();
            for s in &self.services {
                if s.backends.is_empty() {
                    writeln!(
                        out,
                        "  ip daddr {} {} dport {} counter {}",
                        s.ip,
                        s.protocol.nft(),
                        s.port,
                        if s.local { "drop" } else { "reject" }
                    )
                    .unwrap();
                }
            }
            writeln!(out, " }}").unwrap();
        }
        // Original Service tuple scopes SNAT to our DNAT connections. No
        // packet mark is reserved or changed, and direct Pod traffic is intact.
        let cidrs = self
            .pod_cidrs
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(
            out,
            " chain postrouting {{ type nat hook postrouting priority 90; policy accept;"
        )
        .unwrap();
        for s in &self.services {
            if s.backends.is_empty() {
                continue;
            }
            let original = format!(
                "ct status dnat ct original ip daddr {} meta l4proto {} ct original proto-dst {}",
                s.ip,
                s.protocol.nft(),
                s.port
            );
            writeln!(
                out,
                "  {original} ip saddr != {{ {cidrs} }} counter masquerade"
            )
            .unwrap();
            for b in &s.backends {
                writeln!(
                    out,
                    "  {original} ip saddr {} ip daddr {} {} dport {} counter masquerade",
                    b.ip,
                    b.ip,
                    s.protocol.nft(),
                    b.port
                )
                .unwrap();
            }
        }
        writeln!(out, " }}").unwrap();
        for s in &self.services {
            if s.backends.is_empty() {
                continue;
            }
            writeln!(out, " chain {} {{", s.id).unwrap();
            if s.backends.len() == 1 {
                writeln!(
                    out,
                    "  jump {}",
                    endpoint_chain(s, s.backends.first().unwrap())
                )
                .unwrap();
            } else {
                let entries = s
                    .backends
                    .iter()
                    .enumerate()
                    .map(|(i, b)| format!("{i} : jump {}", endpoint_chain(s, b)))
                    .collect::<Vec<_>>()
                    .join(", ");
                writeln!(
                    out,
                    "  numgen random mod {} vmap {{ {entries} }}",
                    s.backends.len()
                )
                .unwrap();
            }
            writeln!(out, " }}").unwrap();
            for b in &s.backends {
                writeln!(
                    out,
                    " chain {} {{ meta l4proto {} counter dnat to {}:{}; }}",
                    endpoint_chain(s, b),
                    s.protocol.nft(),
                    b.ip,
                    b.port
                )
                .unwrap();
            }
            if out.len() > MAX_RULESET {
                return Err(Error::Invalid("ruleset exceeds byte limit"));
            }
        }
        writeln!(out, "}}").unwrap();
        if out.len() > MAX_RULESET {
            return Err(Error::Invalid("ruleset exceeds byte limit"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn nodes() -> Vec<Value> {
        vec![
            json!({"metadata":{"name":"server"},"spec":{"podCIDR":"10.42.0.0/24"}}),
            json!({"metadata":{"name":"worker"},"spec":{"podCIDR":"10.42.2.0/24"}}),
        ]
    }
    fn service() -> Value {
        json!({"metadata":{"name":"web","namespace":"test","uid":"new-service"},"spec":{"clusterIP":"10.43.0.10","ports":[{"name":"http","port":80,"protocol":"TCP"},{"name":"dns","port":53,"protocol":"UDP"}]}})
    }
    fn slice() -> Value {
        json!({"metadata":{"namespace":"test","labels":{"kubernetes.io/service-name":"web"},"ownerReferences":[{"kind":"Service","name":"web","uid":"new-service"}]},"addressType":"IPv4","ports":[{"name":"http","port":8080,"protocol":"TCP"},{"name":"dns","port":1053,"protocol":"UDP"}],"endpoints":[{"addresses":["10.42.0.2"],"nodeName":"server","conditions":{"ready":true}},{"addresses":["10.42.2.2"],"nodeName":"worker"}]})
    }
    #[test]
    fn discovery_filters_stale_unready_terminating_foreign_and_duplicate_endpoints() {
        let a = slice();
        let mut stale = a.clone();
        stale["metadata"]["ownerReferences"][0]["uid"] = json!("old-service");
        stale["endpoints"][0]["addresses"] = json!(["10.42.0.99"]);
        let mut foreign = a.clone();
        foreign["metadata"]["namespace"] = json!("other");
        foreign["endpoints"][0]["addresses"] = json!(["10.42.0.98"]);
        let mut unavailable = a.clone();
        unavailable["endpoints"][0]["conditions"]["ready"] = json!(false);
        unavailable["endpoints"][0]["addresses"] = json!(["10.42.0.97"]);
        unavailable["endpoints"][1]["conditions"] = json!({"ready":true,"terminating":true});
        unavailable["endpoints"][1]["addresses"] = json!(["10.42.2.97"]);
        let p = plan(
            &[service()],
            &[a.clone(), a, stale, foreign, unavailable],
            &nodes(),
            "server",
        )
        .unwrap();
        assert_eq!(p.services.len(), 2);
        for s in &p.services {
            assert_eq!(s.backends.len(), 2);
            assert_eq!(
                s.backends
                    .iter()
                    .map(|b| b.ip.to_string())
                    .collect::<Vec<_>>(),
                ["10.42.0.2", "10.42.2.2"]
            );
            assert!(s.backends.iter().all(|b| b.port
                == if s.protocol == Protocol::Tcp {
                    8080
                } else {
                    1053
                }));
        }
    }
    #[test]
    fn local_policy_empty_services_and_manual_slices_have_explicit_behavior() {
        let mut s = service();
        s["spec"]["internalTrafficPolicy"] = json!("Local");
        let mut slice = slice();
        slice["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("ownerReferences");
        let p = plan(&[s.clone()], &[slice.clone()], &nodes(), "server").unwrap();
        assert!(p
            .services
            .iter()
            .all(|s| s.backends.len() == 1
                && s.backends.first().unwrap().ip.to_string() == "10.42.0.2"));
        slice["endpoints"][0]["conditions"]["ready"] = json!(false);
        let rules = plan(&[s], &[slice], &nodes(), "server")
            .unwrap()
            .render(&"a".repeat(64))
            .unwrap();
        assert!(rules.contains("counter drop"));
        assert!(!rules.contains("dnat to"));
        let rules = plan(&[service()], &[], &nodes(), "server")
            .unwrap()
            .render(&"a".repeat(64))
            .unwrap();
        assert!(rules.contains("counter reject"));
    }
    #[test]
    fn rendered_transactions_are_stable_scoped_and_include_hairpin_and_host_snat() {
        let s = service();
        let a = slice();
        let p = plan(
            std::slice::from_ref(&s),
            std::slice::from_ref(&a),
            &nodes(),
            "server",
        )
        .unwrap();
        let rules = p.render(&"a".repeat(64)).unwrap();
        assert!(rules.starts_with("add table ip h3s_proxy\ndelete table ip h3s_proxy\n"));
        assert!(!rules.contains("flush ruleset"));
        assert!(!rules.contains("mark"));
        assert!(rules.contains("numgen random mod 2 vmap"));
        assert!(rules.contains("ct original ip daddr 10.43.0.10"));
        assert!(rules
            .contains("ip saddr 10.42.0.2 ip daddr 10.42.0.2 tcp dport 8080 counter masquerade"));
        assert!(rules.contains("ip saddr != { 10.42.0.0/24, 10.42.2.0/24 } counter masquerade"));
        let mut reversed = a.clone();
        reversed["endpoints"].as_array_mut().unwrap().reverse();
        assert_eq!(
            rules,
            plan(&[s], &[reversed, a], &nodes(), "server")
                .unwrap()
                .render(&"a".repeat(64))
                .unwrap()
        );
        assert!(p.render("\";flush ruleset;").is_err());
    }
    #[test]
    fn invalid_addresses_policy_and_incomplete_topology_do_not_produce_rules() {
        for cidr in [
            "10.0.0.0/8",
            "10.42.0.0/24",
            "10.42.0.0/23",
            "10.43.0.0/24",
            "10.0.0.0/7",
        ] {
            let mut topology = nodes();
            topology[1]["spec"]["podCIDR"] = json!(cidr);
            assert!(
                plan(&[service()], &[slice()], &topology, "server").is_err(),
                "{cidr}"
            );
        }
        for address in [
            "127.0.0.1",
            "169.254.1.2",
            "224.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "10.43.0.4",
            "192.0.2.2; flush ruleset",
        ] {
            let mut a = slice();
            a["endpoints"][0]["addresses"] = json!([address]);
            assert!(
                plan(&[service()], &[a], &nodes(), "server").is_err(),
                "{address}"
            );
        }
        let mut s = service();
        s["spec"]["sessionAffinity"] = json!("ClientIP");
        assert!(plan(&[s], &[slice()], &nodes(), "server")
            .unwrap()
            .services
            .is_empty());
        assert!(plan(&[service()], &[slice()], &[], "server").is_err());
        let mut headless = service();
        headless["spec"]["clusterIP"] = json!("None");
        let mut external = service();
        external["spec"]["type"] = json!("ExternalName");
        assert!(plan(&[headless, external], &[slice()], &nodes(), "server")
            .unwrap()
            .services
            .is_empty());
    }
}
