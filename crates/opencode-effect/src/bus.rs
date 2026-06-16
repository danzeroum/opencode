//! In-process **event bus** — the Rust analog of the TS `event.ts` PubSub (`PubSub.unbounded` global
//! stream + `PubSub.sliding(1)` per-aggregate notifications).
//!
//! - **Global stream:** an [`async_broadcast`] channel. Unlike `tokio::sync::broadcast` (which drops
//!   for slow consumers via `Lagged` and would corrupt projectors), async-broadcast supports
//!   backpressure; here the global stream is configured *overflow* (drop-oldest) so a stalled SSE
//!   client can never block producers — fire-and-forget semantics, matching the plan's note that SSE
//!   `/event` is the lossy consumer. A dedicated lossless (overflow-off) subscription for projectors
//!   lands with the projector wiring (Phase 4).
//! - **Per-aggregate notifications:** [`tokio::sync::watch`] (sliding-1 "latest value" — a bump
//!   counter), so a watcher always sees that the aggregate changed without buffering every event.
//!
//! During TS↔Rust coexistence this bus has **no producers** (the TS runner produces events in its own
//! process), so it is *infrastructure*: it is wired into `AppContext` and exposed via an internal SSE
//! route, but the contract `/event` stays proxied to TS until the runner is in Rust.

use std::collections::HashMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::watch;

/// An event carried on the [`EventBus`].
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BusEvent {
    /// Event type (e.g. `"session.created"`).
    #[serde(rename = "type")]
    pub kind: String,
    /// The aggregate this event belongs to (e.g. a session id), if any — drives per-aggregate watch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aggregate_id: Option<String>,
    /// Event payload.
    pub data: Value,
}

impl BusEvent {
    /// A global event with no aggregate.
    pub fn new(kind: impl Into<String>, data: Value) -> Self {
        Self {
            kind: kind.into(),
            aggregate_id: None,
            data,
        }
    }

    /// An event scoped to `aggregate_id` (also bumps that aggregate's watch on publish).
    pub fn for_aggregate(
        kind: impl Into<String>,
        aggregate_id: impl Into<String>,
        data: Value,
    ) -> Self {
        Self {
            kind: kind.into(),
            aggregate_id: Some(aggregate_id.into()),
            data,
        }
    }
}

/// A subscription to the global event stream — a `Stream` of [`BusEvent`].
pub type EventReceiver = async_broadcast::Receiver<BusEvent>;

/// Default global-stream capacity (events buffered before the oldest is dropped for a lagging reader).
const DEFAULT_CAPACITY: usize = 256;

/// The in-process event bus (see the module docs).
pub struct EventBus {
    global: async_broadcast::Sender<BusEvent>,
    // An inactive receiver keeps the channel open even when there are no active subscribers, so
    // `publish` never fails with "closed".
    _keepalive: async_broadcast::InactiveReceiver<BusEvent>,
    aggregates: Mutex<HashMap<String, watch::Sender<u64>>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }
}

impl EventBus {
    /// Create a bus with the default capacity.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a bus whose global stream buffers up to `capacity` events per subscriber.
    pub fn with_capacity(capacity: usize) -> Self {
        let (mut tx, rx) = async_broadcast::broadcast(capacity.max(1));
        // Drop the oldest event for a lagging subscriber rather than blocking producers.
        tx.set_overflow(true);
        Self {
            global: tx,
            _keepalive: rx.deactivate(),
            aggregates: Mutex::new(HashMap::new()),
        }
    }

    /// Subscribe to the global event stream. A new subscriber sees only events published *after* it
    /// subscribes.
    pub fn subscribe(&self) -> EventReceiver {
        self.global.new_receiver()
    }

    /// Publish an event to the global stream (and bump its aggregate's watch, if scoped).
    pub fn publish(&self, event: BusEvent) {
        if let Some(aggregate_id) = event.aggregate_id.clone() {
            self.notify_aggregate(&aggregate_id);
        }
        // overflow=true → never blocks or errors on a full buffer; the keepalive prevents "closed".
        let _ = self.global.try_broadcast(event);
    }

    /// A sliding-1 notification handle for `aggregate_id`: the receiver observes a monotonically
    /// bumped counter (only the latest value, coalescing rapid bumps).
    pub fn watch_aggregate(&self, aggregate_id: &str) -> watch::Receiver<u64> {
        self.aggregate_sender(aggregate_id).subscribe()
    }

    fn notify_aggregate(&self, aggregate_id: &str) {
        self.aggregate_sender(aggregate_id)
            .send_modify(|v| *v = v.wrapping_add(1));
    }

    fn aggregate_sender(&self, aggregate_id: &str) -> watch::Sender<u64> {
        self.aggregates
            .lock()
            .expect("event bus mutex poisoned")
            .entry(aggregate_id.to_string())
            .or_insert_with(|| watch::channel(0u64).0)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn publish_reaches_all_subscribers() {
        let bus = EventBus::new();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();
        bus.publish(BusEvent::new("session.created", json!({ "id": "ses_1" })));
        let e1 = rx1.recv().await.unwrap();
        let e2 = rx2.recv().await.unwrap();
        assert_eq!(e1.kind, "session.created");
        assert_eq!(e1.data["id"], "ses_1");
        assert_eq!(e2, e1);
    }

    #[tokio::test]
    async fn subscriber_only_sees_events_after_subscribing() {
        let bus = EventBus::new();
        bus.publish(BusEvent::new("early", json!(null))); // no active subscriber → not delivered
        let mut rx = bus.subscribe();
        bus.publish(BusEvent::new("late", json!(null)));
        assert_eq!(rx.recv().await.unwrap().kind, "late");
    }

    #[tokio::test]
    async fn events_are_delivered_in_order() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        for i in 0..5 {
            bus.publish(BusEvent::new(format!("e{i}"), json!(i)));
        }
        for i in 0..5 {
            assert_eq!(rx.recv().await.unwrap().kind, format!("e{i}"));
        }
    }

    #[tokio::test]
    async fn watch_aggregate_observes_changes_sliding() {
        let bus = EventBus::new();
        let mut w = bus.watch_aggregate("ses_1");
        // A different aggregate's events don't bump this watcher.
        bus.publish(BusEvent::for_aggregate("x", "ses_other", json!(null)));
        bus.publish(BusEvent::for_aggregate("a", "ses_1", json!(null)));
        w.changed().await.unwrap();
        assert_eq!(*w.borrow_and_update(), 1);
        // Rapid bumps coalesce (sliding-1): the watcher sees the latest counter, not each step.
        bus.publish(BusEvent::for_aggregate("b", "ses_1", json!(null)));
        bus.publish(BusEvent::for_aggregate("c", "ses_1", json!(null)));
        w.changed().await.unwrap();
        assert_eq!(*w.borrow_and_update(), 3);
    }
}
