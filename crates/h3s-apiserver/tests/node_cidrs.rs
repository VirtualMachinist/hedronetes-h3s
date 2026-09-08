mod common;
use common::Server;
use h3s_api::network::{NODE_CIDR_CONTROLLER_ID, NODE_CIDR_PATH};
use serde_json::{json, Value};

fn node(name: &str) -> Value {
    json!({"apiVersion":"v1","kind":"Node","metadata":{"name":name}})
}
async fn controller(s: &Server) -> kube::Client {
    let identity = s.pki.issue_client(NODE_CIDR_CONTROLLER_ID, None).unwrap();
    h3s_controllers::client_from_kubeconfig(&s.pki.kubeconfig(&s.endpoint(), &identity).unwrap())
        .await
        .unwrap()
}
async fn get(s: &Server, name: &str) -> Value {
    let (code, value) = s
        .json(
            s.admin(),
            "GET",
            &format!("/api/v1/nodes/{name}"),
            json!({}),
        )
        .await;
    assert_eq!(code, 200, "{value}");
    value
}
async fn ledger(s: &Server) -> Value {
    let (code, value) = s.json(s.admin(), "GET", NODE_CIDR_PATH, json!({})).await;
    assert_eq!(code, 200, "{value}");
    value
}
async fn assign(s: &Server, name: &str, cidr: &str) -> (u16, Value) {
    let mut value = get(s, name).await;
    value["spec"] = json!({"podCIDR":cidr,"podCIDRs":[cidr]});
    s.json(s.admin(), "PUT", &format!("/api/v1/nodes/{name}"), value)
        .await
}

#[tokio::test]
async fn allocations_survive_restart_deletion_and_exhaustion() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start_with_node_cidrs(dir.path(), "10.42.0.0/23", 24).await;
    for name in ["a", "b"] {
        assert_eq!(
            s.json(s.admin(), "POST", "/api/v1/nodes", node(name))
                .await
                .0,
            201
        );
    }
    h3s_controllers::node_cidrs_once(controller(&s).await)
        .await
        .unwrap();
    assert_eq!(
        get(&s, "a").await["spec"]["podCIDRs"],
        json!(["10.42.0.0/24"])
    );
    assert_eq!(
        get(&s, "b").await["spec"]["podCIDRs"],
        json!(["10.42.1.0/24"])
    );
    let before = ledger(&s).await;
    assert_eq!(
        s.json(s.admin(), "DELETE", "/api/v1/nodes/a", json!({}))
            .await
            .0,
        200
    );
    let s = s.restart(dir.path()).await;
    assert_eq!(ledger(&s).await, before);
    assert_eq!(
        s.json(s.admin(), "POST", "/api/v1/nodes", node("c"))
            .await
            .0,
        201
    );
    assert!(h3s_controllers::node_cidrs_once(controller(&s).await)
        .await
        .is_err());
    assert!(get(&s, "c").await["spec"]["podCIDR"].is_null());
    assert_eq!(
        s.json(s.admin(), "POST", "/api/v1/nodes", node("a"))
            .await
            .0,
        201
    );
    // The recreated name retains its network; exhaustion on c cannot recycle it.
    assert!(h3s_controllers::node_cidrs_once(controller(&s).await)
        .await
        .is_err());
    assert_eq!(get(&s, "a").await["spec"]["podCIDR"], "10.42.0.0/24");
    assert_eq!(ledger(&s).await, before);
}

#[tokio::test]
async fn competing_assignments_and_invalid_writes_do_not_leak_claims() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start_with_node_cidrs(dir.path(), "10.42.0.0/16", 24).await;
    for name in ["a", "b"] {
        assert_eq!(
            s.json(s.admin(), "POST", "/api/v1/nodes", node(name))
                .await
                .0,
            201
        );
    }
    let (a, b) = tokio::join!(
        assign(&s, "a", "10.42.8.0/24"),
        assign(&s, "b", "10.42.8.0/24")
    );
    assert!(
        (a.0 == 200 && b.0 == 422) || (a.0 == 422 && b.0 == 200),
        "{a:?} {b:?}"
    );
    let (winner, loser) = if a.0 == 200 { ("a", "b") } else { ("b", "a") };
    let before = ledger(&s).await;
    assert_eq!(before["reservations"].as_object().unwrap().len(), 1);
    for cidr in ["10.42.7.0/24", "10.42.8.0/25", "10.43.1.0/24", ""] {
        assert_eq!(assign(&s, winner, cidr).await.0, 422);
    }
    let mut stale = get(&s, loser).await;
    assert_eq!(
        s.patch(
            s.admin(),
            &format!("/api/v1/nodes/{loser}"),
            "application/merge-patch+json",
            json!({"metadata":{"annotations":{"revision":"changed"}}})
        )
        .await
        .0,
        200
    );
    stale["spec"] = json!({"podCIDR":"10.42.9.0/24","podCIDRs":["10.42.9.0/24"]});
    assert_eq!(
        s.json(s.admin(), "PUT", &format!("/api/v1/nodes/{loser}"), stale)
            .await
            .0,
        409
    );
    let mut duplicate = node(loser);
    duplicate["spec"] = json!({"podCIDR":"10.42.9.0/24","podCIDRs":["10.42.9.0/24"]});
    assert_eq!(
        s.json(s.admin(), "POST", "/api/v1/nodes", duplicate)
            .await
            .0,
        409
    );
    for spec in [
        json!({"podCIDRs":["10.42.9.0/24"]}),
        json!({"podCIDR":"10.42.9.0/24","podCIDRs":["10.42.10.0/24"]}),
        json!({"podCIDR":"10.42.9.1/24"}),
        json!({"podCIDR":"10.42.9.0/25"}),
        json!({"podCIDR":"10.43.9.0/24"}),
    ] {
        let mut value = get(&s, loser).await;
        value["spec"] = spec;
        assert_eq!(
            s.json(s.admin(), "PUT", &format!("/api/v1/nodes/{loser}"), value)
                .await
                .0,
            422
        );
    }
    assert_eq!(ledger(&s).await, before);
    let (one, two) = tokio::join!(
        h3s_controllers::node_cidrs_once(controller(&s).await),
        h3s_controllers::node_cidrs_once(controller(&s).await)
    );
    assert!(one.is_ok() || two.is_ok());
    h3s_controllers::node_cidrs_once(controller(&s).await)
        .await
        .unwrap();
    assert_ne!(
        get(&s, winner).await["spec"]["podCIDR"],
        get(&s, loser).await["spec"]["podCIDR"]
    );
}

