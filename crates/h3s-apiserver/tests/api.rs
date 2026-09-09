mod common;
use common::Server;
use h3s_storage::SqliteStore;
use http_body_util::BodyExt;
use serde_json::{json, Value};

#[tokio::test]
async fn tls_auth_and_namespace_rbac_precede_resource_access() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let path = "/api/v1/namespaces/team-a/configmaps";
    let (status, v) = s
        .json(s.pki.client_config(None).unwrap(), "GET", path, json!({}))
        .await;
    assert_eq!(status, 401);
    assert_eq!(v["kind"], "Status");
    let response = s
        .raw(
            s.pki.client_config(None).unwrap(),
            "GET",
            path,
            json!({}),
            &[("X-Remote-User", "h3s-admin")],
        )
        .await;
    assert_eq!(response.status(), 401);
    let alice = s.pki.issue_client("alice", Some("developers")).unwrap();
    let client = || s.pki.client_config(Some(&alice)).unwrap();
    assert_eq!(s.json(client(), "GET", "/api", json!({})).await.0, 200);
    assert_eq!(s.json(client(), "GET", path, json!({})).await.0, 403);
    let role = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{"name":"reader"},"rules":[{"verbs":["get","list","watch"],"apiGroups":[""],"resources":["configmaps"]}]});
    assert_eq!(
        s.json(
            s.admin(),
            "POST",
            "/apis/rbac.authorization.k8s.io/v1/namespaces/team-a/roles",
            role
        )
        .await
        .0,
        201
    );
    let binding = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding","metadata":{"name":"reader"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":"reader"},"subjects":[{"kind":"Group","apiGroup":"rbac.authorization.k8s.io","name":"developers"}]});
    assert_eq!(
        s.json(
            s.admin(),
            "POST",
            "/apis/rbac.authorization.k8s.io/v1/namespaces/team-a/rolebindings",
            binding
        )
        .await
        .0,
        201
    );
    assert_eq!(s.json(client(), "GET", path, json!({})).await.0, 200);
    assert_eq!(s.json(client(), "POST", path, json!({})).await.0, 403);
    assert_eq!(
        s.json(
            client(),
            "GET",
            "/api/v1/namespaces/default/configmaps",
            json!({})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        s.raw(
            client(),
            "GET",
            path,
            json!({}),
            &[("Impersonate-User", "h3s-admin")]
        )
        .await
        .status(),
        403
    );
    assert_eq!(
        s.raw(
            s.admin(),
            "GET",
            path,
            json!({}),
            &[("Authorization", "Bearer invalid")]
        )
        .await
        .status(),
        401
    );
}
#[tokio::test]
async fn durable_crud_conflicts_delete_preconditions_and_restart() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let created = s.configmap("settings", "one").await;
    let path = "/api/v1/namespaces/team-a/configmaps/settings";
    let mut changed = created.clone();
    changed["data"]["value"] = "two".into();
    let (code, updated) = s.json(s.admin(), "PUT", path, changed).await;
    assert_eq!(code, 200);
    assert_eq!(updated["metadata"]["uid"], created["metadata"]["uid"]);
    assert_ne!(
        updated["metadata"]["resourceVersion"],
        created["metadata"]["resourceVersion"]
    );
    assert_eq!(s.json(s.admin(), "PUT", path, created).await.0, 409);
    let ca = s.pki.ca_pem().to_owned();
    drop(s);
    tokio::task::yield_now().await;
    let s = Server::start(dir.path()).await;
    assert_eq!(ca, s.pki.ca_pem());
    let (code, reopened) = s.json(s.admin(), "GET", path, json!({})).await;
    assert_eq!(code, 200);
    assert_eq!(reopened, updated);
    assert_eq!(
        s.json(
            s.admin(),
            "DELETE",
            path,
            json!({"preconditions":{"uid":"wrong"}})
        )
        .await
        .0,
        409
    );
    assert_eq!(
        s.json(
            s.admin(),
            "DELETE",
            path,
            json!({"preconditions":{"uid":reopened["metadata"]["uid"]}})
        )
        .await
        .0,
        200
    );
    assert_eq!(s.json(s.admin(), "GET", path, json!({})).await.0, 404);
}

