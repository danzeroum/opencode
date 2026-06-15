//! Persistence layer (Phase 2): event-store / session-store repositories.
//!
//! The trait shapes are fixed here with an in-memory implementation (reference + test double). The
//! sqlx/SQLite-backed implementation — reusing the exact TS DDL/indexes and the migration-journal
//! compat — lands as a follow-up (it needs the C `libsqlite3-sys`, deferred to keep early CI and
//! cross-compile simple).

use std::collections::HashMap;
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
}
