//! Bounded resolver configuration for CRI Pod sandboxes.
use crate::{invalid, pod, Result};
use h3s_cri::v1::DnsConfig;
use serde_json::Value;
use std::{
    io::Read,
    net::{IpAddr, Ipv4Addr},
};

#[derive(Clone, Debug)]
pub struct ClusterDns {
    server: Ipv4Addr,
    domain: String,
}
impl ClusterDns {
    pub fn new(server: Ipv4Addr, domain: String) -> Result<Self> {
        if server.is_unspecified()
            || server.is_loopback()
            || server.is_multicast()
            || server.is_link_local()
            || server.is_broadcast()
            || server.octets()[0] >= 240
            || server.octets()[0] == 0
            || !domain_name(&domain)
            || domain.ends_with('.')
        {
            return Err(invalid(
                "cluster DNS requires a unicast IPv4 address and canonical DNS domain",
            ));
        }
        // Reserve space for the longest namespace and the svc label.
        if domain.len() + 68 > 253 {
            return Err(invalid(
                "cluster domain leaves insufficient namespace search space",
            ));
        }
        Ok(Self { server, domain })
    }
}
fn label(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 63
        && s.as_bytes()[0].is_ascii_alphanumeric()
        && s.as_bytes()[s.len() - 1].is_ascii_alphanumeric()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
fn domain_name(s: &str) -> bool {
    let s = s.strip_suffix('.').unwrap_or(s);
    s.len() <= 253 && s.split('.').all(label)
}
fn token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
fn option(name: &str, value: Option<&str>) -> Result<String> {
    let value = value.filter(|v| !v.is_empty());
    if !token(name) || value.is_some_and(|v| !token(v)) {
        return Err(invalid("invalid resolver option"));
    }
    Ok(value.map_or_else(|| name.into(), |v| format!("{name}:{v}")))
}
fn unique(values: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    values.retain(|s| seen.insert(s.clone()));
}
fn set_option(options: &mut Vec<String>, value: String) {
    let name = value.split(':').next().unwrap();
    if let Some(old) = options
        .iter_mut()
        .find(|v| v.split(':').next() == Some(name))
    {
        *old = value;
    } else {
        options.push(value);
    }
}
fn bounded(config: &mut DnsConfig) -> Result<()> {
    for server in &mut config.servers {
        let ip: IpAddr = server
            .parse()
            .map_err(|_| invalid("invalid DNS nameserver"))?;
        if ip.is_unspecified() || ip.is_multicast() {
            return Err(invalid("DNS nameserver must be unicast"));
        }
        *server = ip.to_string();
    }
    if config.searches.iter().any(|s| s != "." && !domain_name(s)) {
        return Err(invalid("invalid DNS search domain"));
    }
    unique(&mut config.servers);
    unique(&mut config.searches);
    if config.servers.len() > 3
        || config.searches.len() > 32
        || config.searches.join(" ").len() > 2048
        || config.options.len() > 32
        || config.options.join(" ").len() > 4096
    {
        return Err(invalid("resolver configuration exceeds supported limits"));
    }
    Ok(())
}
fn overrides(config: &mut DnsConfig, value: &Value) -> Result<()> {
    if !value.is_null() && !value.is_object() {
        return Err(invalid("dnsConfig must be an object"));
    }
    pod::fields(value, &["nameservers", "searches", "options"])?;
    config.servers.extend(pod::strings(&value["nameservers"])?);
    config.searches.extend(pod::strings(&value["searches"])?);
    if !value["options"].is_null() && !value["options"].is_array() {
        return Err(invalid("DNS options must be an array"));
    }
    let options = value["options"].as_array();
    if options.is_some_and(|v| v.len() > 32) {
        return Err(invalid("too many DNS options"));
    }
    for item in options.into_iter().flatten() {
        pod::fields(item, &["name", "value"])?;
        let name = pod::text(item, "name")?;
        if !item["value"].is_null() && !item["value"].is_string() {
            return Err(invalid("DNS option value must be a string"));
        }
        set_option(&mut config.options, option(name, item["value"].as_str())?);
    }
    bounded(config)
}
fn host(text: &str) -> Result<DnsConfig> {
    if text.len() > 65536 {
        return Err(invalid("host resolver file exceeds limit"));
    }
    let mut config = DnsConfig::default();
    for line in text.lines() {
        let mut words = line
            .split(['#', ';'])
            .next()
            .unwrap_or("")
            .split_whitespace();
        match words.next() {
            Some("nameserver") => {
                if let Some(server) = words.next() {
                    config.servers.push(server.into());
                }
            }
            Some("search") => config.searches = words.map(str::to_owned).collect(),
            Some("domain") => config.searches = words.take(1).map(str::to_owned).collect(),
            Some("options") => {
                for word in words {
                    let (name, value) = word
                        .split_once(':')
                        .map_or((word, None), |(n, v)| (n, Some(v)));
                    set_option(&mut config.options, option(name, value)?);
                }
            }
            _ => {}
        }
    }
    bounded(&mut config)?;
    Ok(config)
}
fn resolve(p: &Value, cluster: Option<&ClusterDns>, host_resolver: &str) -> Result<DnsConfig> {
    let policy = p["spec"]["dnsPolicy"].as_str().unwrap_or("ClusterFirst");
    let mut config = match policy {
        "Default" => host(host_resolver)?,
        "None" => DnsConfig::default(),
        "ClusterFirst" => {
            let cluster = cluster.ok_or_else(|| invalid("ClusterFirst requires --cluster-dns"))?;
            let namespace = pod::text(&p["metadata"], "namespace")?;
            if !label(namespace) {
                return Err(invalid("invalid DNS namespace"));
            }
            let mut searches = vec![
                format!("{namespace}.svc.{}", cluster.domain),
                format!("svc.{}", cluster.domain),
                cluster.domain.clone(),
            ];
            searches.extend(host(host_resolver)?.searches);
            DnsConfig {
                servers: vec![cluster.server.to_string()],
                searches,
                options: vec!["ndots:5".into()],
            }
        }
        _ => return Err(invalid("unsupported DNS policy")),
    };
    overrides(&mut config, &p["spec"]["dnsConfig"])?;
    if policy == "None" && config.servers.is_empty() {
        return Err(invalid("dnsPolicy None requires at least one nameserver"));
    }
    Ok(config)
}
pub(crate) fn for_pod(p: &Value, cluster: Option<&ClusterDns>) -> Result<DnsConfig> {
    let mut text = String::new();
    if p["spec"]["dnsPolicy"] != "None" {
        std::fs::File::open("/etc/resolv.conf")?
            .take(65537)
            .read_to_string(&mut text)?;
    }
    resolve(p, cluster, &text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn cluster() -> ClusterDns {
        ClusterDns::new("10.43.0.10".parse().unwrap(), "cluster.local".into()).unwrap()
    }
    #[test]
    fn cluster_first_merges_namespace_searches_and_explicit_overrides() {
        let p = json!({"metadata":{"namespace":"apps"},"spec":{"dnsPolicy":"ClusterFirst","dnsConfig":{"nameservers":["10.43.0.10","1.1.1.1"],"searches":["apps.svc.cluster.local","example.org"],"options":[{"name":"ndots","value":"2"},{"name":"single-request-reopen"}]}}});
        let c = resolve(
            &p,
            Some(&cluster()),
            "nameserver 9.9.9.9\nsearch host.example\noptions timeout:1",
        )
        .unwrap();
        assert_eq!(c.servers, ["10.43.0.10", "1.1.1.1"]);
        assert_eq!(
            c.searches,
            [
                "apps.svc.cluster.local",
                "svc.cluster.local",
                "cluster.local",
                "host.example",
                "example.org"
            ]
        );
        assert_eq!(c.options, ["ndots:2", "single-request-reopen"]);
        assert!(resolve(&p, None, "").is_err());
        let mut implicit = p.clone();
        implicit["spec"]
            .as_object_mut()
            .unwrap()
            .remove("dnsPolicy");
        assert_eq!(
            resolve(&implicit, Some(&cluster()), "").unwrap(),
            resolve(&p, Some(&cluster()), "").unwrap()
        );
    }
    #[test]
    fn default_and_none_preserve_resolver_policy_and_option_precedence() {
        let p = json!({"spec":{"dnsPolicy":"Default","dnsConfig":{"options":[{"name":"timeout","value":"3"}]}}});
        let c=resolve(&p,None,"nameserver 127.0.0.53 # local\nsearch ignored.example\ndomain host.example\noptions timeout:1 attempts:2\noptions timeout:2 ; comment").unwrap();
        assert_eq!(c.servers, ["127.0.0.53"]);
        assert_eq!(c.searches, ["host.example"]);
        assert_eq!(c.options, ["timeout:3", "attempts:2"]);
        let p = json!({"spec":{"dnsPolicy":"None","dnsConfig":{"nameservers":["::1"],"searches":["."],"options":[{"name":"rotate"}]}}});
        let c = resolve(&p, None, "nameserver 9.9.9.9").unwrap();
        assert_eq!(c.servers, ["::1"]);
        assert_eq!(c.searches, ["."]);
        assert_eq!(c.options, ["rotate"]);
        assert!(resolve(&json!({"spec":{"dnsPolicy":"None"}}), None, "").is_err());
    }
    #[test]
    fn reject_malformed_overrides_injection_and_excessive_merged_configuration() {
        for config in [
            json!(true),
            json!({"nameservers":"1.1.1.1"}),
            json!({"searches":["ok\nnameserver 1.1.1.1"]}),
            json!({"options":{}}),
            json!({"options":[{"name":"ndots","value":2}]}),
            json!({"options":[{"name":"rotate\nsearch"}]}),
            json!({"nameservers":["0.0.0.0"]}),
            json!({"nameservers":["224.0.0.1"]}),
            json!({"extra":true}),
        ] {
            let p = json!({"metadata":{"namespace":"apps"},"spec":{"dnsPolicy":"ClusterFirst","dnsConfig":config}});
            assert!(resolve(&p, Some(&cluster()), "").is_err(), "{config}");
        }
        let p = json!({"metadata":{"namespace":"apps"},"spec":{"dnsPolicy":"ClusterFirst","dnsConfig":{"nameservers":["1.1.1.1","8.8.8.8","9.9.9.9"]}}});
        assert!(resolve(&p, Some(&cluster()), "").is_err());
        let p = json!({"metadata":{"namespace":"apps"},"spec":{"dnsPolicy":"ClusterFirst","dnsConfig":{"searches":(0..30).map(|n|format!("d{n}.example")).collect::<Vec<_>>()}}});
        assert!(resolve(&p, Some(&cluster()), "").is_err());
        assert!(host(&"x".repeat(65537)).is_err());
    }
    #[test]
    fn cluster_configuration_rejects_unsafe_addresses_and_domains() {
        for ip in [
            "0.0.0.0",
            "127.0.0.1",
            "169.254.1.1",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            assert!(ClusterDns::new(ip.parse().unwrap(), "cluster.local".into()).is_err());
        }
        for domain in [
            "",
            "cluster.local.",
            "Cluster.local",
            "a..b",
            "-a.b",
            "a.b-",
            "a\nsearch evil",
        ] {
            assert!(ClusterDns::new("10.43.0.10".parse().unwrap(), domain.into()).is_err());
        }
    }
}