#[tokio::test]
async fn helm_release_put_without_resource_version_updates_latest() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;

    let created = s.configmap("release", "pending-install").await;
    let path = "/api/v1/namespaces/team-a/configmaps/release";
    let mut helm_update = json!({
        "apiVersion":"v1",
        "kind":"ConfigMap",
        "metadata":{
            "name":"release",
            "namespace":"team-a",
            "labels":{"owner":"helm","status":"deployed","name":"release","version":"1"}
        },
        "data":{"value":"deployed","release":"blob"}
    });
    let (code, updated) = s.json(s.admin(), "PUT", path, helm_update.clone()).await;
    assert_eq!(code, 200, "{updated}");
    assert_eq!(updated["metadata"]["uid"], created["metadata"]["uid"]);
    assert_eq!(updated["metadata"]["labels"]["status"], "deployed");
    assert_eq!(updated["data"]["value"], "deployed");
    assert_ne!(
        updated["metadata"]["resourceVersion"],
        created["metadata"]["resourceVersion"]
    );
    helm_update["data"]["value"] = "upgraded".into();
    helm_update["metadata"]["labels"]["version"] = "2".into();
    let (code, upgraded) = s.json(s.admin(), "PUT", path, helm_update).await;
    assert_eq!(code, 200, "{upgraded}");
    assert_eq!(upgraded["data"]["value"], "upgraded");
    assert_eq!(upgraded["metadata"]["labels"]["version"], "2");
    assert_eq!(s.json(s.admin(), "PUT", path, created).await.0, 409);

    let secret_name = "sh.helm.release.v1.release.v1";
    let secret_path = format!("/api/v1/namespaces/team-a/secrets/{secret_name}");
    let (code, created) = s
        .json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/team-a/secrets",
            json!({
                "apiVersion":"v1",
                "kind":"Secret",
                "metadata":{
                    "name":secret_name,
                    "labels":{"owner":"helm","status":"pending-install","name":"release","version":"1"}
                },
                "type":"helm.sh/release.v1",
                "data":{"release":"cGVuZGluZw=="}
            }),
        )
        .await;
    assert_eq!(code, 201, "{created}");
    let helm_update = json!({
        "apiVersion":"v1",
        "kind":"Secret",
        "metadata":{
            "name":secret_name,
            "namespace":"team-a",
            "labels":{"owner":"helm","status":"deployed","name":"release","version":"1"}
        },
        "type":"helm.sh/release.v1",
        "data":{"release":"ZGVwbG95ZWQ="}
    });
    let (code, updated) = s.json(s.admin(), "PUT", &secret_path, helm_update).await;
    assert_eq!(code, 200, "{updated}");
    assert_eq!(updated["metadata"]["uid"], created["metadata"]["uid"]);
    assert_eq!(updated["metadata"]["labels"]["status"], "deployed");
    assert_eq!(updated["type"], "helm.sh/release.v1");
    assert_eq!(updated["data"]["release"], "ZGVwbG95ZWQ=");
    assert_ne!(
        updated["metadata"]["resourceVersion"],
        created["metadata"]["resourceVersion"]
    );

    let account_path = "/api/v1/namespaces/team-a/serviceaccounts/default";
    let account_update = json!({
        "apiVersion":"v1",
        "kind":"ServiceAccount",
        "metadata":{"name":"default","namespace":"team-a"}
    });
    let (code, error) = s.json(s.admin(), "PUT", account_path, account_update).await;
    assert_eq!(code, 400, "{error}");
    assert_eq!(error["message"], "update requires metadata.resourceVersion");
}

