use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};

use crate::*;

#[derive(Clone)]
pub struct SqliteStore {
    connection: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// Open on a blocking worker; callers provide a private, persistent data path.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_owned();
        let connection = tokio::task::spawn_blocking(move || -> Result<Connection> {
            let mut connection = Connection::open(path)?;
            connection.busy_timeout(Duration::from_secs(5))?;
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
            let application: i64 = tx.pragma_query_value(None, "application_id", |r| r.get(0))?;
            if version != 0 && version != 1 {
                return Err(Error::SchemaVersion(version));
            }
            if version == 0 {
                let objects: i64 = tx.query_row(
                    "SELECT count(*) FROM sqlite_schema WHERE name NOT LIKE 'sqlite_%'",
                    [],
                    |r| r.get(0),
                )?;
                if objects != 0 || application != 0 {
                    return Err(Error::ForeignDatabase);
                }
            } else if application != 0x48335331 {
                return Err(Error::ForeignDatabase);
            }
            tx.execute_batch(
                "CREATE TABLE IF NOT EXISTS registry_meta (
                singleton INTEGER PRIMARY KEY CHECK(singleton=1), revision INTEGER NOT NULL,
                compacted INTEGER NOT NULL);
                INSERT OR IGNORE INTO registry_meta VALUES(1,0,0);
                CREATE TABLE IF NOT EXISTS registry_versions (
                    key TEXT NOT NULL, revision INTEGER NOT NULL UNIQUE,
                    value BLOB NOT NULL, kind INTEGER NOT NULL CHECK(kind BETWEEN 0 AND 2),
                    PRIMARY KEY(key,revision));
                CREATE TABLE IF NOT EXISTS registry_leases (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    ttl_ms INTEGER NOT NULL, expires_ms INTEGER NOT NULL);
                PRAGMA user_version=1; PRAGMA application_id=0x48335331;",
            )?;
            tx.commit()?;
            connection.pragma_update(None, "journal_mode", "WAL")?;
            connection.pragma_update(None, "synchronous", "FULL")?;
            Ok(connection)
        })
        .await
        .map_err(|e| Error::Worker(e.to_string()))??;
        Ok(Self {
            connection: Arc::new(Mutex::new(connection)),
        })
    }

    async fn run<T: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let connection = self.connection.clone();
        tokio::task::spawn_blocking(move || {
            let mut guard = connection
                .lock()
                .map_err(|e| Error::Worker(e.to_string()))?;
            f(&mut guard)
        })
        .await
        .map_err(|e| Error::Worker(e.to_string()))?
    }

    async fn mutate(
        &self,
        mut obj: StoredObject,
        expected: Option<ResourceVersion>,
        delete: bool,
    ) -> Result<StoredObject> {
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let previous = get_current(&tx, &obj.key)?;
            match (expected, previous.as_ref()) {
                (None, Some(_)) => return Err(Error::AlreadyExists(obj.key)),
                (Some(_), None) => return Err(Error::NotFound(obj.key)),
                (Some(expected), Some(old)) if expected != old.revision => {
                    return Err(Error::Conflict {
                        expected,
                        actual: old.revision,
                    })
                }
                _ => {}
            }
            let kind = if delete {
                EventKind::Deleted
            } else if previous.is_some() {
                EventKind::Modified
            } else {
                EventKind::Added
            };
            if delete {
                obj.value = previous.expect("validated existing object").value;
            }
            tx.execute(
                "UPDATE registry_meta SET revision=revision+1 WHERE singleton=1",
                [],
            )?;
            obj.revision = head(&tx)?.0;
            tx.execute(
                "INSERT INTO registry_versions(key,revision,value,kind) VALUES(?1,?2,?3,?4)",
                params![
                    obj.key.as_str(),
                    revision_i64(obj.revision)?,
                    &obj.value,
                    kind_number(kind)
                ],
            )?;
            tx.commit()?;
            Ok(obj)
        })
        .await
    }

    async fn changes(
        &self,
        prefix: String,
        after: ResourceVersion,
    ) -> Result<(Vec<WatchEvent>, ResourceVersion)> {
        self.run(move |connection| {
            let tx = connection.transaction()?;
            let (current, floor) = head(&tx)?;
            validate_revision(after, current, floor)?;
            let mut statement = tx.prepare(
                "SELECT key,revision,value,kind FROM registry_versions
                WHERE revision>?1 AND substr(key,1,length(?2))=?2 ORDER BY revision LIMIT 256",
            )?;
            let rows = statement.query_map(params![revision_i64(after)?, prefix], |r| {
                let revision = read_revision(r, 1)?;
                let kind = match r.get::<_, i64>(3)? {
                    0 => EventKind::Added,
                    1 => EventKind::Modified,
                    _ => EventKind::Deleted,
                };
                Ok(WatchEvent {
                    kind,
                    revision,
                    object: Some(StoredObject {
                        key: StoreKey(r.get(0)?),
                        revision,
                        value: r.get(2)?,
                    }),
                })
            })?;
            let events = rows.collect::<std::result::Result<Vec<_>, _>>()?;
            let next = if events.len() == 256 {
                events.last().expect("full batch").revision
            } else {
                current
            };
            Ok((events, next))
        })
        .await
    }
}

