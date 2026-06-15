//! Persistence layer (Phase 2): a `sqlx` SQLite pool, a custom migration runner that preserves the
//! TS migration journal (for on-disk compatibility with existing user databases), and the
//! event-store / session-store repositories.
//!
//! This is a Phase 0 placeholder that fixes the key trait shapes.

#![allow(dead_code)]

use async_trait::async_trait;

/// Database / event-store errors.
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    /// Optimistic-concurrency conflict: the unique `(aggregate_id, seq)` index rejected the write.
    /// Maps to the TS `ConflictError`.
    #[error("optimistic concurrency conflict on aggregate {aggregate_id} at seq {seq}")]
    Conflict { aggregate_id: String, seq: i64 },
    /// Any other persistence error.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Append-only event store. The concrete impl reuses the exact DDL/indexes from
/// `packages/core/src/event/sql.ts` so existing SQLite databases keep working.
#[async_trait]
pub trait EventStore: Send + Sync {
    /// Append events for an aggregate inside a transaction that bumps `event_sequence` and relies on
    /// the unique `(aggregate_id, seq)` index for optimistic concurrency. Returns the new head seq.
    async fn append(
        &self,
        aggregate_id: &str,
        events: Vec<serde_json::Value>,
    ) -> Result<i64, DbError>;
}
