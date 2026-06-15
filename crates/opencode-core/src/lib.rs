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
