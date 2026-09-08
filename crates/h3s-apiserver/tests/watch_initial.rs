mod common;
use common::Server;
use h3s_storage::{SqliteStore, Storage};
use http_body_util::BodyExt;
use serde_json::{json, Value};

const PATH: &str = "/api/v1/namespaces/team-a/configmaps";
const INITIAL: &str = "watch=true&sendInitialEvents=true&resourceVersionMatch=NotOlderThan";

async fn events(response: hyper::Response<hyper::body::Incoming>) -> Vec<Value> {
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn revision(object: &Value) -> u64 {
    object["metadata"]["resourceVersion"]
        .as_str()
        .unwrap()
        .parse()
        .unwrap()
}

fn assert_boundary(event: &Value) -> u64 {
    assert_eq!(event["type"], "BOOKMARK");
    assert_eq!(
        event["object"]["metadata"]["annotations"],
        json!({"k8s.io/initial-events-end":"true"})
    );
    revision(&event["object"])
}

#[tokio::test]
async fn initial_watch_finishes_filtered_multi_page_snapshot_before_live_changes() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let mut expected = Vec::new();
    for i in 0..260 {
        let selected = i == 0 || i == 259;
        let (code, object) = s.json(s.admin(), "POST", PATH, json!({
            "apiVersion":"v1", "kind":"ConfigMap",
            "metadata":{"name":format!("item-{i:03}"), "labels":{"app":if selected {"web"} else {"other"}}}
        })).await;
        assert_eq!(code, 201, "{object}");
        if selected {
            expected.push(object);
        }
    }
    let response = s.raw(s.admin(), "GET",
        &format!("{PATH}?{INITIAL}&resourceVersion=&labelSelector=app%3Dweb&allowWatchBookmarks=true&timeoutSeconds=2"),
        json!({}), &[]).await;
    // The subscription already has its snapshot anchor. Mutate its last page
    // before draining the body; initial state and subsequent changes must both
    // survive without a gap or an early completion bookmark.
    let mut changed = expected[1].clone();
    changed["data"] = json!({"value":"after-subscription"});
    let (code, updated) = s
        .json(s.admin(), "PUT", &format!("{PATH}/item-259"), changed)
        .await;
    assert_eq!(code, 200);
    let actual = events(response).await;
    assert_eq!(actual.len(), 4, "{actual:?}");
    for (event, object) in actual[..2].iter().zip(expected) {
        assert_eq!(event, &json!({"type":"ADDED","object":object}));
    }
    let anchor = assert_boundary(&actual[2]);
    assert!(actual[..2]
        .iter()
        .all(|event| revision(&event["object"]) <= anchor));
    assert_eq!(actual[3], json!({"type":"MODIFIED","object":updated}));
    assert!(revision(&actual[3]["object"]) > anchor);
}

#[tokio::test]
async fn initial_watch_empty_selection_still_completes_without_periodic_bookmarks() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    s.configmap("outside", "hidden").await;
    let actual = events(s.raw(s.admin(), "GET",
        &format!("{PATH}?{INITIAL}&resourceVersion=0&labelSelector=app%3Dweb&allowWatchBookmarks=false&timeoutSeconds=1"),
        json!({}), &[]).await).await;
    assert_eq!(actual.len(), 1);
    assert_boundary(&actual[0]);
}

#[tokio::test]
async fn initial_not_older_than_replaces_compacted_revision_with_current_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    let old = s.configmap("old", "one").await;
    let latest = s.configmap("latest", "two").await;
    let store = SqliteStore::open(dir.path().join("registry.db"))
        .await
        .unwrap();
    store.compact(revision(&latest)).await.unwrap();
    let old_rv = revision(&old);
    assert_eq!(
        s.json(
            s.admin(),
            "GET",
            &format!("{PATH}?watch=true&resourceVersion={old_rv}"),
            json!({})
        )
        .await
        .0,
        410
    );
    let actual = events(
        s.raw(
            s.admin(),
            "GET",
            &format!("{PATH}?{INITIAL}&resourceVersion={old_rv}&timeoutSeconds=1"),
            json!({}),
            &[],
        )
        .await,
    )
    .await;
    assert_eq!(actual.len(), 3);
    assert_eq!(actual[0]["object"], latest);
    assert_eq!(actual[1]["object"], old);
    assert!(assert_boundary(&actual[2]) >= revision(&latest));
}

#[tokio::test]
async fn explicit_no_initial_events_only_delivers_changes_after_subscription() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    s.configmap("existing", "old").await;
    for rv in ["", "0"] {
        let response = s.raw(s.admin(), "GET",
            &format!("{PATH}?watch=true&sendInitialEvents=false&resourceVersionMatch=NotOlderThan&resourceVersion={rv}&timeoutSeconds=1"),
            json!({}), &[]).await;
        let created = s
            .configmap(
                if rv.is_empty() {
                    "new-empty"
                } else {
                    "new-zero"
                },
                "live",
            )
            .await;
        assert_eq!(
            events(response).await,
            vec![json!({"type":"ADDED","object":created})]
        );
    }
}

#[tokio::test]
async fn streaming_list_rejects_invalid_options_and_preserves_authorization() {
    let dir = tempfile::tempdir().unwrap();
    let s = Server::start(dir.path()).await;
    s.namespace("team-a").await;
    for query in [
        "sendInitialEvents=true",
        "sendInitialEvents=false",
        "watch=true&sendInitialEvents=invalid",
        "watch=true&sendInitialEvents=true",
        "watch=true&sendInitialEvents=false",
        "watch=true&resourceVersionMatch=NotOlderThan",
        "watch=true&sendInitialEvents=true&resourceVersionMatch=Exact",
        "watch=true&sendInitialEvents=false&resourceVersionMatch=Exact",
        "watch=true&sendInitialEvents=true&resourceVersionMatch=NotOlderThan&continue=invalid",
        "watch=true&sendInitialEvents=true&resourceVersionMatch=NotOlderThan&resourceVersion=invalid",
    ] {
        assert_eq!(s.json(s.admin(), "GET", &format!("{PATH}?{query}"), json!({})).await.0, 400, "{query}");
    }
    let query = format!("{PATH}?{INITIAL}");
    let alice = s.pki.issue_client("alice", None).unwrap();
    assert_eq!(
        s.json(
            s.pki.client_config(Some(&alice)).unwrap(),
            "GET",
            &query,
            json!({})
        )
        .await
        .0,
        403
    );
    assert_eq!(
        s.json(s.pki.client_config(None).unwrap(), "GET", &query, json!({}))
            .await
            .0,
        401
    );
    let (code, failure) = s
        .json(
            s.admin(),
            "GET",
            &format!("{query}&resourceVersion={}", u64::MAX),
            json!({}),
        )
        .await;
    assert_ne!(code, 200);
    assert_eq!(failure["kind"], "Status");
}