#[tokio::test]
async fn paginated_list_keeps_snapshot_and_watch_replays_json_events() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    s.configmap("a", "original").await;
    s.configmap("b", "original").await;
    let path = "/api/v1/namespaces/team-a/configmaps";
    let (status, page) = s
        .json(s.admin(), "GET", &format!("{path}?limit=1"), json!({}))
        .await;
    assert_eq!(status, 200);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let rv = page["metadata"]["resourceVersion"].as_str().unwrap();
    let created = s.configmap("c", "later").await;
    let token = page["metadata"]["continue"].as_str().unwrap();
    let (status, next) = s
        .json(
            s.admin(),
            "GET",
            &format!("{path}?limit=1&continue={token}"),
            json!({}),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(next["metadata"]["resourceVersion"], rv);
    assert_eq!(next["items"][0]["metadata"]["name"], "b");
    assert_eq!(next["metadata"]["continue"], "");
    let response = s
        .raw(
            s.admin(),
            "GET",
            &format!("{path}?watch=true&resourceVersion={rv}&timeoutSeconds=1"),
            json!({}),
            &[],
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let events: Vec<Value> = String::from_utf8(bytes.to_vec())
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "ADDED");
    assert_eq!(events[0]["object"], created);
    let (code, filtered) = s
        .json(
            s.admin(),
            "GET",
            &format!("{path}?labelSelector=app%3Dweb"),
            json!({}),
        )
        .await;
    assert_eq!(code, 200);
    assert_eq!(filtered["items"], json!([]));
}
#[tokio::test]
async fn validation_and_secret_write_only_string_data() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let path = "/api/v1/namespaces/team-a/secrets";
    let secret = json!({"apiVersion":"v1","kind":"Secret","metadata":{"name":"test-secret"},"stringData":{"token":"synthetic-test-value"}});
    let (code, value) = s.json(s.admin(), "POST", path, secret).await;
    assert_eq!(code, 201, "{value}");
    assert!(value.get("stringData").is_none());
    assert_eq!(value["data"]["token"], "c3ludGhldGljLXRlc3QtdmFsdWU=");
    let bad =
        json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bad","namespace":"other"}});
    assert_eq!(
        s.json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/team-a/configmaps",
            bad
        )
        .await
        .0,
        400
    );
    let bad = json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":"bad"},"data":{"invalid":123}});
    assert_eq!(
        s.json(
            s.admin(),
            "POST",
            "/api/v1/namespaces/team-a/configmaps",
            bad
        )
        .await
        .0,
        422
    );
}

#[tokio::test]
async fn filtered_pagination_scans_storage_pages_at_one_snapshot_and_binds_selectors() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let path = "/api/v1/namespaces/team-a/configmaps";
    let mut last = Value::Null;
    // The second match lies beyond the registry's 256-item internal page.
    for i in 0..260 {
        let label = if i == 0 || i == 259 { "web" } else { "other" };
        let (code, object) = s.json(s.admin(), "POST", path,
            json!({"apiVersion":"v1","kind":"ConfigMap","metadata":{"name":format!("item-{i:03}"),"labels":{"app":label}}})).await;
        assert_eq!(code, 201, "{object}");
        last = object;
    }
    let query = format!("{path}?limit=1&labelSelector=app%3Dweb");
    let (code, first) = s.json(s.admin(), "GET", &query, json!({})).await;
    assert_eq!(code, 200);
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    assert_eq!(first["items"][0]["metadata"]["name"], "item-000");
    let token = first["metadata"]["continue"].as_str().unwrap();
    assert!(!token.is_empty());
    let original = last.clone();
    last["metadata"]["labels"]["app"] = "other".into();
    assert_eq!(
        s.json(s.admin(), "PUT", &format!("{path}/item-259"), last)
            .await
            .0,
        200
    );
    let (code, second) = s
        .json(
            s.admin(),
            "GET",
            &format!("{query}&continue={token}"),
            json!({}),
        )
        .await;
    assert_eq!(code, 200);
    assert_eq!(second["items"], json!([original]));
    assert_eq!(
        second["metadata"]["resourceVersion"],
        first["metadata"]["resourceVersion"]
    );
    assert_eq!(second["metadata"]["continue"], "");
    for changed in [
        format!("{path}?labelSelector=app%3Dother&continue={token}"),
        format!("{query}&fieldSelector=metadata.name%3Ditem-000&continue={token}"),
        format!("{query}&resourceVersion=1&continue={token}"),
        format!("{path}?fieldSelector=spec.nodeName%3Dworker"),
        format!("{path}?labelSelector=app%20in%20%28web"),
    ] {
        assert_eq!(
            s.json(s.admin(), "GET", &changed, json!({})).await.0,
            400,
            "{changed}"
        );
    }
}

