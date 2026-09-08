use futures_util::StreamExt;
use h3s_storage::*;
use std::time::Duration;

fn object(name: &str, value: &str) -> StoredObject {
    StoredObject {
        key: StoreKey::new(format!("/registry/pods/test/{name}")).unwrap(),
        value: value.as_bytes().to_vec(),
        revision: 0,
    }
}
async fn next(stream: &mut WatchStream) -> WatchEvent {
    tokio::time::timeout(Duration::from_secs(3), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn durable_crud_cas_and_delete_recreate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.db");
    let store = SqliteStore::open(&path).await.unwrap();
    let first = store.create(object("a", "one")).await.unwrap();
    assert_eq!(first.revision, 1);
    assert!(matches!(
        store.create(object("a", "duplicate")).await,
        Err(Error::AlreadyExists(_))
    ));
    let second = store
        .update(object("a", "two"), first.revision)
        .await
        .unwrap();
    assert_eq!(second.revision, 2);
    assert!(matches!(
        store.update(object("a", "stale"), first.revision).await,
        Err(Error::Conflict {
            expected: 1,
            actual: 2
        })
    ));
    assert!(matches!(
        store.delete(&first.key, first.revision).await,
        Err(Error::Conflict { .. })
    ));
    drop(store);
    let reopened = SqliteStore::open(&path).await.unwrap();
    assert_eq!(
        reopened.get(&first.key).await.unwrap(),
        Some(second.clone())
    );
    let mut watch = reopened
        .watch(WatchSelect::new(
            "/registry/pods/test/",
            Some(second.revision),
        ))
        .await
        .unwrap();
    reopened.delete(&first.key, second.revision).await.unwrap();
    assert!(reopened.get(&first.key).await.unwrap().is_none());
    let recreated = reopened.create(object("a", "three")).await.unwrap();
    assert_eq!(recreated.revision, 4);
    let deleted = next(&mut watch).await;
    assert_eq!(deleted.kind, EventKind::Deleted);
    assert_eq!(deleted.revision, 3);
    assert_eq!(deleted.object.unwrap().value, b"two");
    assert_eq!(next(&mut watch).await.object, Some(recreated));
}

#[tokio::test]
async fn competing_connections_have_one_cas_winner_and_watch_observes_commit() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("registry.db");
    let a = SqliteStore::open(&path).await.unwrap();
    let b = SqliteStore::open(&path).await.unwrap();
    let initial = a.create(object("a", "old")).await.unwrap();
    // Construct the watch before either write, but poll it only after commit.
    let mut watch = b
        .watch(WatchSelect::new(
            "/registry/pods/test/",
            Some(initial.revision),
        ))
        .await
        .unwrap();
    let (x, y) = tokio::join!(
        a.update(object("a", "left"), initial.revision),
        b.update(object("a", "right"), initial.revision)
    );
    assert_eq!(usize::from(x.is_ok()) + usize::from(y.is_ok()), 1);
    let winner = match (x, y) {
        (Ok(obj), Err(Error::Conflict { .. })) | (Err(Error::Conflict { .. }), Ok(obj)) => obj,
        other => panic!("unexpected CAS results: {other:?}"),
    };
    assert_eq!(winner.revision, 2);
    assert_eq!(next(&mut watch).await.object, Some(winner));
}

#[tokio::test]
async fn pagination_preserves_snapshot_across_concurrent_mutations() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("db")).await.unwrap();
    let a = store.create(object("a", "old-a")).await.unwrap();
    let b = store.create(object("b", "old-b")).await.unwrap();
    store
        .create(StoredObject {
            key: StoreKey::new("/registry/pods/test-other/hidden").unwrap(),
            value: vec![],
            revision: 0,
        })
        .await
        .unwrap();
    let mut sel = ListSelect::new("/registry/pods/test/");
    sel.limit = 1;
    let first = store.list(sel.clone()).await.unwrap();
    assert_eq!(first.items, vec![a]);
    assert!(first.next_after.is_some());
    store
        .update(object("b", "new-b"), b.revision)
        .await
        .unwrap();
    store.create(object("c", "new-c")).await.unwrap();
    sel.at_revision = Some(first.revision);
    sel.start_after = first.next_after;
    let second = store.list(sel).await.unwrap();
    assert_eq!(second.revision, first.revision);
    assert_eq!(second.items, vec![b]);
    assert!(second.next_after.is_none());
}

#[tokio::test]
async fn compaction_retains_floor_snapshot_and_rejects_stale_readers() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let store = SqliteStore::open(&path).await.unwrap();
    let a = store.create(object("a", "one")).await.unwrap();
    let b = store.create(object("b", "two")).await.unwrap();
    let mut lagging = store
        .watch(WatchSelect::new("/registry/pods/test/", Some(a.revision)))
        .await
        .unwrap();
    let changed = store
        .update(object("a", "three"), a.revision)
        .await
        .unwrap();
    store.compact(b.revision).await.unwrap();
    let mut sel = ListSelect::new("/registry/pods/test/");
    sel.at_revision = Some(b.revision);
    assert_eq!(
        store.list(sel.clone()).await.unwrap().items,
        vec![a.clone(), b.clone()]
    );
    sel.at_revision = Some(a.revision);
    assert!(matches!(
        store.list(sel).await,
        Err(Error::Compacted { .. })
    ));
    assert!(matches!(
        lagging.next().await,
        Some(Err(Error::Compacted { .. }))
    ));
    let mut current = store
        .watch(WatchSelect::new("/registry/pods/test/", Some(b.revision)))
        .await
        .unwrap();
    assert_eq!(next(&mut current).await.object, Some(changed.clone()));
    store.compact(changed.revision).await.unwrap();
    store.compact(1).await.unwrap();
    drop(store);
    let reopened = SqliteStore::open(&path).await.unwrap();
    assert_eq!(reopened.get(&a.key).await.unwrap(), Some(changed));
    assert!(matches!(
        reopened
            .watch(WatchSelect::new("/registry/", Some(1)))
            .await,
        Err(Error::Compacted { .. })
    ));
}

