//! Internal **event-store contract** — separate from `opencode-proto` (the external HTTP contract)
//! so the event schema can evolve independently.
//!
//! Mirrors `packages/core/src/event/sql.ts`: the `event` table is `(id, aggregate_id, seq, type,
//! data)` with a unique `(aggregate_id, seq)` index (the basis of optimistic concurrency). The
//! schema version is embedded in `type` (the TS `versionedType` `name.N` convention); the `replay`
//! flag is runtime-only and is not persisted.

use serde::{Deserialize, Serialize};

/// A new event to append. `id` / `aggregate_id` / `seq` are assigned by the store on append.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventInput {
    /// Event type (the `type` column; encodes the schema version as `name.N`).
    #[serde(rename = "type")]
    pub kind: String,
    /// The event payload (the `data` JSON column).
    pub data: serde_json::Value,
}

impl EventInput {
    /// Convenience constructor.
    pub fn new(kind: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            kind: kind.into(),
            data,
        }
    }
}

/// A persisted event row — matches the `event` table columns exactly for on-disk compatibility with
/// the TypeScript store. `(aggregate_id, seq)` is unique.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredEvent {
    /// Event id (ULID-based, e.g. `evt_…`).
    pub id: String,
    /// Aggregate this event belongs to (e.g. a session id).
    pub aggregate_id: String,
    /// Monotonic per-aggregate sequence number.
    pub seq: i64,
    /// Event type (the `type` column).
    #[serde(rename = "type")]
    pub kind: String,
    /// The event payload (the `data` JSON column).
    pub data: serde_json::Value,
}

/// Upcasts an older event payload to the current shape before a projector sees it. The registry
/// chains upcasters by event `type`, whose `.N` suffix encodes the version.
pub trait Upcaster: Send + Sync {
    /// Upcast `data` of event `kind` (which encodes its version) to the latest shape.
    fn upcast(&self, kind: &str, data: serde_json::Value) -> serde_json::Value;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_event_roundtrips_with_type_key() {
        let ev = StoredEvent {
            id: "evt_1".into(),
            aggregate_id: "ses_123".into(),
            seq: 1,
            kind: "session.created.1".into(),
            data: serde_json::json!({ "title": "hello" }),
        };
        let json = serde_json::to_value(&ev).unwrap();
        assert_eq!(json["type"], "session.created.1");
        assert_eq!(json["id"], "evt_1");
        let back: StoredEvent = serde_json::from_value(json).unwrap();
        assert_eq!(back, ev);
    }
}
