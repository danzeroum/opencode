//! Internal **event-store contract** — deliberately separate from `opencode-proto` (the external
//! HTTP contract) so the event schema can evolve without touching the public API.
//!
//! Ported from `packages/core/src/event.ts` + `event/sql.ts`: an append-only `event` table keyed
//! by a unique `(aggregate_id, seq)`, a versioned event registry (`type.N`), and explicit
//! upcasters between versions.

use serde::{Deserialize, Serialize};

/// A persisted event. `(aggregate_id, seq)` is unique — the basis of optimistic concurrency.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventEnvelope<T> {
    /// Aggregate this event belongs to (e.g. a session id).
    pub aggregate_id: String,
    /// Monotonic per-aggregate sequence number.
    pub seq: i64,
    /// Event type discriminator (the `type` column).
    #[serde(rename = "type")]
    pub kind: String,
    /// Schema version of `payload` (the `.N` in the TS `versionedType`).
    pub version: u32,
    /// The event payload.
    pub payload: T,
    /// When true, projectors skip non-replayable side effects. Ported verbatim from the TS
    /// `Payload.replay` flag so replay/rebuild behaves identically.
    #[serde(default)]
    pub replay: bool,
}

/// Upcasts an older event payload version to the current shape before a projector sees it.
///
/// Implementors live next to each `EventKind` variant; the registry chains them by `(kind, version)`.
pub trait Upcaster: Send + Sync {
    /// Upcast `payload` of `kind`@`version` to the latest version. Returns the migrated value.
    fn upcast(&self, kind: &str, version: u32, payload: serde_json::Value) -> serde_json::Value;
}

/// A new event to append. `aggregate_id`/`seq` are assigned by the store on append.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EventInput {
    /// Event type discriminator (the `type` column).
    #[serde(rename = "type")]
    pub kind: String,
    /// Schema version of `payload`.
    pub version: u32,
    /// The event payload.
    pub payload: serde_json::Value,
    /// Skip non-replayable side effects when applied during replay.
    #[serde(default)]
    pub replay: bool,
}

/// A persisted event with its assigned aggregate/seq and a JSON payload.
pub type StoredEvent = EventEnvelope<serde_json::Value>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_roundtrips_with_type_key() {
        let env = EventEnvelope {
            aggregate_id: "ses_123".into(),
            seq: 1,
            kind: "session.created".into(),
            version: 1,
            payload: serde_json::json!({ "title": "hello" }),
            replay: false,
        };
        let json = serde_json::to_value(&env).unwrap();
        assert_eq!(json["type"], "session.created");
        let back: EventEnvelope<serde_json::Value> = serde_json::from_value(json).unwrap();
        assert_eq!(back, env);
    }
}