#[tokio::test]
async fn topology_reads_do_not_grant_network_assignment_or_foreign_writes() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start_with_node_cidrs(dir.path(), "10.42.0.0/16", 24).await;
    let identity = s
        .pki
        .issue_client("system:node:a", Some("system:nodes"))
        .unwrap();
    let worker = || s.pki.client_config(Some(&identity)).unwrap();
    for name in ["a", "b"] {
        assert_eq!(
            s.json(s.admin(), "POST", "/api/v1/nodes", node(name))
                .await
                .0,
            201
        );
    }
    assert_eq!(
        s.json(worker(), "GET", "/api/v1/nodes", json!({})).await.0,
        200
    );
    assert_eq!(
        s.json(worker(), "GET", "/api/v1/nodes/b", json!({}))
            .await
            .0,
        200
    );
    let response = s
        .raw(
            worker(),
            "GET",
            "/api/v1/nodes?watch=true&timeoutSeconds=1",
            json!({}),
            &[],
        )
        .await;
    assert_eq!(response.status(), 200);
    drop(response);
    for path in [NODE_CIDR_PATH, "/api/v1/namespaces/default/secrets"] {
        assert_eq!(s.json(worker(), "GET", path, json!({})).await.0, 403);
    }
    assert_eq!(
        s.patch(
            worker(),
            "/api/v1/nodes/a",
            "application/merge-patch+json",
            json!({"spec":{"podCIDR":"10.42.0.0/24","podCIDRs":["10.42.0.0/24"]}})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        s.patch(
            worker(),
            "/api/v1/nodes/b/status",
            "application/strategic-merge-patch+json",
            json!({"metadata":{"annotations":{"flannel.alpha.coreos.com/backend-data":"{}"}}})
        )
        .await
        .0,
        403
    );
    h3s_controllers::node_cidrs_once(controller(&s).await)
        .await
        .unwrap();
    let before = ledger(&s).await;
    // Flannel's own-node status annotation shape is permitted; spec resets.
    let (code,value)=s.patch(worker(),"/api/v1/nodes/a/status","application/strategic-merge-patch+json",json!({"metadata":{"annotations":{"flannel.alpha.coreos.com/backend-data":"{}"}},"spec":{"podCIDR":"10.42.99.0/24"}})).await;
    assert_eq!(code, 200, "{value}");
    assert_eq!(value["spec"]["podCIDR"], "10.42.0.0/24");
    assert_eq!(
        value["metadata"]["annotations"]["flannel.alpha.coreos.com/backend-data"],
        "{}"
    );
    assert_eq!(ledger(&s).await, before);
    assert_eq!(
        s.json(s.admin(), "POST", NODE_CIDR_PATH, json!({})).await.0,
        405
    );
    assert_eq!(
        s.json(
            s.admin(),
            "GET",
            &format!("{NODE_CIDR_PATH}?unexpected=true"),
            json!({})
        )
        .await
        .0,
        400
    );
    assert_eq!(
        s.json(
            s.admin(),
            "GET",
            &format!("{NODE_CIDR_PATH}?timeout=10s"),
            json!({})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        s.json(
            worker(),
            "GET",
            &format!("{NODE_CIDR_PATH}?timeout=10s"),
            json!({})
        )
        .await
        .0,
        403
    );
    let id = s.pki.issue_client(NODE_CIDR_CONTROLLER_ID, None).unwrap();
    let config = || s.pki.client_config(Some(&id)).unwrap();
    assert_eq!(
        s.json(config(), "GET", NODE_CIDR_PATH, json!({})).await.0,
        200
    );
    assert_eq!(
        s.json(
            config(),
            "GET",
            "/api/v1/namespaces/default/secrets",
            json!({})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        s.json(config(), "DELETE", "/api/v1/nodes/a", json!({}))
            .await
            .0,
        403
    );
    assert_eq!(
        s.patch(
            config(),
            "/api/v1/nodes/a/status",
            "application/merge-patch+json",
            json!({"status":{}})
        )
        .await
        .0,
        403
    );
}