#[tokio::test]
async fn filtered_watch_replays_entry_change_exit_reentry_and_delete_after_restart() {
    use h3s_storage::Storage;
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let path = "/api/v1/namespaces/team-a/configmaps";
    let named = format!("{path}/settings");
    let mut current = s.configmap("settings", "initial").await;
    let rv = current["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .to_owned();
    let mut expected = Vec::new();
    for (label, data, event_type) in [
        ("web", "entered", Some("ADDED")),
        ("web", "changed", Some("MODIFIED")),
        ("other", "left", Some("DELETED")),
        ("other", "still outside", None),
        ("web", "returned", Some("ADDED")),
    ] {
        let previous = current.clone();
        current["metadata"]["labels"] = json!({"app":label});
        current["data"]["value"] = data.into();
        let (code, updated) = s.json(s.admin(), "PUT", &named, current).await;
        assert_eq!(code, 200, "{updated}");
        current = updated;
        if let Some(typ) = event_type {
            let mut value = if typ == "DELETED" {
                previous
            } else {
                current.clone()
            };
            value["metadata"]["resourceVersion"] = current["metadata"]["resourceVersion"].clone();
            expected.push(json!({"type":typ,"object":value}));
        }
    }
    // Compact to the starting revision and reopen before constructing the watch:
    // exit events must come from durable old values, not an in-memory cache.
    let store = SqliteStore::open(dir.path().join("registry.db"))
        .await
        .unwrap();
    store.compact(rv.parse().unwrap()).await.unwrap();
    drop(store);
    drop(s);
    tokio::task::yield_now().await;
    let s = Server::start(dir.path()).await;
    let response = s
        .raw(
            s.admin(),
            "GET",
            &format!(
                "{path}?watch=true&labelSelector=app%3Dweb&resourceVersion={rv}&timeoutSeconds=1"
            ),
            json!({}),
            &[],
        )
        .await;
    assert_eq!(response.status(), 200);
    // A live event after subscription must follow the replayed events.
    let (code, deletion) = s.json(s.admin(), "DELETE", &named, json!({})).await;
    assert_eq!(code, 200);
    assert_eq!(deletion["status"], "Success");
    let (_, list) = s.json(s.admin(), "GET", path, json!({})).await;
    current["metadata"]["resourceVersion"] = list["metadata"]["resourceVersion"].clone();
    expected.push(json!({"type":"DELETED","object":current}));
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let actual: Vec<Value> = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(actual, expected);
}

#[tokio::test]
async fn resource_names_rbac_requires_exact_field_selection_for_list_and_watch() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let visible = s.configmap("visible", "allowed").await;
    s.configmap("hidden", "must not leak").await;
    let base = "/apis/rbac.authorization.k8s.io/v1/namespaces/team-a";
    let role = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{"name":"named-reader"},"rules":[{"apiGroups":[""],"resources":["configmaps"],"resourceNames":["visible"],"verbs":["list","watch"]}]});
    let binding = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding","metadata":{"name":"named-reader"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":"named-reader"},"subjects":[{"apiGroup":"rbac.authorization.k8s.io","kind":"User","name":"named-reader"}]});
    assert_eq!(
        s.json(s.admin(), "POST", &format!("{base}/roles"), role)
            .await
            .0,
        201
    );
    assert_eq!(
        s.json(s.admin(), "POST", &format!("{base}/rolebindings"), binding)
            .await
            .0,
        201
    );
    let cert = s.pki.issue_client("named-reader", None).unwrap();
    let client = || s.pki.client_config(Some(&cert)).unwrap();
    let path = "/api/v1/namespaces/team-a/configmaps";
    for query in [
        "",
        "?fieldSelector=metadata.name%21%3Dhidden",
        "?fieldSelector=metadata.name%3Dhidden",
        "?labelSelector=metadata.name%3Dvisible",
        "?watch=true",
    ] {
        assert_eq!(
            s.json(client(), "GET", &format!("{path}{query}"), json!({}))
                .await
                .0,
            403
        );
    }
    let selected = format!("{path}?fieldSelector=metadata.name%3Dvisible");
    let (code, list) = s.json(client(), "GET", &selected, json!({})).await;
    assert_eq!(code, 200);
    assert_eq!(list["items"], json!([visible.clone()]));
    for path in [
        format!("{selected}&watch=true&timeoutSeconds=1"),
        format!("{path}/visible?watch=true&timeoutSeconds=1"),
    ] {
        let response = s.raw(client(), "GET", &path, json!({}), &[]).await;
        assert_eq!(response.status(), 200);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let events: Vec<Value> = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events, vec![json!({"type":"ADDED","object":visible})]);
    }
}

