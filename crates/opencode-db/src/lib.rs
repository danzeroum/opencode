//! Persistence layer (Phase 2): the single shared SQLite pool, the event-store repositories, the
//! migration-journal verifier, and the session-store repositories.
//!
//! The on-disk schema is **owned by the TypeScript server** during the migration ("TS migrates,
//! Rust verifies"): Rust opens the same SQLite file, creates only the append-only `event` tables it
//! needs if they are missing (`IF NOT EXISTS`, a no-op against a TS-created DB), and **verifies** the
//! migration journal on boot rather than applying migrations (see [`migration`]). Full
//! migration-apply is deferred to Phase 6.

pub mod migration;
pub mod session;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

// Re-export the event-store contract types: they appear in this crate's public trait signatures, so
// downstream callers need them without taking a direct `opencode-events` dependency.
pub use opencode_events::{EventInput, StoredEvent};

pub use migration::{MigrationReport, EXPECTED_MIGRATIONS};
pub use session::{
    MemorySessionStore, SessionContextEpoch, SessionContextEpochRepo, SessionInput,
    SessionInputRepo, SessionListQuery, SessionRecord, SessionStore, SqlxSessionStore,
    SESSION_CONTEXT_EPOCH_DDL, SESSION_INPUT_DDL,
};

/// Database / event-store errors.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// Optimistic-concurrency conflict: the expected head seq didn't match the stored head — the
    /// analog of the unique `(aggregate_id, seq)` index rejecting a write. Maps to TS `ConflictError`.
    #[error("optimistic concurrency conflict on {aggregate_id}: expected head {expected}, found {found}")]
    Conflict {
        /// Aggregate whose append conflicted.
        aggregate_id: String,
        /// Head the caller expected.
        expected: i64,
        /// Head actually found in the store.
        found: i64,
    },
    /// SQL / database error.
    #[error(transparent)]
    Sql(#[from] sqlx::Error),
    /// JSON (de)serialization error for the event `data` column.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Any other persistence error.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Append-only event store keyed by `(aggregate_id, seq)` with optimistic concurrency.
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Append `events` to `aggregate_id`, requiring the current head seq to equal `expected_head`
    /// (0 for a new aggregate). Returns the new head seq, or [`DbError::Conflict`] on mismatch.
    async fn append(
        &self,
        aggregate_id: &str,
        expected_head: i64,
        events: Vec<EventInput>,
    ) -> Result<i64, DbError>;

    /// Read events for `aggregate_id` with `seq > from_seq`, in order.
    async fn read(&self, aggregate_id: &str, from_seq: i64) -> Result<Vec<StoredEvent>, DbError>;

    /// The current head seq for `aggregate_id` (0 if it has no events).
    async fn head_seq(&self, aggregate_id: &str) -> Result<i64, DbError>;
}

/// In-memory [`EventStore`] — reference implementation and test double.
#[derive(Default)]
pub struct MemoryEventStore {
    logs: std::sync::Mutex<std::collections::HashMap<String, Vec<StoredEvent>>>,
}

impl MemoryEventStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl EventStore for MemoryEventStore {
    async fn append(
        &self,
        aggregate_id: &str,
        expected_head: i64,
        events: Vec<EventInput>,
    ) -> Result<i64, DbError> {
        let mut logs = self.logs.lock().expect("event store mutex poisoned");
        let log = logs.entry(aggregate_id.to_string()).or_default();
        let found = log.last().map(|e| e.seq).unwrap_or(0);
        if found != expected_head {
            return Err(DbError::Conflict {
                aggregate_id: aggregate_id.to_string(),
                expected: expected_head,
                found,
            });
        }
        let mut seq = found;
        for ev in events {
            seq += 1;
            log.push(StoredEvent {
                id: format!("evt_{}", ulid::Ulid::new().to_string().to_lowercase()),
                aggregate_id: aggregate_id.to_string(),
                seq,
                kind: ev.kind,
                data: ev.data,
            });
        }
        Ok(seq)
    }

    async fn read(&self, aggregate_id: &str, from_seq: i64) -> Result<Vec<StoredEvent>, DbError> {
        let logs = self.logs.lock().expect("event store mutex poisoned");
        Ok(logs
            .get(aggregate_id)
            .map(|log| log.iter().filter(|e| e.seq > from_seq).cloned().collect())
            .unwrap_or_default())
    }