fn read_revision(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, value))
}

fn revision_i64(rev: ResourceVersion) -> Result<i64> {
    i64::try_from(rev).map_err(|_| Error::Invalid("revision exceeds backend range".into()))
}

fn head(c: &Connection) -> Result<(ResourceVersion, ResourceVersion)> {
    Ok(c.query_row(
        "SELECT revision,compacted FROM registry_meta WHERE singleton=1",
        [],
        |r| Ok((read_revision(r, 0)?, read_revision(r, 1)?)),
    )?)
}

fn validate_revision(
    rev: ResourceVersion,
    current: ResourceVersion,
    floor: ResourceVersion,
) -> Result<()> {
    if rev < floor {
        return Err(Error::Compacted {
            requested: rev,
            floor,
        });
    }
    if rev > current {
        return Err(Error::FutureRevision {
            requested: rev,
            current,
        });
    }
    Ok(())
}

fn get_current(c: &Connection, key: &StoreKey) -> Result<Option<StoredObject>> {
    let row = c.query_row("SELECT revision,value,kind FROM registry_versions WHERE key=?1 ORDER BY revision DESC LIMIT 1",
        [key.as_str()], |r| Ok((read_revision(r, 0)?,r.get::<_,Vec<u8>>(1)?,r.get::<_,i64>(2)?))).optional()?;
    Ok(row
        .filter(|(_, _, kind)| *kind != 2)
        .map(|(revision, value, _)| StoredObject {
            key: key.clone(),
            revision,
            value,
        }))
}

fn kind_number(k: EventKind) -> i64 {
    match k {
        EventKind::Added => 0,
        EventKind::Modified => 1,
        EventKind::Deleted => 2,
        EventKind::Bookmark => unreachable!(),
    }
}

fn now_ms() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| Error::Worker(e.to_string()))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| Error::Invalid("clock exceeds backend range".into()))
}

#[async_trait]
impl Storage for SqliteStore {
    async fn get(&self, key: &StoreKey) -> Result<Option<StoredObject>> {
        let key = key.clone();
        self.run(move |c| get_current(c, &key)).await
    }

