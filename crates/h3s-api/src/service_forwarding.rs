//! Typed Service forwarding policy shared by API admission and the native proxy.
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceForwarding {
    /// ClusterIP with a virtual IP; the native proxy programs nftables for this.
    ClusterIp,
    /// ExternalName is admitted but has no dataplane rules.
    ExternalName,
    /// Headless services (`clusterIP: None`) are skipped by the proxy.
    Headless,
    /// NodePort, LoadBalancer, and other types the M1 API does not implement.
    UnsupportedType,
    /// ClusterIP services whose session or traffic policy the proxy does not implement.
    UnsupportedPolicy,
}

impl ServiceForwarding {
    /// Classify a Service spec after API defaults are applied.
    pub fn classify(spec: &Value) -> Self {
        match spec["type"].as_str().unwrap_or("ClusterIP") {
            "ExternalName" => return Self::ExternalName,
            "ClusterIP" if spec["clusterIP"].as_str() == Some("None") => return Self::Headless,
            "ClusterIP" => {}
            _ => return Self::UnsupportedType,
        }
        if spec["sessionAffinity"]
            .as_str()
            .is_some_and(|s| s != "None")
            || spec["externalIPs"]
                .as_array()
                .is_some_and(|values| !values.is_empty())
            || !spec["trafficDistribution"].is_null()
        {
            return Self::UnsupportedPolicy;
        }
        Self::ClusterIp
    }

    pub fn proxied(self) -> bool {
        matches!(self, Self::ClusterIp)
    }

    pub fn skip_in_plan(self) -> bool {
        !self.proxied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cluster_ip_is_proxied_and_unsupported_variants_skip() {
        assert_eq!(
            ServiceForwarding::classify(&json!({"ports":[{"port":80}]})),
            ServiceForwarding::ClusterIp
        );
        assert!(ServiceForwarding::ExternalName.skip_in_plan());
        assert!(ServiceForwarding::Headless.skip_in_plan());
        assert!(ServiceForwarding::UnsupportedType.skip_in_plan());
        assert!(ServiceForwarding::UnsupportedPolicy.skip_in_plan());
        assert_eq!(
            ServiceForwarding::classify(
                &json!({"type":"ExternalName","externalName":"example.invalid"})
            ),
            ServiceForwarding::ExternalName
        );
        assert_eq!(
            ServiceForwarding::classify(
                &json!({"type":"ClusterIP","clusterIP":"None","ports":[{"port":80}]})
            ),
            ServiceForwarding::Headless
        );
        assert_eq!(
            ServiceForwarding::classify(&json!({"type":"NodePort","ports":[{"port":80}]})),
            ServiceForwarding::UnsupportedType
        );
        assert_eq!(
            ServiceForwarding::classify(
                &json!({"sessionAffinity":"ClientIP","ports":[{"port":80}]})
            ),
            ServiceForwarding::UnsupportedPolicy
        );
    }
}