    async fn head_seq(&self, aggregate_id: &str) -> Result<i64, DbError> {
        let logs = self.logs.lock().expect("event store mutex poisoned");
        Ok(logs
            .get(aggregate_id)
            .and_then(|l| l.last())
            .map(|e| e.seq)
            .unwrap_or(0))
    }
}

/// The event-store DDL, reused **verbatim** from `packages/core/src/event/sql.ts` for on-disk
/// compatibility with the TypeScript store. `IF NOT EXISTS` makes it a no-op against a TS-created DB.
const SCHEMA_DDL: &str = "\
CREATE TABLE IF NOT EXISTS event_sequence (\
  aggregate_id TEXT PRIMARY KEY NOT NULL,\
  seq INTEGER NOT NULL,\
  owner_id TEXT\
);\
CREATE TABLE IF NOT EXISTS event (\
  id TEXT PRIMARY KEY NOT NULL,\
  aggregate_id TEXT NOT NULL REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE,\
  seq INTEGER NOT NULL,\
  type TEXT NOT NULL,\
  data TEXT NOT NULL\
);\
CREATE UNIQUE INDEX IF NOT EXISTS event_aggregate_seq_idx ON event (aggregate_id, seq);\
CREATE INDEX IF NOT EXISTS event_aggregate_type_seq_idx ON event (aggregate_id, type, seq);";

/// Whether a path string refers to SQLite's in-memory database.
fn is_memory(path: &Path) -> bool {
    path.as_os_str() == ":memory:"
}

/// Open (creating if missing) a SQLite pool at `path`, applying the same per-connection PRAGMAs the
/// TypeScript server uses (`database.ts`): WAL + `synchronous=NORMAL` + 5s busy timeout + a 64 MiB
/// page cache + foreign keys. Applying them via [`SqliteConnectOptions`] means every pooled
/// connection inherits them. In-memory databases cap the pool at one connection (each connection
/// would otherwise get its own private database) and skip WAL (unsupported for `:memory:`).
async fn open_pool(path: impl AsRef<Path>) -> Result<SqlitePool, DbError> {
    let path = path.as_ref();
    let memory = is_memory(path);
    let mut opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .busy_timeout(Duration::from_secs(5))
        .synchronous(SqliteSynchronous::Normal)
        .foreign_keys(true)
        .pragma("cache_size", "-64000");
    if !memory {
        opts = opts.journal_mode(SqliteJournalMode::Wal);
    }
    let mut pool_opts = SqlitePoolOptions::new();
    if memory {
        pool_opts = pool_opts.max_connections(1);
    }
    Ok(pool_opts.connect_with(opts).await?)
}

/// Create the append-only `event` / `event_sequence` tables if they don't already exist.
async fn ensure_event_schema(pool: &SqlitePool) -> Result<(), DbError> {
    for stmt in SCHEMA_DDL
        .split(';')
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        sqlx::query(stmt).execute(pool).await?;
    }
    Ok(())
}

/// The opencode database: a single, shared [`SqlitePool`] plus the repositories built over it.
///
/// `sqlx::SqlitePool` is internally reference-counted, so every [`SqlitePool::clone`] handed to a
/// repository shares the *same* underlying connection pool — satisfying the "one shared pool"
/// requirement. Construct once at boot via [`Database::connect`], verify the journal with
/// [`Database::verify_migrations`], then mint repositories ([`Database::event_store`],
/// [`Database::session_input`], [`Database::session_context_epoch`]).
#[derive(Clone)]
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Open (creating if missing) the database at `path`, apply the standard PRAGMAs, and ensure the
    /// event schema exists. Pass `":memory:"` for an ephemeral database (single connection).
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let pool = open_pool(path).await?;
        ensure_event_schema(&pool).await?;
        Ok(Self { pool })
    }

    /// The shared connection pool.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// An [`EventStore`] backed by the shared pool.
    pub fn event_store(&self) -> Arc<dyn EventStore> {
        Arc::new(SqlxEventStore::from_pool(self.pool.clone()))
    }

    /// A [`SessionStore`] (read model over the `session` projection table), backed by the shared pool.
    pub fn session_store(&self) -> Arc<dyn SessionStore> {
        Arc::new(SqlxSessionStore::new(self.pool.clone()))
    }

    /// The `session_input` repository, backed by the shared pool.
    pub fn session_input(&self) -> SessionInputRepo {
        SessionInputRepo::new(self.pool.clone())
    }

    /// The `session_context_epoch` repository, backed by the shared pool.
    pub fn session_context_epoch(&self) -> SessionContextEpochRepo {
        SessionContextEpochRepo::new(self.pool.clone())
    }

    /// Verify the migration journal against the migrations this build was compiled with, without
    /// applying anything (TS owns schema during the migration). See [`migration::verify`].
    pub async fn verify_migrations(&self) -> Result<MigrationReport, DbError> {
        migration::verify(&self.pool).await
    }
}

