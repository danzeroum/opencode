//! **Session-runner control-flow spike** (Phase 4 de-risking — the plan's #1 technical risk).
//!
//! This is *pure logic*: no IO, DB, HTTP, LLM, or route cutover. It exists to prove (or refute) that
//! the Effect-based control flow in `packages/core/src/session/runner/llm.ts` maps cleanly to
//! panic-free, explicit Rust. The hard patterns and their mappings:
//!
//! | TypeScript (Effect)                                   | Rust (here)                                  |
//! |-------------------------------------------------------|----------------------------------------------|
//! | `Effect.die(TurnTransitionError{_tag})` + `catchDefect` to restart a turn | [`TurnTransition`] returned as `Err` + the [`run_turn`] driver loop |
//! | `runTurn` (recover overflow once) vs `runAfterOverflowCompaction` | [`OverflowRecovery`] mode flipped `Enabled → Disabled` |
//! | "Post-compaction provider attempt cannot recover another overflow" | [`RunError::DoubleOverflow`] |
//! | `needsContinuation` boolean (more tool calls → another turn) | [`TurnOutcome::Continue`] / [`TurnOutcome::Done`] |
//! | outer continuation loop, step-limited                 | [`run_session`] |
//! | `FiberSet` of streaming tool fibers                   | [`ToolExecutor`] over a `tokio::task::JoinSet` |
//! | `raceFirst(FiberSet.join, FiberSet.awaitEmpty)`       | [`ToolExecutor::drain`] (fail-fast on first error, else all-settled) |
//! | `FiberSet.clear` on interrupt/failure                 | [`ToolExecutor::cancel_all`] (`abort_all`) |
//! | `Effect.uninterruptibleMask` around commit/tool spawn | ownership of the critical section (don't drop the guard mid-section) — a discipline, noted at call sites in later phases |
//!
//! The real runner is async; the *transition decisions* ([`run_turn`]/[`run_session`]) are pure and
//! synchronous here so each branch is exhaustively unit-tested. Only [`ToolExecutor`] is async.

use std::future::Future;

use tokio::task::JoinSet;

/// How a promoted [`SessionInput`](crate) was delivered (the TS `SessionInput.Delivery`). Carried by a
/// rebuild transition so the restarted turn re-promotes the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promotion {
    /// A "steer" mid-run injection.
    Steer,
    /// A queued follow-up prompt.
    Queue,
}

/// The result of a single turn attempt — the explicit form of the TS `needsContinuation` boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnOutcome {
    /// The turn issued tool calls (or otherwise needs continuation) → run another turn.
    Continue,
    /// The turn finished with nothing left to do.
    Done,
}

/// A restart signal — the explicit form of the TS `TurnTransitionError` thrown via `Effect.die` and
/// caught by `catchDefect`. Returned as the `Err` of a turn attempt; [`run_turn`] interprets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnTransition {
    /// Preparation observed a concurrent change (or agent/model mismatch, or compaction) — restart the
    /// turn from durable state, re-promoting `promotion` if set.
    RebuildPreparedTurn {
        /// The promotion to re-apply on the rebuilt turn.
        promotion: Option<Promotion>,
    },
    /// Overflow compaction completed — restart once through the path that no longer recovers overflow.
    ContinueAfterOverflowCompaction,
}

/// Whether the current attempt may recover a context-overflow by compacting (the `runTurn` path) or
/// must not (the `runAfterOverflowCompaction` path, entered after a prior overflow recovery).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverflowRecovery {
    /// Overflow recovery is allowed (initial `runTurn`).
    Enabled,
    /// Overflow recovery already happened once; another overflow is fatal.
    Disabled,
}

/// The error of a single turn attempt: either a restart [`TurnTransition`] or a fatal failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnAttemptError {
    /// Restart the turn per the transition.
    Transition(TurnTransition),
    /// A non-recoverable failure (provider/tool/internal) — aborts the run.
    Fatal(String),
}