#[tokio::test]
async fn initial_watch_snapshot_bookmark_and_subsequent_changes() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("db")).await.unwrap();
    let a = store.create(object("a", "one")).await.unwrap();
    let mut sel = WatchSelect::new("/registry/pods/test/", None);
    sel.bookmark_interval = Duration::from_millis(20);
    let mut watch = store.watch(sel).await.unwrap();
    let b = store.create(object("b", "two")).await.unwrap();
    assert_eq!(next(&mut watch).await.object, Some(a.clone()));
    let bookmark = next(&mut watch).await;
    assert_eq!(bookmark.kind, EventKind::Bookmark);
    assert_eq!(bookmark.revision, a.revision);
    assert_eq!(next(&mut watch).await.object, Some(b.clone()));
    let bookmark = next(&mut watch).await;
    assert_eq!(bookmark.kind, EventKind::Bookmark);
    assert_eq!(bookmark.revision, b.revision);
}

#[tokio::test]
async fn empty_watch_registration_does_not_duplicate_first_write() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("db")).await.unwrap();
    let mut watch = store
        .watch(WatchSelect::new("/registry/pods/test/", None))
        .await
        .unwrap();
    let obj = store.create(object("a", "first")).await.unwrap();
    let bookmark = next(&mut watch).await;
    assert_eq!(bookmark.kind, EventKind::Bookmark);
    assert_eq!(bookmark.revision, 0);
    assert_eq!(next(&mut watch).await.object, Some(obj));
    assert!(
        tokio::time::timeout(Duration::from_millis(150), watch.next())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn lease_reopen_keepalive_expiry_and_invalid_inputs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("db");
    let store = SqliteStore::open(&path).await.unwrap();
    let lease = store.lease_grant(Duration::from_secs(5)).await.unwrap();
    drop(store);
    let reopened = SqliteStore::open(&path).await.unwrap();
    reopened.lease_keepalive(lease.id).await.unwrap();
    let short = reopened
        .lease_grant(Duration::from_millis(1))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(matches!(
        reopened.lease_keepalive(short.id).await,
        Err(Error::LeaseExpired(_))
    ));
    assert!(matches!(
        reopened.lease_grant(Duration::ZERO).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        reopened.lease_keepalive(999).await,
        Err(Error::LeaseExpired(999))
    ));
    for key in [
        "/registry/pods//bad",
        "/registry/pods/../bad",
        "/registry/pods/test/bad/name",
        "outside",
    ] {
        assert!(StoreKey::new(key).is_err());
    }
    assert!(matches!(
        reopened.list(ListSelect::new("/registry/pods/test")).await,
        Err(Error::Invalid(_))
    ));
    let mut sel = ListSelect::new("/registry/");
    sel.at_revision = Some(999);
    assert!(matches!(
        reopened.list(sel).await,
        Err(Error::FutureRevision { .. })
    ));
}

#[tokio::test]
async fn foreign_database_and_future_schema_are_not_modified() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("other.db");
    let other = rusqlite::Connection::open(&path).unwrap();
    other
        .execute_batch(
            "CREATE TABLE other_product(value TEXT); INSERT INTO other_product VALUES('preserve');",
        )
        .unwrap();
    drop(other);
    let original = std::fs::read(&path).unwrap();
    assert!(matches!(
        SqliteStore::open(&path).await,
        Err(Error::ForeignDatabase)
    ));
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let other = rusqlite::Connection::open(&path).unwrap();
    other.pragma_update(None, "user_version", 99).unwrap();
    drop(other);
    let original = std::fs::read(&path).unwrap();
    assert!(matches!(
        SqliteStore::open(&path).await,
        Err(Error::SchemaVersion(99))
    ));
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[tokio::test]
async fn unpolled_watch_reads_multiple_durable_batches_without_loss() {
    let dir = tempfile::tempdir().unwrap();
    let store = SqliteStore::open(dir.path().join("db")).await.unwrap();
    let mut current = store.create(object("a", "initial")).await.unwrap();
    let mut watch = store
        .watch(WatchSelect::new(
            "/registry/pods/test/",
            Some(current.revision),
        ))
        .await
        .unwrap();
    for i in 0..600 {
        current = store
            .update(object("a", &i.to_string()), current.revision)
            .await
            .unwrap();
    }
    for revision in 2..=601 {
        let event = next(&mut watch).await;
        assert_eq!(event.revision, revision);
        assert_eq!(event.kind, EventKind::Modified);
    }
    assert_eq!(store.get(&current.key).await.unwrap(), Some(current));
}
