//! Versioned, read-only allocation snapshot shared by API and native controller.
use ipnet::Ipv4Net;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const NODE_CIDR_CONTROLLER_ID: &str = "system:h3s:node-cidr-controller";
pub const NODE_CIDR_PATH: &str = "/v1-h3s/network/node-cidrs";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCidrAllocations {
    pub version: u8,
    pub cluster_cidr: String,
    pub node_prefix: u8,
    /// Stable node names own reservations, including after Node deletion.
    pub reservations: BTreeMap<String, String>,
}
impl NodeCidrAllocations {
    pub fn new(cluster_cidr: &str, node_prefix: u8) -> Result<Self, &'static str> {
        let result = Self {
            version: 1,
            cluster_cidr: cluster_cidr.into(),
            node_prefix,
            reservations: BTreeMap::new(),
        };
        result.validate()?;
        Ok(result)
    }
    pub fn pool(&self) -> Result<Ipv4Net, &'static str> {
        let pool = self
            .cluster_cidr
            .parse::<Ipv4Net>()
            .map_err(|_| "cluster CIDR must be IPv4")?;
        let services: Ipv4Net = "10.43.0.0/16".parse().unwrap();
        if self.version != 1
            || pool.to_string() != self.cluster_cidr
            || pool.addr() != pool.network()
            || !pool.network().is_private()
            || !pool.broadcast().is_private()
            || self.node_prefix < pool.prefix_len()
            || self.node_prefix > 30
            || self.node_prefix - pool.prefix_len() > 12
            || pool.contains(&services.network())
            || services.contains(&pool.network())
        {
            return Err("require a canonical private IPv4 pool, at most 4096 node subnets through /30, disjoint from Service CIDR 10.43.0.0/16");
        }
        Ok(pool)
    }
    pub fn subnet(&self, cidr: &str) -> Result<Ipv4Net, &'static str> {
        let subnet = cidr
            .parse::<Ipv4Net>()
            .map_err(|_| "invalid IPv4 node CIDR")?;
        if subnet.prefix_len() != self.node_prefix
            || subnet.addr() != subnet.network()
            || subnet.to_string() != cidr
            || !self.pool()?.contains(&subnet.network())
        {
            return Err("node CIDR must be a canonical subnet of the configured pool with its configured prefix");
        }
        Ok(subnet)
    }
    pub fn validate(&self) -> Result<(), &'static str> {
        let pool = self.pool()?;
        if self.reservations.len() > 1usize << (self.node_prefix - pool.prefix_len()) {
            return Err("too many node CIDR reservations");
        }
        let mut used = BTreeSet::new();
        for (name, cidr) in &self.reservations {
            if !crate::valid_node_name(name) || !used.insert(self.subnet(cidr)?) {
                return Err("invalid node name or duplicate node CIDR reservation");
            }
        }
        Ok(())
    }
    pub fn reserve(&mut self, name: &str, cidr: &str) -> Result<bool, &'static str> {
        if !crate::valid_node_name(name) {
            return Err("invalid node name");
        }
        self.subnet(cidr)?;
        if let Some(old) = self.reservations.get(name) {
            return if old == cidr {
                Ok(false)
            } else {
                Err("node CIDR reservation is immutable")
            };
        }
        if self.reservations.values().any(|used| used == cidr) {
            return Err("node CIDR is reserved by another node");
        }
        self.reservations.insert(name.into(), cidr.into());
        Ok(true)
    }
    pub fn next(&self, name: &str) -> Result<String, &'static str> {
        self.validate()?;
        if let Some(cidr) = self.reservations.get(name) {
            return Ok(cidr.clone());
        }
        let used: BTreeSet<_> = self.reservations.values().map(String::as_str).collect();
        self.pool()?
            .subnets(self.node_prefix)
            .map_err(|_| "invalid node prefix")?
            .map(|net| net.to_string())
            .find(|cidr| !used.contains(cidr.as_str()))
            .ok_or("node CIDR pool exhausted; retained reservations require operator investigation")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pool_validation_exhaustion_and_stable_claims() {
        for (pool, prefix) in [
            ("10.42.0.1/16", 24),
            ("10.43.0.0/16", 24),
            ("10.0.0.0/8", 24),
            ("127.0.0.0/16", 24),
            ("2001:db8::/64", 24),
            ("10.42.0.0/24", 23),
            ("10.42.0.0/24", 31),
        ] {
            assert!(
                NodeCidrAllocations::new(pool, prefix).is_err(),
                "{pool}/{prefix}"
            );
        }
        let mut pool = NodeCidrAllocations::new("10.42.0.0/23", 24).unwrap();
        assert_eq!(pool.next("a").unwrap(), "10.42.0.0/24");
        assert!(pool.reserve("a", "10.42.0.0/24").unwrap());
        assert!(!pool.reserve("a", "10.42.0.0/24").unwrap());
        assert!(pool.reserve("b", "10.42.0.0/24").is_err());
        assert!(pool.reserve("a", "10.42.1.0/24").is_err());
        assert!(pool.reserve("b", "10.42.0.1/24").is_err());
        assert!(pool.reserve("b", "10.42.2.0/24").is_err());
        assert_eq!(pool.next("b").unwrap(), "10.42.1.0/24");
        pool.reserve("b", "10.42.1.0/24").unwrap();
        assert!(pool.next("c").is_err());
        assert_eq!(pool.next("a").unwrap(), "10.42.0.0/24");
    }
}