/// A terminal run error.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    /// A second overflow after a post-compaction attempt (TS: "Post-compaction provider attempt
    /// cannot recover another overflow").
    #[error("post-compaction attempt cannot recover another overflow")]
    DoubleOverflow,
    /// A turn attempt failed fatally.
    #[error("turn attempt failed: {0}")]
    Attempt(String),
}

/// Drive one turn to a [`TurnOutcome`], applying the restart semantics of `runTurn` /
/// `runAfterOverflowCompaction`. `attempt(recovery)` runs one attempt under the given overflow mode.
///
/// - `Ok(outcome)` → done, return it.
/// - `Err(Transition(RebuildPreparedTurn))` → retry in the **same** mode.
/// - `Err(Transition(ContinueAfterOverflowCompaction))` → switch `Enabled → Disabled` and retry; a
///   second one (already `Disabled`) is [`RunError::DoubleOverflow`].
/// - `Err(Fatal)` → [`RunError::Attempt`].
pub fn run_turn<F>(mut attempt: F) -> Result<TurnOutcome, RunError>
where
    F: FnMut(OverflowRecovery) -> Result<TurnOutcome, TurnAttemptError>,
{
    let mut recovery = OverflowRecovery::Enabled;
    loop {
        match attempt(recovery) {
            Ok(outcome) => return Ok(outcome),
            Err(TurnAttemptError::Fatal(message)) => return Err(RunError::Attempt(message)),
            Err(TurnAttemptError::Transition(TurnTransition::RebuildPreparedTurn { .. })) => {
                // Retry from durable state in the same overflow-recovery mode.
            }
            Err(TurnAttemptError::Transition(TurnTransition::ContinueAfterOverflowCompaction)) => {
                if recovery == OverflowRecovery::Disabled {
                    return Err(RunError::DoubleOverflow);
                }
                recovery = OverflowRecovery::Disabled;
            }
        }
    }
}

/// The outcome of an entire session run (the outer continuation loop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOutcome {
    /// The run completed after `steps` turns (the last turn was [`TurnOutcome::Done`]).
    Completed {
        /// Number of turns executed.
        steps: usize,
    },
    /// The step limit was hit before completion (every turn returned [`TurnOutcome::Continue`]).
    StepLimitReached {
        /// The step limit that was reached.
        steps: usize,
    },
}

/// The outer, step-limited continuation loop: run turns while each returns [`TurnOutcome::Continue`],
/// stopping at [`TurnOutcome::Done`] or when `step_limit` turns have run. `turn(step)` runs one turn
/// (typically built on [`run_turn`]).
pub fn run_session<F>(step_limit: usize, mut turn: F) -> Result<SessionOutcome, RunError>
where
    F: FnMut(usize) -> Result<TurnOutcome, RunError>,
{
    for step in 0..step_limit {
        match turn(step)? {
            TurnOutcome::Continue => continue,
            TurnOutcome::Done => return Ok(SessionOutcome::Completed { steps: step + 1 }),
        }
    }
    Ok(SessionOutcome::StepLimitReached { steps: step_limit })
}

/// Why a tool call did not produce a successful result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolError {
    /// The tool ran and failed.
    Failed(String),
    /// The tool exceeded its deadline.
    Timeout,
    /// The tool was cancelled (the set was cleared).
    Cancelled,
}

/// The settled result of one tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// The tool-call id.
    pub id: String,
    /// `Ok(output)` on success, else why it didn't settle.
    pub outcome: Result<String, ToolError>,
}

/// What [`ToolExecutor::drain`] observed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DrainOutcome {
    /// Every spawned tool completed successfully.
    AllSettled(Vec<ToolResult>),
    /// A tool failed; remaining tools are still pending (the caller should [`ToolExecutor::cancel_all`]).
    Failed(ToolResult),
}

/// Explicit lifecycle for streaming tool calls — the Rust analog of the runner's `FiberSet`.
///
/// Tools are spawned as they stream in ([`spawn`](ToolExecutor::spawn)); [`drain`](ToolExecutor::drain)
/// is `raceFirst(join, awaitEmpty)` (return on the first failure, else when all have settled); and
/// [`cancel_all`](ToolExecutor::cancel_all) is `FiberSet.clear` (abort the rest on interrupt/failure).
pub struct ToolExecutor {
    set: JoinSet<ToolResult>,
}