/// SQLite-backed [`EventStore`] (sqlx). Reuses the exact TS `event`/`event_sequence` DDL so a TS- or
/// Rust-written database is interchangeable. Prefer constructing it from a [`Database`] (shared pool)
/// via [`Database::event_store`]; [`SqlxEventStore::connect_path`] is a standalone convenience that
/// opens its own pool.
pub struct SqlxEventStore {
    pool: SqlitePool,
}

impl SqlxEventStore {
    /// Wrap an existing (shared) pool.
    pub fn from_pool(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Open a standalone database at `path` (own pool), ensuring the event schema exists.
    pub async fn connect_path(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let pool = open_pool(path).await?;
        ensure_event_schema(&pool).await?;
        Ok(Self::from_pool(pool))
    }
}

#[async_trait]
impl EventStore for SqlxEventStore {
    async fn append(
        &self,
        aggregate_id: &str,
        expected_head: i64,
        events: Vec<EventInput>,
    ) -> Result<i64, DbError> {
        let mut tx = self.pool.begin().await?;
        let found: i64 =
            sqlx::query_scalar("SELECT seq FROM event_sequence WHERE aggregate_id = ?")
                .bind(aggregate_id)
                .fetch_optional(&mut *tx)
                .await?
                .unwrap_or(0);
        if found != expected_head {
            return Err(DbError::Conflict {
                aggregate_id: aggregate_id.to_string(),
                expected: expected_head,
                found,
            });
        }
        let final_seq = found + events.len() as i64;
        // Upsert the aggregate's head first so the `event` rows' FK target (event_sequence) exists.
        sqlx::query(
            "INSERT INTO event_sequence (aggregate_id, seq) VALUES (?, ?) \
             ON CONFLICT(aggregate_id) DO UPDATE SET seq = excluded.seq",
        )
        .bind(aggregate_id)
        .bind(final_seq)
        .execute(&mut *tx)
        .await?;
        let mut seq = found;
        for ev in events {
            seq += 1;
            let id = format!("evt_{}", ulid::Ulid::new().to_string().to_lowercase());
            let data = serde_json::to_string(&ev.data)?;
            sqlx::query(
                "INSERT INTO event (id, aggregate_id, seq, \"type\", data) VALUES (?, ?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(aggregate_id)
            .bind(seq)
            .bind(&ev.kind)
            .bind(&data)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(final_seq)
    }

    async fn read(&self, aggregate_id: &str, from_seq: i64) -> Result<Vec<StoredEvent>, DbError> {
        let rows: Vec<(String, i64, String, String)> = sqlx::query_as(
            "SELECT id, seq, \"type\", data FROM event WHERE aggregate_id = ? AND seq > ? ORDER BY seq",
        )
        .bind(aggregate_id)
        .bind(from_seq)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|(id, seq, kind, data)| {
                Ok(StoredEvent {
                    id,
                    aggregate_id: aggregate_id.to_string(),
                    seq,
                    kind,
                    data: serde_json::from_str(&data)?,
                })
            })
            .collect()
    }

    async fn head_seq(&self, aggregate_id: &str) -> Result<i64, DbError> {
        Ok(
            sqlx::query_scalar("SELECT seq FROM event_sequence WHERE aggregate_id = ?")
                .bind(aggregate_id)
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or(0),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(kind: &str) -> EventInput {
        EventInput::new(kind, json!({}))
    }

    #[tokio::test]
    async fn append_assigns_sequential_seqs_and_reads_back() {
        let store = MemoryEventStore::new();
        assert_eq!(
            store
                .append("ses_1", 0, vec![ev("a"), ev("b")])
                .await
                .unwrap(),
            2
        );
        assert_eq!(store.append("ses_1", 2, vec![ev("c")]).await.unwrap(), 3);

        let all = store.read("ses_1", 0).await.unwrap();
        assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert!(all.iter().all(|e| e.id.starts_with("evt_")));

        let tail = store.read("ses_1", 2).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].kind, "c");
        assert_eq!(store.head_seq("ses_1").await.unwrap(), 3);
    }

    #[tokio::test]
    async fn append_with_stale_head_conflicts() {
        let store = MemoryEventStore::new();
        store.append("ses_1", 0, vec![ev("a")]).await.unwrap();
        let err = store.append("ses_1", 0, vec![ev("b")]).await.unwrap_err();
        assert!(matches!(
            err,
            DbError::Conflict {
                expected: 0,
                found: 1,
                ..
            }
        ));
        // The failed append did not mutate the log.
        assert_eq!(store.head_seq("ses_1").await.unwrap(), 1);
    }

    #[tokio::test]
    async fn unknown_aggregate_is_empty() {
        let store = MemoryEventStore::new();
        assert_eq!(store.head_seq("nope").await.unwrap(), 0);
        assert!(store.read("nope", 0).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sqlx_store_roundtrips_and_conflicts() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqlxEventStore::connect_path(dir.path().join("events.db"))
            .await
            .unwrap();

        assert_eq!(
            store
                .append("ses_1", 0, vec![ev("a"), ev("b")])
                .await
                .unwrap(),
            2
        );
        assert_eq!(store.append("ses_1", 2, vec![ev("c")]).await.unwrap(), 3);

        let all = store.read("ses_1", 0).await.unwrap();
        assert_eq!(all.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 2, 3]);
        assert_eq!(all[0].kind, "a");
        assert!(all.iter().all(|e| e.id.starts_with("evt_")));
        assert_eq!(store.read("ses_1", 2).await.unwrap().len(), 1);
        assert_eq!(store.head_seq("ses_1").await.unwrap(), 3);

        let err = store.append("ses_1", 0, vec![ev("x")]).await.unwrap_err();
        assert!(matches!(
            err,
            DbError::Conflict {
                found: 3,
                expected: 0,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn sqlx_store_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.db");
        {
            let store = SqlxEventStore::connect_path(&path).await.unwrap();
            store.append("ses_1", 0, vec![ev("a")]).await.unwrap();
        }
        // Reopen the same file — data persists on disk.
        let store = SqlxEventStore::connect_path(&path).await.unwrap();
        assert_eq!(store.head_seq("ses_1").await.unwrap(), 1);
        assert_eq!(store.read("ses_1", 0).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn database_shares_one_pool_across_repositories() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("opencode.db"))
            .await
            .unwrap();
        // The event store minted from the shared pool reads/writes the same database.
        let store = db.event_store();
        assert_eq!(store.append("ses_1", 0, vec![ev("a")]).await.unwrap(), 1);
        // A second event store over the same pool sees the first one's write.
        let store2 = db.event_store();
        assert_eq!(store2.head_seq("ses_1").await.unwrap(), 1);
    }

    /// The event store must read rows written by *another* writer (the TS server) — i.e. events
    /// inserted directly into the shared schema, not via our `append`. Proves on-disk compatibility.
    #[tokio::test]
    async fn event_store_reads_externally_written_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("opencode.db"))
            .await
            .unwrap();
        // Insert as the TS store would: the aggregate head, then the event rows with `type`/`data`.
        sqlx::query(
            "INSERT INTO event_sequence (aggregate_id, seq, owner_id) VALUES ('ses_x', 2, 'usr_1')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        for (id, seq, kind, data) in [
            ("evt_one", 1, "session.created.1", r#"{"title":"hi"}"#),
            ("evt_two", 2, "session.updated.1", r#"{"title":"bye"}"#),
        ] {
            sqlx::query(
                "INSERT INTO event (id, aggregate_id, seq, type, data) VALUES (?, 'ses_x', ?, ?, ?)",
            )
            .bind(id)
            .bind(seq)
            .bind(kind)
            .bind(data)
            .execute(db.pool())
            .await
            .unwrap();
        }

        let store = db.event_store();
        assert_eq!(store.head_seq("ses_x").await.unwrap(), 2);
        let events = store.read("ses_x", 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, "evt_one");
        assert_eq!(events[0].kind, "session.created.1");
        assert_eq!(events[0].data["title"], "hi");
        assert_eq!(events[1].seq, 2);
    }
}
