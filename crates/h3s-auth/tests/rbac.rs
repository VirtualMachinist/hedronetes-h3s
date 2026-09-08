use h3s_auth::{rule_allows, Rbac, Request, ResourceRequest, User};
use serde_json::json;
fn user() -> User {
    User {
        name: "alice".into(),
        groups: vec!["developers".into(), "system:authenticated".into()],
    }
}
fn req<'a>(verb: &'a str, ns: Option<&'static str>) -> Request<'a> {
    Request::Resource(ResourceRequest {
        verb,
        namespace: ns,
        group: "",
        resource: "pods",
        subresource: None,
        name: Some("web"),
    })
}
fn scoped() -> Rbac {
    Rbac{
        roles:vec![serde_json::from_value(json!({"metadata":{"name":"reader","namespace":"team-a"},"rules":[{"verbs":["get","list","watch"],"apiGroups":[""],"resources":["pods"]}]})).unwrap()],
        role_bindings:vec![serde_json::from_value(json!({"metadata":{"name":"readers","namespace":"team-a"},"subjects":[{"kind":"Group","apiGroup":"rbac.authorization.k8s.io","name":"developers"}],"roleRef":{"kind":"Role","apiGroup":"rbac.authorization.k8s.io","name":"reader"}})).unwrap()],
        ..Default::default()
    }
}
#[test]
fn namespace_group_and_verb_boundaries_deny_by_default() {
    let r = scoped();
    let u = user();
    for verb in ["get", "list", "watch"] {
        assert!(r.allows(&u, &req(verb, Some("team-a"))));
    }
    assert!(!r.allows(&u, &req("create", Some("team-a"))));
    assert!(!r.allows(&u, &req("get", Some("team-b"))));
    assert!(!r.allows(&u, &req("list", None)));
    assert!(!r.allows(
        &User {
            name: "mallory".into(),
            groups: vec![]
        },
        &req("get", Some("team-a"))
    ));
    assert!(!r.allows(
        &u,
        &Request::NonResource {
            verb: "get",
            path: "/metrics"
        }
    ));
}
#[test]
fn cluster_role_binding_and_namespaced_cluster_role_reference() {
    let mut r = scoped();
    r.cluster_roles.push(serde_json::from_value(json!({"metadata":{"name":"reader"},"rules":[{"verbs":["get"],"apiGroups":[""],"resources":["pods"]}]})).unwrap());
    r.role_bindings[0].role_ref.kind = "ClusterRole".into();
    assert!(r.allows(&user(), &req("get", Some("team-a"))));
    assert!(!r.allows(&user(), &req("get", Some("team-b"))));
    r.cluster_role_bindings.push(serde_json::from_value(json!({"metadata":{"name":"everywhere"},"subjects":[{"kind":"User","apiGroup":"rbac.authorization.k8s.io","name":"alice"}],"roleRef":{"kind":"ClusterRole","apiGroup":"rbac.authorization.k8s.io","name":"reader"}})).unwrap());
    assert!(r.allows(&user(), &req("get", Some("team-b"))));
    r.cluster_role_bindings[0].role_ref.api_group = "example.invalid".into();
    assert!(!r.allows(&user(), &req("get", Some("team-b"))));
    r.role_bindings[0].role_ref.name = "missing".into();
    assert!(!r.allows(&user(), &req("get", Some("team-a"))));
}
#[test]
fn resource_names_do_not_grant_collection_actions_or_wildcard_names() {
    let rule = serde_json::from_value(
        json!({"verbs":["*"],"apiGroups":["*"],"resources":["pods"],"resourceNames":["web"]}),
    )
    .unwrap();
    assert!(rule_allows(&rule, &req("get", None)));
    let mut q = match req("list", None) {
        Request::Resource(r) => r,
        _ => unreachable!(),
    };
    q.name = None;
    assert!(!rule_allows(&rule, &Request::Resource(q.clone())));
    q.name = Some("other");
    assert!(!rule_allows(&rule, &Request::Resource(q)));
    let rule = serde_json::from_value(
        json!({"verbs":["*"],"apiGroups":["*"],"resources":["pods"],"resourceNames":["*"]}),
    )
    .unwrap();
    assert!(!rule_allows(&rule, &req("get", None)));
}
#[test]
fn subresource_wildcards_match_upstream_v134() {
    let mut q = match req("get", None) {
        Request::Resource(r) => r,
        _ => unreachable!(),
    };
    q.subresource = Some("log");
    for (resources, expected) in [
        ("pods", false),
        ("pods/log", true),
        ("*/log", true),
        ("pods/*", false),
        ("*", true),
        ("*/status", false),
    ] {
        let rule = serde_json::from_value(
            json!({"verbs":["get"],"apiGroups":[""],"resources":[resources]}),
        )
        .unwrap();
        assert_eq!(
            rule_allows(&rule, &Request::Resource(q.clone())),
            expected,
            "{resources}"
        );
    }
}
#[test]
fn service_account_subjects_are_scoped_and_api_groups_cannot_be_spoofed() {
    let mut r = scoped();
    r.role_bindings[0].subjects = Some(vec![serde_json::from_value(
        json!({"kind":"ServiceAccount","name":"worker"}),
    )
    .unwrap()]);
    let sa = User {
        name: "system:serviceaccount:team-a:worker".into(),
        groups: vec![],
    };
    assert!(r.allows(&sa, &req("get", Some("team-a"))));
    assert!(!r.allows(
        &User {
            name: "system:serviceaccount:team-b:worker".into(),
            groups: vec![]
        },
        &req("get", Some("team-a"))
    ));
    r.role_bindings[0].subjects.as_mut().unwrap()[0].api_group =
        Some("rbac.authorization.k8s.io".into());
    assert!(!r.allows(&sa, &req("get", Some("team-a"))));
}
#[test]
fn nonresource_urls_have_only_trailing_prefix_wildcards() {
    let rule =
        serde_json::from_value(json!({"verbs":["get"],"nonResourceURLs":["/apis/*","/version"]}))
            .unwrap();
    for (path, expected) in [
        ("/apis/apps", true),
        ("/version", true),
        ("/apis", false),
        ("/versions", false),
        ("/metrics", false),
    ] {
        assert_eq!(
            rule_allows(&rule, &Request::NonResource { verb: "get", path }),
            expected
        );
    }
    assert!(!rule_allows(
        &rule,
        &Request::NonResource {
            verb: "post",
            path: "/version"
        }
    ));
    assert!(!rule_allows(&rule, &req("get", None)));
}
#[test]
fn x509_cn_and_groups_drive_admin_and_node_identity() {
    let temp = tempfile::tempdir().unwrap();
    let pki =
        h3s_certs::ClusterPki::open_or_create(&temp.path().join("pki"), &["localhost".into()])
            .unwrap();
    let admin =
        User::from_verified_certificate(pki.admin().certificate_der().unwrap().as_ref()).unwrap();
    assert_eq!(admin.name, "h3s-admin");
    assert!(admin.is_superuser());
    assert!(Rbac::default().allows(&admin, &req("delete", None)));
    let node = pki
        .issue_client("system:node:worker", Some("system:nodes"))
        .unwrap();
    let node = User::from_verified_certificate(node.certificate_der().unwrap().as_ref()).unwrap();
    assert_eq!(node.node_name(), Some("worker"));
    assert!(!node.is_superuser());
    assert!(!Rbac::default().allows(&node, &req("delete", None)));
    assert!(node.groups.iter().any(|g| g == "system:authenticated"));
    assert!(User::from_verified_certificate(b"not a certificate").is_err());
}