#[tokio::test]
async fn patches_preserve_identity_validate_preconditions_and_commit_atomically() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let original = s.configmap("settings", "one").await;
    let path = "/api/v1/namespaces/team-a/configmaps/settings";
    let merge = "application/merge-patch+json";
    let json_patch = "application/json-patch+json";
    let (code, merged) = s
        .patch(
            s.admin(),
            path,
            merge,
            json!({"data":{"value":null,"next":"two"},"metadata":{"labels":{"app":"web"}}}),
        )
        .await;
    assert_eq!(code, 200, "{merged}");
    assert_eq!(merged["data"], json!({"next":"two"}));
    assert_eq!(merged["metadata"]["uid"], original["metadata"]["uid"]);
    assert_eq!(
        merged["metadata"]["creationTimestamp"],
        original["metadata"]["creationTimestamp"]
    );
    assert_ne!(
        merged["metadata"]["resourceVersion"],
        original["metadata"]["resourceVersion"]
    );
    let operations = json!([
        {"op":"test","path":"/metadata/resourceVersion","value":merged["metadata"]["resourceVersion"]},
        {"op":"copy","from":"/data/next","path":"/data/copy"},
        {"op":"move","from":"/data/copy","path":"/data/moved"},
        {"op":"remove","path":"/data/next"},
        {"op":"add","path":"/metadata/annotations","value":{"example.org/key":"old"}},
        {"op":"replace","path":"/metadata/annotations/example.org~1key","value":"new"}
    ]);
    let (code, patched) = s.patch(s.admin(), path, json_patch, operations).await;
    assert_eq!(code, 200, "{patched}");
    assert_eq!(patched["data"], json!({"moved":"two"}));
    assert_eq!(patched["metadata"]["annotations"]["example.org/key"], "new");
    for (content_type, value, expected) in [
        (
            json_patch,
            json!([{"op":"add","path":"/data/transient","value":"must roll back"},{"op":"test","path":"/data/moved","value":"wrong"}]),
            422,
        ),
        (
            merge,
            json!({"metadata":{"resourceVersion":original["metadata"]["resourceVersion"]}}),
            409,
        ),
        (merge, json!({"metadata":{"name":"renamed"}}), 400),
        (merge, json!({"metadata":{"namespace":"default"}}), 400),
        (merge, json!({"metadata":{"uid":"different"}}), 409),
        (merge, json!({"metadata":{"resourceVersion":42}}), 422),
        (merge, json!({"data":{"moved":42}}), 422),
        (merge, json!({"metadata":null}), 422),
        ("application/apply-patch+yaml", json!({}), 415),
        ("application/strategic-merge-patch+json", json!([]), 422),
    ] {
        let (code, failure) = s.patch(s.admin(), path, content_type, value).await;
        assert_eq!(code, expected, "{failure}");
        assert_eq!(s.json(s.admin(), "GET", path, json!({})).await.1, patched);
    }
    let (code, _) = s
        .patch(
            s.admin(),
            "/api/v1/namespaces/team-a/configmaps/missing",
            merge,
            json!({"data":{"x":"y"}}),
        )
        .await;
    assert_eq!(code, 404);
    // Two patches of the same observed revision may never overwrite each other.
    let left = json!({"metadata":{"resourceVersion":patched["metadata"]["resourceVersion"]},"data":{"winner":"left"}});
    let right = json!({"metadata":{"resourceVersion":patched["metadata"]["resourceVersion"]},"data":{"winner":"right"}});
    let (a, b) = tokio::join!(
        s.patch(s.admin(), path, merge, left),
        s.patch(s.admin(), path, merge, right)
    );
    assert!(matches!((a.0, b.0), (200, 409) | (409, 200)));
}