    async fn list(&self, sel: ListSelect) -> Result<ObjectList> {
        validate_prefix(&sel.prefix)?;
        if sel.limit == 0 || sel.limit > 4096 {
            return Err(Error::Invalid("LIST limit must be 1..4096".into()));
        }
        if sel
            .start_after
            .as_ref()
            .is_some_and(|key| !key.as_str().starts_with(&sel.prefix))
        {
            return Err(Error::Invalid(
                "continuation key is outside the selection".into(),
            ));
        }
        self.run(move |connection| {
            let tx=connection.transaction()?;
            let (current,floor)=head(&tx)?;
            let revision=sel.at_revision.filter(|r| *r!=0).unwrap_or(current);
            validate_revision(revision,current,floor)?;
            let mut statement=tx.prepare("SELECT v.key,v.revision,v.value FROM registry_versions v
                WHERE substr(v.key,1,length(?1))=?1 AND v.key>?2 AND v.kind!=2
                AND v.revision=(SELECT MAX(p.revision) FROM registry_versions p WHERE p.key=v.key AND p.revision<=?3)
                ORDER BY v.key LIMIT ?4")?;
            let rows=statement.query_map(params![sel.prefix,sel.start_after.as_ref().map(StoreKey::as_str).unwrap_or(""),revision_i64(revision)?,(sel.limit+1) as i64], |r| {
                Ok(StoredObject { key:StoreKey(r.get(0)?),revision:read_revision(r, 1)?,value:r.get(2)? })
            })?;
            let mut items=rows.collect::<std::result::Result<Vec<_>,_>>()?;
            let next_after=if items.len()>sel.limit { items.pop();items.last().map(|x|x.key.clone()) } else { None };
            Ok(ObjectList { items,revision,next_after })
        }).await
    }

    async fn create(&self, obj: StoredObject) -> Result<StoredObject> {
        self.mutate(obj, None, false).await
    }
    async fn update(&self, obj: StoredObject, rv: ResourceVersion) -> Result<StoredObject> {
        self.mutate(obj, Some(rv), false).await
    }
    async fn delete(&self, key: &StoreKey, rv: ResourceVersion) -> Result<()> {
        self.mutate(
            StoredObject {
                key: key.clone(),
                value: vec![],
                revision: 0,
            },
            Some(rv),
            true,
        )
        .await?;
        Ok(())
    }

    async fn watch(&self, sel: WatchSelect) -> Result<WatchStream> {
        validate_prefix(&sel.prefix)?;
        if sel.bookmark_interval.is_zero() {
            return Err(Error::Invalid("bookmark interval must be positive".into()));
        }
        let requested = sel.after_revision.filter(|r| *r != 0);
        let anchor = self
            .run(move |connection| {
                let (current, floor) = head(connection)?;
                let revision = requested.unwrap_or(current);
                validate_revision(revision, current, floor)?;
                Ok(revision)
            })
            .await?;
        let store = self.clone();
        Ok(Box::pin(async_stream::try_stream! {
            if requested.is_none() {
                let mut next=None;
                if anchor > 0 { loop {
                    let page=store.list(ListSelect { prefix:sel.prefix.clone(),at_revision:Some(anchor),start_after:next,limit:256 }).await?;
                    for object in page.items {
                        yield WatchEvent { kind:EventKind::Added,revision:object.revision,object:Some(object) };
                    }
                    next=page.next_after;
                    if next.is_none() { break; }
                }}
                yield WatchEvent { kind:EventKind::Bookmark,revision:anchor,object:None };
            }
            let mut cursor=anchor;
            let mut bookmark=tokio::time::Instant::now();
            loop {
                let (events,next)=store.changes(sel.prefix.clone(),cursor).await?;
                let full=events.len()==256;
                for event in events { yield event; }
                cursor=next;
                if bookmark.elapsed()>=sel.bookmark_interval {
                    yield WatchEvent { kind:EventKind::Bookmark,revision:cursor,object:None };
                    bookmark=tokio::time::Instant::now();
                }
                if !full { tokio::time::sleep(Duration::from_millis(50)).await; }
            }
        }))
    }

    async fn compact(&self, rev: ResourceVersion) -> Result<()> {
        self.run(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (current, floor) = head(&tx)?;
            if rev > current {
                return Err(Error::FutureRevision {
                    requested: rev,
                    current,
                });
            }
            if rev <= floor {
                return Ok(());
            }
            // Retain each key's baseline at the floor, plus all newer changes.
            tx.execute(
                "DELETE FROM registry_versions WHERE revision<?1 AND revision NOT IN
                (SELECT MAX(revision) FROM registry_versions WHERE revision<=?1 GROUP BY key)",
                [revision_i64(rev)?],
            )?;
            tx.execute(
                "UPDATE registry_meta SET compacted=?1 WHERE singleton=1",
                [revision_i64(rev)?],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
    }

    async fn lease_grant(&self, ttl: Duration) -> Result<Lease> {
        let ttl_ms = i64::try_from(ttl.as_millis())
            .map_err(|_| Error::Invalid("lease TTL too large".into()))?;
        if ttl_ms <= 0 {
            return Err(Error::Invalid("lease TTL must be at least 1ms".into()));
        }
        self.run(move |connection| {
            let expires = now_ms()?
                .checked_add(ttl_ms)
                .ok_or_else(|| Error::Invalid("lease expiry overflow".into()))?;
            connection.execute(
                "INSERT INTO registry_leases(ttl_ms,expires_ms) VALUES(?1,?2)",
                params![ttl_ms, expires],
            )?;
            Ok(Lease {
                id: connection.last_insert_rowid() as u64,
                ttl,
            })
        })
        .await
    }

    async fn lease_keepalive(&self, id: LeaseId) -> Result<()> {
        self.run(move |connection| {
            let now = now_ms()?;
            let changed = connection.execute(
                "UPDATE registry_leases SET expires_ms=?1+ttl_ms WHERE id=?2 AND expires_ms>?1",
                params![now, revision_i64(id)?],
            )?;
            if changed == 0 {
                return Err(Error::LeaseExpired(id));
            }
            Ok(())
        })
        .await
    }
}
