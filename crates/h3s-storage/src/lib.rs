//! Durable Kubernetes registry storage. Object payloads are opaque to the store.
//!
//! SQLite keeps an MVCC version log: a LIST page reads a fixed revision and WATCH
//! resumes after that revision. Slow watchers read bounded batches from disk; they
//! fail explicitly after compaction rather than silently losing events.

use std::{pin::Pin, time::Duration};

use async_trait::async_trait;
use futures_core::Stream;

mod sqlite;
pub use sqlite::SqliteStore;

pub const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
pub type ResourceVersion = u64;
pub type LeaseId = u64;
pub type Result<T> = std::result::Result<T, Error>;
pub type WatchStream = Pin<Box<dyn Stream<Item = Result<WatchEvent>> + Send>>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid registry key or selection: {0}")]
    Invalid(String),
    #[error("object already exists: {0}")]
    AlreadyExists(StoreKey),
    #[error("object not found: {0}")]
    NotFound(StoreKey),
    #[error("resource version conflict: expected {expected}, current {actual}")]
    Conflict {
        expected: ResourceVersion,
        actual: ResourceVersion,
    },
    #[error("revision {requested} is older than compaction floor {floor}")]
    Compacted {
        requested: ResourceVersion,
        floor: ResourceVersion,
    },
    #[error("revision {requested} is newer than current revision {current}")]
    FutureRevision {
        requested: ResourceVersion,
        current: ResourceVersion,
    },
    #[error("lease {0} is missing or expired")]
    LeaseExpired(LeaseId),
    #[error("database is not an h3s registry; refusing to initialize or modify it")]
    ForeignDatabase,
    #[error("unsupported registry schema version {0}")]
    SchemaVersion(i64),
    #[error("registry database: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("registry worker: {0}")]
    Worker(String),
}

/// `/registry/{resource}/{name}` or `/registry/{resource}/{namespace}/{name}`.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct StoreKey(String);

impl StoreKey {
    pub fn new(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let parts: Vec<_> = value.split('/').collect();
        if !(parts.len() == 4 || parts.len() == 5)
            || !parts[0].is_empty()
            || parts[1] != "registry"
            || parts[2..].iter().any(|p| !valid_segment(p))
        {
            return Err(Error::Invalid(value));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for StoreKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

fn valid_segment(s: &str) -> bool {
    !s.is_empty()
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-._".contains(&b))
}

fn validate_prefix(prefix: &str) -> Result<()> {
    let Some(tail) = prefix.strip_prefix("/registry/") else {
        return Err(Error::Invalid(prefix.into()));
    };
    if tail.is_empty() {
        return Ok(());
    }
    let Some(tail) = tail.strip_suffix('/') else {
        return Err(Error::Invalid(prefix.into()));
    };
    if tail.split('/').count() > 2 || tail.split('/').any(|s| !valid_segment(s)) {
        return Err(Error::Invalid(prefix.into()));
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredObject {
    pub key: StoreKey,
    pub value: Vec<u8>,
    /// Assigned by storage on mutation; an input object's revision is not trusted.
    pub revision: ResourceVersion,
}

#[derive(Clone, Debug)]
pub struct ListSelect {
    /// A whole registry/resource/namespace prefix, ending in `/`.
    pub prefix: String,
    /// None or zero selects the current snapshot. Subsequent pages reuse the RV.
    pub at_revision: Option<ResourceVersion>,
    pub start_after: Option<StoreKey>,
    pub limit: usize,
}

impl ListSelect {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
            at_revision: None,
            start_after: None,
            limit: 256,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ObjectList {
    pub items: Vec<StoredObject>,
    pub revision: ResourceVersion,
    /// Opaque API continuation tokens can carry this key and the snapshot RV.
    pub next_after: Option<StoreKey>,
}

#[derive(Clone, Debug)]
pub struct WatchSelect {
    pub prefix: String,
    /// Resume strictly after this revision. None/zero emits a consistent initial
    /// snapshot followed by changes. API adapters choose their wire semantics.
    pub after_revision: Option<ResourceVersion>,
    pub bookmark_interval: Duration,
}

impl WatchSelect {
    pub fn new(prefix: impl Into<String>, after_revision: Option<ResourceVersion>) -> Self {
        Self {
            prefix: prefix.into(),
            after_revision,
            bookmark_interval: Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EventKind {
    Added,
    Modified,
    Deleted,
    Bookmark,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WatchEvent {
    /// Previous live version, read in the same MVCC snapshot as this event.
    /// Selector watches use it to report objects leaving the watched view.
    pub previous: Option<StoredObject>,
    pub kind: EventKind,
    pub revision: ResourceVersion,
    pub object: Option<StoredObject>,
}

#[derive(Clone, Debug)]
pub struct Lease {
    pub id: LeaseId,
    pub ttl: Duration,
}

#[async_trait]
pub trait Storage: Send + Sync + 'static {
    async fn get(&self, key: &StoreKey) -> Result<Option<StoredObject>>;
    async fn list(&self, sel: ListSelect) -> Result<ObjectList>;
    async fn create(&self, obj: StoredObject) -> Result<StoredObject>;
    async fn update(&self, obj: StoredObject, rv: ResourceVersion) -> Result<StoredObject>;
    async fn delete(&self, key: &StoreKey, rv: ResourceVersion) -> Result<()>;
    async fn watch(&self, sel: WatchSelect) -> Result<WatchStream>;
    async fn compact(&self, rev: ResourceVersion) -> Result<()>;
    async fn lease_grant(&self, ttl: Duration) -> Result<Lease>;
    async fn lease_keepalive(&self, id: LeaseId) -> Result<()>;
}