impl Default for ToolExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolExecutor {
    /// Create an empty executor.
    pub fn new() -> Self {
        Self {
            set: JoinSet::new(),
        }
    }

    /// Spawn a tool call. `fut` resolves to the tool's output (or a [`ToolError`]).
    pub fn spawn<F>(&mut self, id: impl Into<String>, fut: F)
    where
        F: Future<Output = Result<String, ToolError>> + Send + 'static,
    {
        let id = id.into();
        self.set.spawn(async move {
            let outcome = fut.await;
            ToolResult { id, outcome }
        });
    }

    /// Number of tools still running.
    pub fn pending(&self) -> usize {
        self.set.len()
    }

    /// Whether no tools are running.
    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    /// Drain settled tools, returning on the **first failure** (leaving the rest pending) or once
    /// **all** have settled successfully — `raceFirst(FiberSet.join, FiberSet.awaitEmpty)`.
    pub async fn drain(&mut self) -> DrainOutcome {
        let mut settled = Vec::new();
        while let Some(joined) = self.set.join_next().await {
            match joined {
                Ok(result) if result.outcome.is_err() => return DrainOutcome::Failed(result),
                Ok(result) => settled.push(result),
                // A cancelled task (from `cancel_all`) is skipped; a panic surfaces as a failure.
                Err(join_err) if join_err.is_cancelled() => continue,
                Err(join_err) => {
                    return DrainOutcome::Failed(ToolResult {
                        id: "<panic>".to_string(),
                        outcome: Err(ToolError::Failed(join_err.to_string())),
                    })
                }
            }
        }
        DrainOutcome::AllSettled(settled)
    }

