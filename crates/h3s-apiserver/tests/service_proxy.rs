mod common;
use common::Server;
use serde_json::json;

#[tokio::test]
async fn node_discovery_is_read_only_and_survives_api_restart() {
    let dir = tempfile::tempdir().unwrap();
    let mut s = Server::start_with_node_cidrs(dir.path(), "10.42.0.0/16", 24).await;
    let identity = s
        .pki
        .issue_client("system:node:worker", Some("system:nodes"))
        .unwrap();
    let service = json!({"apiVersion":"v1","kind":"Service","metadata":{"name":"web"},"spec":{"ports":[{"port":80,"protocol":"TCP"},{"name":"dns","port":53,"protocol":"UDP"}]}});
    let mut service = service;
    service["spec"]["ports"][0]["name"] = json!("http");
    let (code, service) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/default/services",
            service,
        )
        .await;
    assert_eq!(code, 201, "{service}");
    let slice = json!({"apiVersion":"discovery.k8s.io/v1","kind":"EndpointSlice","metadata":{"name":"web","labels":{"kubernetes.io/service-name":"web"}},"addressType":"IPv4","ports":[{"name":"http","port":8080}],"endpoints":[{"addresses":["10.42.2.2"]}]});
    let (code, slice) = s
        .json(
            s.admin(),
            "POST",
            "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices",
            slice,
        )
        .await;
    assert_eq!(code, 201, "{slice}");
    for pass in 0..2 {
        for (collection, named, obj) in [
            (
                "/api/v1/services",
                "/api/v1/namespaces/default/services/web",
                &service,
            ),
            (
                "/apis/discovery.k8s.io/v1/endpointslices",
                "/apis/discovery.k8s.io/v1/namespaces/default/endpointslices/web",
                &slice,
            ),
        ] {
            let tls = || s.pki.client_config(Some(&identity)).unwrap();
            let (code, list) = s.json(tls(), "GET", collection, json!({})).await;
            assert_eq!(code, 200);
            assert_eq!(list["items"].as_array().unwrap().len(), 1);
            assert_eq!(s.json(tls(), "GET", named, json!({})).await.0, 200);
            let watch = s
                .raw(
                    tls(),
                    "GET",
                    &format!("{collection}?watch=true&timeoutSeconds=1"),
                    json!({}),
                    &[],
                )
                .await;
            assert_eq!(watch.status(), 200);
            drop(watch);
            for verb in ["PUT", "DELETE"] {
                assert_eq!(s.json(tls(), verb, named, obj.clone()).await.0, 403);
            }
            let create_path = named.rsplit_once('/').unwrap().0;
            assert_eq!(s.json(tls(), "POST", create_path, obj.clone()).await.0, 403);
            assert_eq!(
                s.patch(
                    tls(),
                    named,
                    "application/merge-patch+json",
                    json!({"metadata":{"labels":{"attack":"true"}}})
                )
                .await
                .0,
                403
            );
        }
        if pass == 0 {
            assert_eq!(
                s.json(
                    s.pki.client_config(Some(&identity)).unwrap(),
                    "GET",
                    "/api/v1/secrets",
                    json!({})
                )
                .await
                .0,
                403
            );
            s = s.restart(dir.path()).await;
        }
    }
}
#[tokio::test]
async fn unsupported_forwarding_policies_are_rejected_instead_of_silently_ignored() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    for (name, extra) in [
        ("affinity", json!({"sessionAffinity":"ClientIP"})),
        ("external", json!({"externalIPs":["192.0.2.4"]})),
        ("distribution", json!({"trafficDistribution":"PreferClose"})),
        ("sctp", json!({"ports":[{"port":80,"protocol":"SCTP"}]})),
    ] {
        let mut service = json!({"apiVersion":"v1","kind":"Service","metadata":{"name":name},"spec":{"ports":[{"port":80}]}});
        service["spec"]
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let (code, result) = s
            .json(
                s.admin(),
                "POST",
                "/api/v1/namespaces/default/services",
                service,
            )
            .await;
        assert_eq!(code, 422, "{result}");
    }
}
