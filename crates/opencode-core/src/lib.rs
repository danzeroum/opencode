//! Domain core: sessions, event sourcing, permissions, agents, projects, catalog, background jobs.
//!
//! Ported across Phases 2/4 from `packages/core`. The session runner (`session/runner/llm.ts`) is
//! the hardest target: `die`/`catchDefect` control flow becomes an explicit `TurnOutcome` enum and
//! `FiberSet` becomes an explicit `ToolExecutor` (see plan §(a)/§(c)). This is a Phase 0 placeholder
//! re-exporting the shared context.

pub use opencode_effect::{AppContext, AppError};

/// Outcome of one turn of the agent loop. Replaces the Effect `die(TurnTransitionError)` +
/// `catchDefect` control-flow pattern with an explicit, panic-free state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// Continue to the next turn normally.
    Continue,
    /// Rebuild the prepared turn (e.g. agent/model changed mid-run) and retry.
    RebuildPreparedTurn,
    /// Continue after an overflow compaction started a new context epoch.
    ContinueAfterOverflow,
    /// The run is finished.
    Done,
}

/// Folds events into read-model state — the projector pattern (`session/projector.ts`). Implementors
/// derive aggregate state from the event log; `replay`-flagged events skip non-replayable side
/// effects (the impl decides), while pure projection just folds.
pub trait Projector {
    /// The read-model state this projector builds.
    type State;
    /// Apply one event to `state`.
    fn apply(&self, state: &mut Self::State, event: &opencode_events::StoredEvent);
}

/// Fold `events` into `initial` using `projector` (replay / rebuild from the event log).
pub fn project<P: Projector>(
    projector: &P,
    mut initial: P::State,
    events: &[opencode_events::StoredEvent],
) -> P::State {
    for event in events {
        projector.apply(&mut initial, event);
    }
    initial
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode_events::StoredEvent;

    /// Toy projector: counts events and concatenates their `type`s.
    struct Counter;
    #[derive(Default, PartialEq, Debug)]
    struct CountState {
        count: usize,
        kinds: Vec<String>,
    }
    impl Projector for Counter {
        type State = CountState;
        fn apply(&self, state: &mut CountState, event: &StoredEvent) {
            state.count += 1;
            state.kinds.push(event.kind.clone());
        }
    }

    fn stored(seq: i64, kind: &str) -> StoredEvent {
        StoredEvent {
            id: format!("evt_{seq}"),
            aggregate_id: "ses_1".to_string(),
            seq,
            kind: kind.to_string(),
            data: serde_json::Value::Null,
        }
    }

    #[test]
    fn project_folds_events_in_order() {
        let events = vec![
            stored(1, "created"),
            stored(2, "renamed"),
            stored(3, "closed"),
        ];
        let state = project(&Counter, CountState::default(), &events);
        assert_eq!(state.count, 3);
        assert_eq!(state.kinds, vec!["created", "renamed", "closed"]);
    }
}
