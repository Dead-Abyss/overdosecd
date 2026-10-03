pub mod json;
pub mod sqlite;

use chrono::{DateTime, Utc};

use crate::error::Result;
use crate::project::Project;

use self::json::JsonStore;
use self::sqlite::SqliteStore;

/// Storage backend for the project index.
pub trait Store {
    fn load(&self) -> Result<Vec<Project>>;

    /// Runs `f` against the index under the backend's write lock, then saves.
    ///
    /// The lock covers the whole read-modify-write cycle, so concurrent
    /// writers cannot lose each other's updates. If `f` returns an error,
    /// nothing is written.
    fn update<R>(&self, f: impl FnOnce(&mut Vec<Project>) -> Result<R>) -> Result<R>;

    /// Records a jump: bumps the cached recency/frequency counters and, on
    /// backends that keep a usage log, appends to it.
    fn record_use(&self, id: &str, now: DateTime<Utc>) -> Result<()>;

    /// Most recent jumps for `id`, newest first; a backend without a usage
    /// log returns an empty list.
    fn recent_uses(&self, id: &str, limit: usize) -> Result<Vec<DateTime<Utc>>>;

    /// The storage schema version, when the backend has one and the index
    /// exists; `None` otherwise (always `None` for the JSON backend).
    fn schema_version(&self) -> Result<Option<u32>>;
}

/// Runtime backend selection.
///
/// Commands are generic over [`Store`]; `run` picks the implementation from
/// `[storage] backend` once, and this enum forwards the trait methods to it.
pub enum AnyStore {
    Json(JsonStore),
    Sqlite(SqliteStore),
}

impl Store for AnyStore {
    fn load(&self) -> Result<Vec<Project>> {
        match self {
            AnyStore::Json(store) => store.load(),
            AnyStore::Sqlite(store) => store.load(),
        }
    }

    fn update<R>(&self, f: impl FnOnce(&mut Vec<Project>) -> Result<R>) -> Result<R> {
        match self {
            AnyStore::Json(store) => store.update(f),
            AnyStore::Sqlite(store) => store.update(f),
        }
    }

    fn record_use(&self, id: &str, now: DateTime<Utc>) -> Result<()> {
        match self {
            AnyStore::Json(store) => store.record_use(id, now),
            AnyStore::Sqlite(store) => store.record_use(id, now),
        }
    }

    fn recent_uses(&self, id: &str, limit: usize) -> Result<Vec<DateTime<Utc>>> {
        match self {
            AnyStore::Json(store) => store.recent_uses(id, limit),
            AnyStore::Sqlite(store) => store.recent_uses(id, limit),
        }
    }

    fn schema_version(&self) -> Result<Option<u32>> {
        match self {
            AnyStore::Json(store) => store.schema_version(),
            AnyStore::Sqlite(store) => store.schema_version(),
        }
    }
}
