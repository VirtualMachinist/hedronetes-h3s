//! Print a representative generated ruleset for real nft --check verification.
use serde_json::json;
fn main() {
    let service = json!({"metadata":{"namespace":"fixture","name":"web","uid":"fixture-service"},"spec":{"clusterIP":"10.43.0.10","ports":[{"name":"http","port":80,"protocol":"TCP"},{"name":"dns","port":53,"protocol":"UDP"}]}});
    let slice = json!({"metadata":{"namespace":"fixture","labels":{"kubernetes.io/service-name":"web"},"ownerReferences":[{"kind":"Service","name":"web","uid":"fixture-service"}]},"addressType":"IPv4","ports":[{"name":"http","port":8080,"protocol":"TCP"},{"name":"dns","port":1053,"protocol":"UDP"}],"endpoints":[{"addresses":["10.42.0.2"],"nodeName":"server"},{"addresses":["10.42.2.2"],"nodeName":"worker"}]});
    let nodes = [
        json!({"metadata":{"name":"server"},"spec":{"podCIDR":"10.42.0.0/24"}}),
        json!({"metadata":{"name":"worker"},"spec":{"podCIDR":"10.42.2.0/24"}}),
    ];
    println!(
        "{}",
        h3s_proxy::plan(&[service], &[slice], &nodes, "server")
            .unwrap()
            .render(&"a".repeat(64))
            .unwrap()
    );
}