    /// Cancel all pending tools (`FiberSet.clear`).
    pub fn cancel_all(&mut self) {
        self.set.abort_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    // ---- run_turn (the die/catchDefect restart driver) ----

    #[test]
    fn run_turn_returns_outcome_directly() {
        let outcome = run_turn(|_recovery| Ok(TurnOutcome::Done)).unwrap();
        assert_eq!(outcome, TurnOutcome::Done);
    }

    #[test]
    fn rebuild_prepared_turn_retries_same_mode() {
        let mut calls = 0;
        let outcome = run_turn(|recovery| {
            calls += 1;
            assert_eq!(
                recovery,
                OverflowRecovery::Enabled,
                "rebuild keeps the mode"
            );
            if calls == 1 {
                Err(TurnAttemptError::Transition(
                    TurnTransition::RebuildPreparedTurn {
                        promotion: Some(Promotion::Steer),
                    },
                ))
            } else {
                Ok(TurnOutcome::Continue)
            }
        })
        .unwrap();
        assert_eq!(outcome, TurnOutcome::Continue);
        assert_eq!(calls, 2);
    }

    #[test]
    fn overflow_switches_to_disabled_then_succeeds() {
        let mut modes = Vec::new();
        let outcome = run_turn(|recovery| {
            modes.push(recovery);
            if recovery == OverflowRecovery::Enabled {
                Err(TurnAttemptError::Transition(
                    TurnTransition::ContinueAfterOverflowCompaction,
                ))
            } else {
                Ok(TurnOutcome::Done)
            }
        })
        .unwrap();
        assert_eq!(outcome, TurnOutcome::Done);
        assert_eq!(
            modes,
            vec![OverflowRecovery::Enabled, OverflowRecovery::Disabled]
        );
    }

    #[test]
    fn second_overflow_is_double_overflow_error() {
        let err = run_turn(|_recovery| {
            Err(TurnAttemptError::Transition(
                TurnTransition::ContinueAfterOverflowCompaction,
            ))
        })
        .unwrap_err();
        assert_eq!(err, RunError::DoubleOverflow);
    }

    #[test]
    fn fatal_attempt_propagates() {
        let err = run_turn(|_recovery| Err(TurnAttemptError::Fatal("boom".into()))).unwrap_err();
        assert_eq!(err, RunError::Attempt("boom".to_string()));
    }

    // ---- run_session (the outer continuation loop) ----

    #[test]
    fn session_runs_until_done() {
        // Simulate a turn that streams text + a tool call (Continue), then a final turn (Done).
        let script = [
            TurnOutcome::Continue,
            TurnOutcome::Continue,
            TurnOutcome::Done,
        ];
        let outcome = run_session(10, |step| Ok(script[step])).unwrap();
        assert_eq!(outcome, SessionOutcome::Completed { steps: 3 });
    }

    #[test]
    fn session_stops_at_step_limit() {
        let outcome = run_session(2, |_step| Ok(TurnOutcome::Continue)).unwrap();
        assert_eq!(outcome, SessionOutcome::StepLimitReached { steps: 2 });
    }

    #[test]
    fn session_propagates_turn_error() {
        let err = run_session(5, |_step| Err(RunError::DoubleOverflow)).unwrap_err();
        assert_eq!(err, RunError::DoubleOverflow);
    }

    #[test]
    fn full_turn_simulation_via_run_turn() {
        // A turn that internally rebuilds once, then continues; next turn is done.
        let outcome = run_session(10, |step| {
            run_turn(|_recovery| match step {
                0 => Ok(TurnOutcome::Continue), // streamed a tool call → continue
                _ => Ok(TurnOutcome::Done),
            })
        })
        .unwrap();
        assert_eq!(outcome, SessionOutcome::Completed { steps: 2 });
    }

    // ---- ToolExecutor (the FiberSet analog) ----

    #[tokio::test]
    async fn all_tools_settle() {
        let mut exec = ToolExecutor::new();
        for i in 0..3 {
            exec.spawn(format!("t{i}"), async move { Ok(format!("out{i}")) });
        }
        match exec.drain().await {
            DrainOutcome::AllSettled(mut results) => {
                results.sort_by(|a, b| a.id.cmp(&b.id));
                assert_eq!(results.len(), 3);
                assert_eq!(results[0].outcome, Ok("out0".to_string()));
            }
            other => panic!("expected AllSettled, got {other:?}"),
        }
        assert!(exec.is_empty());
    }

    #[tokio::test]
    async fn first_failure_short_circuits_and_leaves_rest_pending() {
        let mut exec = ToolExecutor::new();
        // An immediate failure and a slow success: drain must return the failure fast.
        exec.spawn("fail", async { Err(ToolError::Failed("nope".into())) });
        exec.spawn("slow", async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok("late".to_string())
        });
        match exec.drain().await {
            DrainOutcome::Failed(result) => {
                assert_eq!(result.id, "fail");
                assert_eq!(result.outcome, Err(ToolError::Failed("nope".into())));
            }
            other => panic!("expected Failed, got {other:?}"),
        }
        // The slow tool is still pending; the caller clears it (interrupt semantics).
        assert_eq!(exec.pending(), 1);
        exec.cancel_all();
    }

    #[tokio::test]
    async fn cancel_all_aborts_pending_without_hanging() {
        let mut exec = ToolExecutor::new();
        exec.spawn("slow", async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok("late".to_string())
        });
        exec.cancel_all();
        // Draining an aborted set must not hang and yields no successful results.
        assert_eq!(exec.drain().await, DrainOutcome::AllSettled(vec![]));
    }

    #[tokio::test]
    async fn timeout_maps_to_tool_error() {
        let mut exec = ToolExecutor::new();
        exec.spawn("slow", async {
            // The caller wraps the tool in a deadline; elapsed → ToolError::Timeout.
            match tokio::time::timeout(
                Duration::from_millis(10),
                tokio::time::sleep(Duration::from_secs(30)),
            )
            .await
            {
                Ok(()) => Ok("done".to_string()),
                Err(_) => Err(ToolError::Timeout),
            }
        });
        match exec.drain().await {
            DrainOutcome::Failed(result) => assert_eq!(result.outcome, Err(ToolError::Timeout)),
            other => panic!("expected Failed(Timeout), got {other:?}"),
        }
    }
}
