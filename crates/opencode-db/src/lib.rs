//! Persistence layer (Phase 2): event-store / session-store repositories.
//!
//! The trait shapes are fixed here with an in-memory implementation (reference + test double). The
//! sqlx/SQLite-backed implementation — reusing the exact TS DDL/indexes and the migration-journal
//! compat — lands as a follow-up (it needs the C `libsqlite3-sys`, deferred to keep early CI and
//! cross-compile simple).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use opencode_events::{EventInput, StoredEvent};

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
    logs: Mutex<HashMap<String, Vec<StoredEvent>>>,
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

/// SQLite-backed [`EventStore`] (sqlx). Reuses the exact TS `event`/`event_sequence` DDL so a TS- or
/// Rust-written database is interchangeable. During TS↔Rust coexistence TS owns schema evolution;
/// [`SqlxEventStore::connect_path`] only creates tables that don't already exist (the TS-migrates /
/// Rust-verifies stance — full migration-apply is deferred to Phase 6).
pub struct SqlxEventStore {
    pool: sqlx::SqlitePool,
}

impl SqlxEventStore {
    /// Open (creating if missing) a SQLite database at `path`, enabling WAL + foreign keys and
    /// ensuring the event schema exists.
    pub async fn connect_path(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let opts = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .busy_timeout(std::time::Duration::from_secs(5));
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect_with(opts)
            .await?;
        let store = Self { pool };
        store.ensure_schema().await?;
        Ok(store)
    }

    async fn ensure_schema(&self) -> Result<(), DbError> {
        // WAL + foreign keys, matching the TS store (many readers + one writer).
        sqlx::query("PRAGMA journal_mode=WAL;")
            .execute(&self.pool)
            .await?;
        sqlx::query("PRAGMA foreign_keys=ON;")
            .execute(&self.pool)
            .await?;
        for stmt in SCHEMA_DDL
            .split(';')
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            sqlx::query(stmt).execute(&self.pool).await?;
        }
        Ok(())
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
}