#[tokio::test]
async fn patch_authorization_is_distinct_from_update_and_cannot_escalate_rbac() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let obj = s.configmap("settings", "original").await;
    let base = "/apis/rbac.authorization.k8s.io/v1/namespaces/team-a";
    let role = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"Role","metadata":{"name":"patcher"},"rules":[{"apiGroups":["*"],"resources":["configmaps","roles"],"verbs":["patch"]}]});
    let binding = json!({"apiVersion":"rbac.authorization.k8s.io/v1","kind":"RoleBinding","metadata":{"name":"patcher"},"roleRef":{"apiGroup":"rbac.authorization.k8s.io","kind":"Role","name":"patcher"},"subjects":[{"apiGroup":"rbac.authorization.k8s.io","kind":"User","name":"patcher"}]});
    assert_eq!(
        s.json(s.admin(), "POST", &format!("{base}/roles"), role)
            .await
            .0,
        201
    );
    assert_eq!(
        s.json(s.admin(), "POST", &format!("{base}/rolebindings"), binding)
            .await
            .0,
        201
    );
    let cert = s.pki.issue_client("patcher", None).unwrap();
    let client = || s.pki.client_config(Some(&cert)).unwrap();
    let path = "/api/v1/namespaces/team-a/configmaps/settings";
    assert_eq!(s.json(client(), "PUT", path, obj).await.0, 403);
    assert_eq!(
        s.patch(
            client(),
            path,
            "application/merge-patch+json",
            json!({"data":{"value":"patched"}})
        )
        .await
        .0,
        200
    );
    assert_eq!(
        s.patch(
            client(),
            &format!("{base}/roles/patcher"),
            "application/merge-patch+json",
            json!({"rules":[{"apiGroups":["*"],"resources":["*"],"verbs":["*"]}]})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        s.patch(
            client(),
            "/api/v1/namespaces/default/configmaps/settings",
            "application/merge-patch+json",
            json!({})
        )
        .await
        .0,
        403
    );
}

#[tokio::test]
async fn patch_growth_is_bounded_before_persistence() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let original = s.configmap("settings", &"x".repeat(1024 * 1024)).await;
    let path = "/api/v1/namespaces/team-a/configmaps/settings";
    let (code, failure) = s
        .patch(
            s.admin(),
            path,
            "application/json-patch+json",
            json!([{"op":"copy","from":"/data/value","path":"/data/duplicate"}]),
        )
        .await;
    assert_eq!(code, 413, "{failure}");
    assert_eq!(s.json(s.admin(), "GET", path, json!({})).await.1, original);
}
