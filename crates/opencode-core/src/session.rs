//! Minimal **session runner** (Phase 4, first end-to-end slice) — wires the pure [`runner`](crate::runner)
//! control-flow primitives into a real async turn loop over the LLM + tools.
//!
//! Per the migration plan this starts with the smallest real subset: **one provider at a time, one
//! tool, no plugins, no permissions** — proving the turn loop end-to-end before layering on overflow
//! recovery, persistence, the event bus, and the `/session` route cutover.
//!
//! The loop is the async analog of [`run_session`](crate::runner::run_session): each turn lowers the
//! conversation to an [`LlmRequest`], calls the [`LlmEngine`] (one provider round-trip → normalized
//! [`LlmEvent`]s), folds the events into the assistant's message (text + tool calls + usage), and — if
//! the model issued tool calls — runs them on the spike's [`ToolExecutor`](crate::runner::ToolExecutor)
//! and feeds the results back as a `tool` message before the next turn. No tool calls ⇒ the turn is
//! [`TurnOutcome::Done`](crate::runner::TurnOutcome). Overflow recovery (the `run_turn` transitions) and
//! durable persistence land once compaction and the event store are ported.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use opencode_db::{DbError, EventStore};
use opencode_effect::{BusEvent, EventBus};
use opencode_events::EventInput;
use opencode_llm::{
    ContentPart, Generation, LlmError, LlmEvent, LlmRequest, Message, Role, ToolChoice,
    ToolDefinition, Usage,
};
use serde_json::Value;

use crate::runner::{DrainOutcome, RunError, SessionOutcome, ToolExecutor};

/// One provider round-trip: lower the request, send it, decode the stream into normalized events. The
/// real impl wraps [`opencode_llm::transport::complete`] for a concrete
/// [`Protocol`](opencode_llm::Protocol); tests script it.
#[async_trait]
pub trait LlmEngine: Send + Sync {
    /// Run one completion, returning the normalized event stream for the turn.
    async fn complete(&self, request: &LlmRequest) -> Result<Vec<LlmEvent>, LlmError>;
}

/// Executes a tool call by name. A tool-level failure is returned as `Err(message)` and is **fed back
/// to the model** as an error result (it does not abort the run); only an infrastructure failure (a
/// panicking tool task) aborts. One tool, no plugins, for now.
#[async_trait]
pub trait ToolBox: Send + Sync {
    /// Execute the named tool with `input`, returning its textual result (`Ok`) or an error fed back to
    /// the model (`Err`).
    async fn invoke(&self, name: &str, input: Value) -> Result<String, String>;
}

/// A permission decision for a tool call — the `permission`/`question` gate's verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Run the tool.
    Allow,
    /// Refuse to run it; `reason` is fed back to the model as the tool's (error) result.
    Deny(String),
    /// Suspend the run pending a user decision — records a `permission.requested` event and returns
    /// [`SessionOutcome::AwaitingPermission`](crate::runner::SessionOutcome::AwaitingPermission).
    Ask,
}

/// Decides whether a tool call may run, consulted by the runner **before** it executes the call — the
/// seam for the `permission`/`question` flow. The default ([`AllowAll`]) permits everything; a real impl
/// consults the permissions store (a later increment).
#[async_trait]
pub trait PermissionGate: Send + Sync {
    /// Decide whether `tool` (with `input`) may run in `session_id`.
    async fn check(&self, session_id: &str, tool: &str, input: &Value) -> Decision;
}

/// A gate that permits every tool call — preserves pre-gate behavior; the default for [`run`] /
/// [`run_with_sink`].
pub struct AllowAll;

#[async_trait]
impl PermissionGate for AllowAll {
    async fn check(&self, _session_id: &str, _tool: &str, _input: &Value) -> Decision {
        Decision::Allow
    }
}

/// The static inputs of a session run plus its limits — the conversation itself is passed to [`run`].
pub struct Session {
    /// Session id — the event-store aggregate and the subject of permission checks.
    pub id: String,
    /// Model id sent on every turn.
    pub model: String,
    /// System prompt parts.
    pub system: Vec<String>,
    /// Tools offered to the model each turn (their JSON schemas).
    pub tools: Vec<ToolDefinition>,
    /// Generation parameters.
    pub generation: Generation,
    /// Maximum number of turns before stopping (the continuation step limit).
    pub step_limit: usize,
}

impl Session {
    /// A session for `model` with a step limit and otherwise empty inputs (empty id).
    pub fn new(model: impl Into<String>, step_limit: usize) -> Self {
        Self {
            id: String::new(),
            model: model.into(),
            system: Vec::new(),
            tools: Vec::new(),
            generation: Generation::default(),
            step_limit,
        }
    }
}

/// What a finished [`run`] produced.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRun {
    /// Why the run stopped (completed vs step-limit reached).
    pub outcome: SessionOutcome,
    /// The full conversation after the run: the seed messages plus each turn's assistant (and tool)
    /// messages.
    pub messages: Vec<Message>,
    /// Every normalized event emitted across all turns, in order (the run transcript).
    pub transcript: Vec<LlmEvent>,
    /// Summed token usage across turns.
    pub usage: Usage,
}

/// One tool call the model requested this turn.
struct ToolCallRecord {
    id: String,
    name: String,
    input: Value,
}

/// Drive a session to completion without persisting (ephemeral run): run turns while the model keeps
/// issuing tool calls, stopping when a turn finishes with none
/// ([`TurnOutcome::Done`](crate::runner::TurnOutcome::Done)) or the step limit is hit. `seed` is the
/// initial conversation (e.g. the user's prompt). Equivalent to [`run_with_sink`] with a [`NoopSink`].
pub async fn run(
    engine: &dyn LlmEngine,
    tools: Arc<dyn ToolBox>,
    session: &Session,
    seed: Vec<Message>,
) -> Result<SessionRun, RunError> {
    run_with_sink(engine, tools, &NoopSink, session, seed).await
}

/// Like [`run`], but streams each turn's events — and the final outcome — to `sink` as durable batches,
/// one [`SessionSink::record`] call per atomic turn. Persistence is a sink ([`EventStoreSink`]); the
/// event bus is another. The returned [`SessionRun`] is unchanged; the sink is a side channel.
/// Equivalent to [`run_gated`] with an [`AllowAll`] gate.
pub async fn run_with_sink(
    engine: &dyn LlmEngine,
    tools: Arc<dyn ToolBox>,
    sink: &dyn SessionSink,
    session: &Session,
    seed: Vec<Message>,
) -> Result<SessionRun, RunError> {
    run_gated(engine, tools, sink, &AllowAll, session, seed).await
}

/// Like [`run_with_sink`], but consults `gate` before executing each tool call (the
/// `permission`/`question` flow). [`Decision::Allow`] runs the tool; [`Decision::Deny`] skips it and
/// feeds the reason back to the model as an error result (recording a `permission.denied` event);
/// [`Decision::Ask`] suspends the run, recording a `permission.requested` event and returning
/// [`SessionOutcome::AwaitingPermission`](crate::runner::SessionOutcome::AwaitingPermission) (the resume
/// path is a later increment). Every decision is audited through `sink`.
pub async fn run_gated(
    engine: &dyn LlmEngine,
    tools: Arc<dyn ToolBox>,
    sink: &dyn SessionSink,
    gate: &dyn PermissionGate,
    session: &Session,
    seed: Vec<Message>,
) -> Result<SessionRun, RunError> {
    let mut messages = seed;
    let mut transcript = Vec::new();
    let mut usage = Usage::default();

    for step in 0..session.step_limit {
        let request = LlmRequest {
            model: session.model.clone(),
            system: session.system.clone(),
            messages: messages.clone(),
            tools: session.tools.clone(),
            // The model decides; "none" is expressed by offering no tools.
            tool_choice: (!session.tools.is_empty()).then_some(ToolChoice::Auto),
            generation: session.generation.clone(),
        };

        let events = engine
            .complete(&request)
            .await
            .map_err(|e| RunError::Attempt(e.to_string()))?;

        let (text, calls) = fold_turn(&events, &mut usage);
        transcript.extend(events);

        // The durable record of this turn (assistant message; tool results appended below).
        let assistant = assistant_event(&text, &calls);

        // Record the assistant turn (text first, then any tool calls), mirroring the message order the
        // provider streamed.
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(ContentPart::Text(text));
        }
        for call in &calls {
            content.push(ContentPart::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                input: call.input.clone(),
            });
        }
        messages.push(Message {
            role: Role::Assistant,
            content,
        });

        // No tool calls ⇒ the model is done: persist the turn, then the outcome.
        if calls.is_empty() {
            sink.record(vec![assistant]).await?;
            let outcome = SessionOutcome::Completed { steps: step + 1 };
            sink.record(vec![finished_event(&outcome, &usage)]).await?;
            return Ok(SessionRun {
                outcome,
                messages,
                transcript,
                usage,
            });
        }

        // Gate each tool call before running it (permission / question flow).
        let mut decisions = Vec::with_capacity(calls.len());
        for call in &calls {
            decisions.push(gate.check(&session.id, &call.name, &call.input).await);
        }

        // `Ask` suspends the run: record the request and yield, awaiting a user decision.
        if decisions.contains(&Decision::Ask) {
            let requested: Vec<Value> = calls
                .iter()
                .zip(&decisions)
                .filter(|(_, d)| **d == Decision::Ask)
                .map(|(c, _)| serde_json::json!({ "id": c.id, "name": c.name, "input": c.input }))
                .collect();
            sink.record(vec![
                assistant,
                EventInput::new(
                    event_kinds::PERMISSION_REQUESTED,
                    serde_json::json!({ "calls": requested }),
                ),
            ])
            .await?;
            return Ok(SessionRun {
                outcome: SessionOutcome::AwaitingPermission { steps: step + 1 },
                messages,
                transcript,
                usage,
            });
        }

        // Execute the allowed calls (denied ones are not run).
        let allowed: Vec<&ToolCallRecord> = calls
            .iter()
            .zip(&decisions)
            .filter(|(_, d)| **d == Decision::Allow)
            .map(|(call, _)| call)
            .collect();
        let outputs = run_tools(&tools, &allowed).await?;

        // The turn's atomic batch: the assistant message, a `permission.denied` event per denied call,
        // then the tool results (executed output for allowed calls, the denial reason for denied ones —
        // fed back so the model sees what happened).
        let mut batch = vec![assistant];
        let mut results = Vec::with_capacity(calls.len());
        for (call, decision) in calls.iter().zip(&decisions) {
            let result = match decision {
                Decision::Deny(reason) => {
                    batch.push(EventInput::new(
                        event_kinds::PERMISSION_DENIED,
                        serde_json::json!({ "id": call.id, "name": call.name, "reason": reason }),
                    ));
                    format!("Permission denied: {reason}")
                }
                // `Allow` (`Ask` is handled above).
                _ => outputs.get(&call.id).cloned().unwrap_or_default(),
            };
            results.push(ContentPart::ToolResult {
                id: call.id.clone(),
                name: call.name.clone(),
                result: Value::String(result),
            });
        }
        batch.push(tool_results_event(&results));
        sink.record(batch).await?;
        messages.push(Message {
            role: Role::Tool,
            content: results,
        });
    }

    let outcome = SessionOutcome::StepLimitReached {
        steps: session.step_limit,
    };
    sink.record(vec![finished_event(&outcome, &usage)]).await?;
    Ok(SessionRun {
        outcome,
        messages,
        transcript,
        usage,
    })
}

/// Fold a turn's normalized events into its assistant text + tool calls, accumulating usage from the
/// terminal `Finish`.
fn fold_turn(events: &[LlmEvent], usage: &mut Usage) -> (String, Vec<ToolCallRecord>) {
    let mut text = String::new();
    let mut calls = Vec::new();
    for event in events {
        match event {
            LlmEvent::TextDelta { text: delta, .. } => text.push_str(delta),
            LlmEvent::ToolCall { id, name, input } => calls.push(ToolCallRecord {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            }),
            LlmEvent::Finish {
                usage: Some(turn), ..
            } => {
                usage.input += turn.input;
                usage.output += turn.output;
                usage.cache_read += turn.cache_read;
                usage.cache_write += turn.cache_write;
            }
            _ => {}
        }
    }
    (text, calls)
}

/// Run `calls` concurrently on the [`ToolExecutor`], returning each call's output keyed by tool-call id.
/// A panicking tool task aborts the run; a tool that returns an error yields its message as the output
/// (fed back to the model, not fatal).
async fn run_tools(
    tools: &Arc<dyn ToolBox>,
    calls: &[&ToolCallRecord],
) -> Result<HashMap<String, String>, RunError> {
    let mut exec = ToolExecutor::new();
    for call in calls {
        let tools = tools.clone();
        let name = call.name.clone();
        let input = call.input.clone();
        exec.spawn(call.id.clone(), async move {
            // Tool-level errors settle as `Ok` here (they are fed back to the model); the executor's
            // error channel is reserved for infrastructure failures (a panicking task).
            Ok(match tools.invoke(&name, input).await {
                Ok(output) => output,
                Err(message) => format!("Tool error: {message}"),
            })
        });
    }

    let mut outputs: HashMap<String, String> = HashMap::new();
    match exec.drain().await {
        DrainOutcome::AllSettled(results) => {
            for result in results {
                if let Ok(output) = result.outcome {
                    outputs.insert(result.id, output);
                }
            }
        }
        DrainOutcome::Failed(result) => {
            exec.cancel_all();
            return Err(RunError::Attempt(format!(
                "tool task failed: {:?}",
                result.outcome
            )));
        }
    }
    Ok(outputs)
}

// ---- Durable persistence (the runner as an event producer) ----

/// The event types the runner appends, versioned `name.N` (the TS `versionedType` convention).
pub mod event_kinds {
    /// An assistant turn — `data`: `{ "text": string, "tool_calls": [{id,name,input}] }`.
    pub const ASSISTANT_MESSAGE: &str = "message.assistant.1";
    /// A turn's tool results — `data`: `{ "results": [{id,name,result}] }`.
    pub const TOOL_RESULTS: &str = "message.tool_results.1";
    /// A tool call the gate refused — `data`: `{ "id", "name", "reason" }`.
    pub const PERMISSION_DENIED: &str = "permission.denied.1";
    /// Tool calls awaiting a user decision — `data`: `{ "calls": [{id,name,input}] }`.
    pub const PERMISSION_REQUESTED: &str = "permission.requested.1";
    /// The run outcome — `data`: `{ "outcome": "completed"|"step_limit", "steps": n, "usage": {…} }`.
    pub const SESSION_FINISHED: &str = "session.finished.1";
}

fn assistant_event(text: &str, calls: &[ToolCallRecord]) -> EventInput {
    let tool_calls: Vec<Value> = calls
        .iter()
        .map(|c| serde_json::json!({ "id": c.id, "name": c.name, "input": c.input }))
        .collect();
    EventInput::new(
        event_kinds::ASSISTANT_MESSAGE,
        serde_json::json!({ "text": text, "tool_calls": tool_calls }),
    )
}

fn tool_results_event(results: &[ContentPart]) -> EventInput {
    let items: Vec<Value> = results
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolResult { id, name, result } => {
                Some(serde_json::json!({ "id": id, "name": name, "result": result }))
            }
            _ => None,
        })
        .collect();
    EventInput::new(
        event_kinds::TOOL_RESULTS,
        serde_json::json!({ "results": items }),
    )
}

fn finished_event(outcome: &SessionOutcome, usage: &Usage) -> EventInput {
    let (label, steps) = match outcome {
        SessionOutcome::Completed { steps } => ("completed", *steps),
        SessionOutcome::StepLimitReached { steps } => ("step_limit", *steps),
        // A suspended run records `permission.requested` instead of a finish; this arm only keeps the
        // match exhaustive.
        SessionOutcome::AwaitingPermission { steps } => ("awaiting_permission", *steps),
    };
    EventInput::new(
        event_kinds::SESSION_FINISHED,
        serde_json::json!({ "outcome": label, "steps": steps, "usage": usage }),
    )
}

/// Receives the runner's durable events as a run progresses — one [`record`](SessionSink::record) call
/// per atomic batch (each turn's events, then the outcome). The seam that decouples the turn loop from
/// where events go: [`EventStoreSink`] persists them; an event-bus sink (Phase 4) will publish them.
#[async_trait]
pub trait SessionSink: Send + Sync {
    /// Persist (or otherwise handle) one atomic batch of events. Returning `Err` aborts the run.
    async fn record(&self, events: Vec<EventInput>) -> Result<(), RunError>;
}

/// A sink that discards events — the default for ephemeral [`run`]s.
pub struct NoopSink;

#[async_trait]
impl SessionSink for NoopSink {
    async fn record(&self, _events: Vec<EventInput>) -> Result<(), RunError> {
        Ok(())
    }
}

/// A [`SessionSink`] that appends each batch to an [`EventStore`] under the session's aggregate id,
/// advancing the optimistic-concurrency head across batches. A conflict (another writer advanced the
/// aggregate) surfaces as a [`RunError`] — concurrent-writer handling (steering / compaction restart)
/// is a later increment; here the runner is the sole writer.
pub struct EventStoreSink {
    store: Arc<dyn EventStore>,
    aggregate_id: String,
    head: std::sync::Mutex<i64>,
}

impl EventStoreSink {
    /// Build a sink for `aggregate_id`, seeding the head from the store's current head so appends
    /// continue an existing log.
    pub async fn new(
        store: Arc<dyn EventStore>,
        aggregate_id: impl Into<String>,
    ) -> Result<Self, DbError> {
        let aggregate_id = aggregate_id.into();
        let head = store.head_seq(&aggregate_id).await?;
        Ok(Self {
            store,
            aggregate_id,
            head: std::sync::Mutex::new(head),
        })
    }
}

#[async_trait]
impl SessionSink for EventStoreSink {
    async fn record(&self, events: Vec<EventInput>) -> Result<(), RunError> {
        if events.is_empty() {
            return Ok(());
        }
        // The runner records sequentially, so the head is read and updated around the append without
        // holding the lock across the await.
        let expected = *self.head.lock().expect("event sink mutex poisoned");
        let new_head = self
            .store
            .append(&self.aggregate_id, expected, events)
            .await
            .map_err(|e| RunError::Attempt(format!("persist: {e}")))?;
        *self.head.lock().expect("event sink mutex poisoned") = new_head;
        Ok(())
    }
}

/// A [`SessionSink`] that publishes each event to the in-process [`EventBus`] (scoped to the session's
/// aggregate), so live consumers — e.g. the SSE `/event` stream — see the run as it happens. Publishing
/// is fire-and-forget (the bus drops for lagging subscribers rather than blocking the runner), so this
/// sink never aborts a run.
pub struct BusSink {
    bus: Arc<EventBus>,
    aggregate_id: String,
}

impl BusSink {
    /// A bus sink that scopes published events to `aggregate_id`.
    pub fn new(bus: Arc<EventBus>, aggregate_id: impl Into<String>) -> Self {
        Self {
            bus,
            aggregate_id: aggregate_id.into(),
        }
    }
}

#[async_trait]
impl SessionSink for BusSink {
    async fn record(&self, events: Vec<EventInput>) -> Result<(), RunError> {
        for event in events {
            self.bus.publish(BusEvent::for_aggregate(
                event.kind,
                self.aggregate_id.as_str(),
                event.data,
            ));
        }
        Ok(())
    }
}

/// Fan one batch out to two sinks: a `primary` (authoritative — its failure aborts the run) followed by
/// a best-effort `secondary` (its failure is swallowed). The runner uses this to **persist then
/// announce**: an [`EventStoreSink`] primary + a [`BusSink`] secondary, so nothing is announced on the
/// bus that failed to persist, and a stalled bus never aborts a run.
pub struct FanOutSink<A: SessionSink, B: SessionSink> {
    primary: A,
    secondary: B,
}

impl<A: SessionSink, B: SessionSink> FanOutSink<A, B> {
    /// Fan out to `primary` (authoritative) then `secondary` (best-effort).
    pub fn new(primary: A, secondary: B) -> Self {
        Self { primary, secondary }
    }
}

#[async_trait]
impl<A: SessionSink, B: SessionSink> SessionSink for FanOutSink<A, B> {
    async fn record(&self, events: Vec<EventInput>) -> Result<(), RunError> {
        // Persist first; a primary failure aborts before anything is announced.
        self.primary.record(events.clone()).await?;
        // Then announce best-effort; a secondary failure does not abort the run.
        let _ = self.secondary.record(events).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode_db::MemoryEventStore;
    use opencode_llm::{FinishReason, Message};
    use serde_json::json;
    use std::sync::Mutex;

    /// An engine that returns a scripted event stream per turn (front of the queue), for deterministic
    /// loop tests.
    struct ScriptedEngine {
        turns: Mutex<std::collections::VecDeque<Vec<LlmEvent>>>,
    }

    impl ScriptedEngine {
        fn new(turns: Vec<Vec<LlmEvent>>) -> Self {
            Self {
                turns: Mutex::new(turns.into()),
            }
        }
    }

    #[async_trait]
    impl LlmEngine for ScriptedEngine {
        async fn complete(&self, _request: &LlmRequest) -> Result<Vec<LlmEvent>, LlmError> {
            Ok(self.turns.lock().unwrap().pop_front().unwrap_or_default())
        }
    }

    /// A toolbox with a single `get_weather` tool; any other name is a tool-level error.
    struct WeatherTools;

    #[async_trait]
    impl ToolBox for WeatherTools {
        async fn invoke(&self, name: &str, input: Value) -> Result<String, String> {
            match name {
                "get_weather" => {
                    let city = input.get("city").and_then(Value::as_str).unwrap_or("?");
                    Ok(
                        json!({ "city": city, "temperature": 22, "condition": "sunny" })
                            .to_string(),
                    )
                }
                other => Err(format!("unknown tool: {other}")),
            }
        }
    }

    fn text_turn(text: &str, reason: FinishReason, usage: Usage) -> Vec<LlmEvent> {
        vec![
            LlmEvent::TextStart { id: "t".into() },
            LlmEvent::TextDelta {
                id: "t".into(),
                text: text.into(),
            },
            LlmEvent::TextEnd { id: "t".into() },
            LlmEvent::Finish {
                reason,
                usage: Some(usage),
            },
        ]
    }

    fn tool_turn(id: &str, name: &str, input: Value, usage: Usage) -> Vec<LlmEvent> {
        vec![
            LlmEvent::ToolInputStart {
                id: id.into(),
                name: name.into(),
            },
            LlmEvent::ToolInputEnd { id: id.into() },
            LlmEvent::ToolCall {
                id: id.into(),
                name: name.into(),
                input,
            },
            LlmEvent::Finish {
                reason: FinishReason::ToolCalls,
                usage: Some(usage),
            },
        ]
    }

    fn usage(input: u64, output: u64) -> Usage {
        Usage {
            input,
            output,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn runs_a_tool_loop_to_completion() {
        // Turn 0 calls the tool; turn 1 answers in text.
        let engine = ScriptedEngine::new(vec![
            tool_turn(
                "call_1",
                "get_weather",
                json!({ "city": "Paris" }),
                usage(10, 5),
            ),
            text_turn(
                "It's sunny and 22°C in Paris.",
                FinishReason::Stop,
                usage(30, 8),
            ),
        ]);
        let mut session = Session::new("claude-haiku-4-5-20251001", 8);
        session.tools = vec![ToolDefinition {
            name: "get_weather".into(),
            description: Some("Get current weather.".into()),
            input_schema: json!({ "type": "object" }),
        }];
        let run = run(
            &engine,
            Arc::new(WeatherTools),
            &session,
            vec![Message::user_text("What's the weather in Paris?")],
        )
        .await
        .unwrap();

        assert_eq!(run.outcome, SessionOutcome::Completed { steps: 2 });
        // user, assistant(tool-call), tool(result), assistant(text)
        assert_eq!(run.messages.len(), 4);
        assert_eq!(run.messages[1].role, Role::Assistant);
        assert!(matches!(
            run.messages[1].content[0],
            ContentPart::ToolCall { .. }
        ));
        // The tool result fed back carries the executed tool's output.
        match &run.messages[2].content[0] {
            ContentPart::ToolResult { id, result, .. } => {
                assert_eq!(id, "call_1");
                assert!(result.as_str().unwrap().contains("sunny"));
            }
            other => panic!("expected tool result, got {other:?}"),
        }
        assert_eq!(
            run.messages[3].content[0],
            ContentPart::Text("It's sunny and 22°C in Paris.".into())
        );
        // Usage is summed across both turns.
        assert_eq!(run.usage, usage(40, 13));
    }

    #[tokio::test]
    async fn finishes_immediately_when_no_tool_calls() {
        let engine =
            ScriptedEngine::new(vec![text_turn("Hello!", FinishReason::Stop, usage(5, 2))]);
        let session = Session::new("claude-haiku-4-5-20251001", 8);
        let run = run(
            &engine,
            Arc::new(WeatherTools),
            &session,
            vec![Message::user_text("Hi")],
        )
        .await
        .unwrap();
        assert_eq!(run.outcome, SessionOutcome::Completed { steps: 1 });
        assert_eq!(run.messages.len(), 2);
        assert_eq!(run.usage, usage(5, 2));
    }

    #[tokio::test]
    async fn tool_error_is_fed_back_not_fatal() {
        // The model calls a tool that errors, then (seeing the error) answers in text.
        let engine = ScriptedEngine::new(vec![
            tool_turn("call_1", "missing_tool", json!({}), usage(10, 5)),
            text_turn("Sorry, I can't do that.", FinishReason::Stop, usage(20, 4)),
        ]);
        let mut session = Session::new("claude-haiku-4-5-20251001", 8);
        session.tools = vec![ToolDefinition {
            name: "get_weather".into(),
            description: None,
            input_schema: json!({ "type": "object" }),
        }];
        let run = run(
            &engine,
            Arc::new(WeatherTools),
            &session,
            vec![Message::user_text("do it")],
        )
        .await
        .unwrap();
        assert_eq!(run.outcome, SessionOutcome::Completed { steps: 2 });
        match &run.messages[2].content[0] {
            ContentPart::ToolResult { result, .. } => {
                assert!(result
                    .as_str()
                    .unwrap()
                    .contains("Tool error: unknown tool"));
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn step_limit_stops_a_runaway_tool_loop() {
        // The model calls the tool every turn and never stops.
        let engine = ScriptedEngine::new(vec![
            tool_turn("a", "get_weather", json!({ "city": "A" }), usage(1, 1)),
            tool_turn("b", "get_weather", json!({ "city": "B" }), usage(1, 1)),
            tool_turn("c", "get_weather", json!({ "city": "C" }), usage(1, 1)),
        ]);
        let mut session = Session::new("claude-haiku-4-5-20251001", 2);
        session.tools = vec![ToolDefinition {
            name: "get_weather".into(),
            description: None,
            input_schema: json!({ "type": "object" }),
        }];
        let run = run(
            &engine,
            Arc::new(WeatherTools),
            &session,
            vec![Message::user_text("loop")],
        )
        .await
        .unwrap();
        assert_eq!(run.outcome, SessionOutcome::StepLimitReached { steps: 2 });
    }

    // ---- End-to-end over the real HTTP transport (in-test axum server, no network/TLS) ----

    /// An [`LlmEngine`] backed by the real `anthropic-messages` transport against `url`.
    struct HttpAnthropic {
        client: reqwest::Client,
        url: String,
    }

    #[async_trait]
    impl LlmEngine for HttpAnthropic {
        async fn complete(&self, request: &LlmRequest) -> Result<Vec<LlmEvent>, LlmError> {
            opencode_llm::transport::complete(
                &self.client,
                &self.url,
                &[("x-api-key", "test"), ("anthropic-version", "2023-06-01")],
                &opencode_llm::anthropic::AnthropicMessages,
                request,
            )
            .await
        }
    }

    const TOOL_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    const TEXT_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"It is sunny in Paris.\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":20,\"output_tokens\":7}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    #[tokio::test]
    async fn end_to_end_tool_loop_over_http_transport() {
        use axum::{routing::post, Router};
        use std::sync::atomic::{AtomicU32, Ordering};

        // The server streams a tool call on the first turn, then a text answer on the second.
        let calls = Arc::new(AtomicU32::new(0));
        let counter = calls.clone();
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let counter = counter.clone();
                async move {
                    let body = if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                        TOOL_SSE
                    } else {
                        TEXT_SSE
                    };
                    ([("content-type", "text/event-stream")], body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let engine = HttpAnthropic {
            client: reqwest::Client::new(),
            url: format!("http://{addr}/v1/messages"),
        };
        let mut session = Session::new("claude-haiku-4-5-20251001", 8);
        session.tools = vec![ToolDefinition {
            name: "get_weather".into(),
            description: Some("Get current weather.".into()),
            input_schema: json!({ "type": "object" }),
        }];

        let run = run(
            &engine,
            Arc::new(WeatherTools),
            &session,
            vec![Message::user_text("What's the weather in Paris?")],
        )
        .await
        .unwrap();

        assert_eq!(run.outcome, SessionOutcome::Completed { steps: 2 });
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        // The decoded tool call drove a real tool execution, fed back into the second turn.
        match &run.messages[2].content[0] {
            ContentPart::ToolResult { id, result, .. } => {
                assert_eq!(id, "toolu_1");
                assert!(result.as_str().unwrap().contains("Paris"));
            }
            other => panic!("expected tool result, got {other:?}"),
        }
        let answer: String = run
            .transcript
            .iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(answer.contains("sunny"));
        // Usage summed across both real turns (10/5 + 20/7).
        assert_eq!(run.usage, usage(30, 12));
    }

    // ---- Durable persistence ----

    #[tokio::test]
    async fn persists_turn_events_to_the_store() {
        let engine = ScriptedEngine::new(vec![
            tool_turn(
                "call_1",
                "get_weather",
                json!({ "city": "Paris" }),
                usage(10, 5),
            ),
            text_turn("It's sunny.", FinishReason::Stop, usage(30, 8)),
        ]);
        let mut session = Session::new("claude-haiku-4-5-20251001", 8);
        session.tools = vec![ToolDefinition {
            name: "get_weather".into(),
            description: None,
            input_schema: json!({ "type": "object" }),
        }];
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());
        let sink = EventStoreSink::new(store.clone(), "ses_persist")
            .await
            .unwrap();

        let run = run_with_sink(
            &engine,
            Arc::new(WeatherTools),
            &sink,
            &session,
            vec![Message::user_text("weather?")],
        )
        .await
        .unwrap();
        assert_eq!(run.outcome, SessionOutcome::Completed { steps: 2 });

        let stored = store.read("ses_persist", 0).await.unwrap();
        let kinds: Vec<&str> = stored.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                event_kinds::ASSISTANT_MESSAGE, // turn 0: assistant (tool call)
                event_kinds::TOOL_RESULTS,      // turn 0: tool results
                event_kinds::ASSISTANT_MESSAGE, // turn 1: assistant (text)
                event_kinds::SESSION_FINISHED,  // outcome
            ]
        );
        // Sequence numbers are monotonic from 1.
        assert_eq!(
            stored.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
        // The first assistant event carries the tool call; the tool-results event carries the output.
        assert_eq!(stored[0].data["tool_calls"][0]["name"], "get_weather");
        assert!(stored[1].data["results"][0]["result"]
            .as_str()
            .unwrap()
            .contains("sunny"));
        // The outcome event snapshots the summed usage + step count.
        assert_eq!(stored[3].data["outcome"], "completed");
        assert_eq!(stored[3].data["steps"], 2);
        assert_eq!(stored[3].data["usage"]["input"], 40);
        assert_eq!(stored[3].data["usage"]["output"], 13);
    }

    #[tokio::test]
    async fn continues_an_existing_event_log() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());
        // A pre-existing event (e.g. `session.created`) already occupies seq 1.
        store
            .append(
                "ses_resume",
                0,
                vec![EventInput::new("session.created.1", json!({}))],
            )
            .await
            .unwrap();

        let engine = ScriptedEngine::new(vec![text_turn("Hi", FinishReason::Stop, usage(5, 2))]);
        let session = Session::new("claude-haiku-4-5-20251001", 8);
        let sink = EventStoreSink::new(store.clone(), "ses_resume")
            .await
            .unwrap();
        run_with_sink(
            &engine,
            Arc::new(WeatherTools),
            &sink,
            &session,
            vec![Message::user_text("hi")],
        )
        .await
        .unwrap();

        // The run's events were appended after the existing one — seqs continue, no conflict.
        let stored = store.read("ses_resume", 0).await.unwrap();
        let kinds: Vec<&str> = stored.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "session.created.1",
                event_kinds::ASSISTANT_MESSAGE,
                event_kinds::SESSION_FINISHED,
            ]
        );
        assert_eq!(
            stored.iter().map(|e| e.seq).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[tokio::test]
    async fn sink_surfaces_a_concurrent_write_conflict() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());
        let sink = EventStoreSink::new(store.clone(), "ses_conflict")
            .await
            .unwrap(); // seeds head = 0
                       // A concurrent writer advances the aggregate after the sink seeded its head.
        store
            .append(
                "ses_conflict",
                0,
                vec![EventInput::new("other.1", json!({}))],
            )
            .await
            .unwrap();
        // The sink still expects head 0, so its append conflicts.
        let err = sink
            .record(vec![EventInput::new(
                event_kinds::ASSISTANT_MESSAGE,
                json!({}),
            )])
            .await
            .unwrap_err();
        match err {
            RunError::Attempt(message) => {
                assert!(message.contains("conflict"), "got {message}")
            }
            other => panic!("expected a persist conflict, got {other:?}"),
        }
    }

    // ---- Event-bus production (fan-out) ----

    #[tokio::test]
    async fn fans_out_to_store_and_bus() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe(); // subscribe before producing
        let sink = FanOutSink::new(
            EventStoreSink::new(store.clone(), "ses_fan").await.unwrap(),
            BusSink::new(bus.clone(), "ses_fan"),
        );
        let engine = ScriptedEngine::new(vec![text_turn("Hi", FinishReason::Stop, usage(5, 2))]);
        let session = Session::new("claude-haiku-4-5-20251001", 8);
        run_with_sink(
            &engine,
            Arc::new(WeatherTools),
            &sink,
            &session,
            vec![Message::user_text("hi")],
        )
        .await
        .unwrap();

        // The store persisted the run.
        let stored = store.read("ses_fan", 0).await.unwrap();
        assert_eq!(
            stored.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>(),
            vec![
                event_kinds::ASSISTANT_MESSAGE,
                event_kinds::SESSION_FINISHED
            ]
        );

        // The bus announced the same events, in order, scoped to the session.
        let mut published = Vec::new();
        while let Ok(event) = rx.try_recv() {
            assert_eq!(event.aggregate_id.as_deref(), Some("ses_fan"));
            published.push(event.kind);
        }
        assert_eq!(
            published,
            vec![
                event_kinds::ASSISTANT_MESSAGE.to_string(),
                event_kinds::SESSION_FINISHED.to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn store_failure_aborts_before_bus_publish() {
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());
        let bus = Arc::new(EventBus::new());
        let mut rx = bus.subscribe();
        let sink = FanOutSink::new(
            EventStoreSink::new(store.clone(), "ses_abort")
                .await
                .unwrap(),
            BusSink::new(bus.clone(), "ses_abort"),
        );
        // A concurrent writer advances the aggregate so the store leg conflicts.
        store
            .append("ses_abort", 0, vec![EventInput::new("other.1", json!({}))])
            .await
            .unwrap();
        let err = sink
            .record(vec![EventInput::new(
                event_kinds::ASSISTANT_MESSAGE,
                json!({}),
            )])
            .await
            .unwrap_err();
        assert!(matches!(err, RunError::Attempt(m) if m.contains("conflict")));
        // Nothing was announced on the bus (persist-then-announce ordering).
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn secondary_failure_is_soft() {
        struct FailingSink;
        #[async_trait]
        impl SessionSink for FailingSink {
            async fn record(&self, _events: Vec<EventInput>) -> Result<(), RunError> {
                Err(RunError::Attempt("secondary boom".into()))
            }
        }
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());
        let sink = FanOutSink::new(
            EventStoreSink::new(store.clone(), "ses_soft")
                .await
                .unwrap(),
            FailingSink,
        );
        // The secondary errors, but the batch still succeeds and the primary persisted it.
        sink.record(vec![EventInput::new(
            event_kinds::ASSISTANT_MESSAGE,
            json!({}),
        )])
        .await
        .unwrap();
        assert_eq!(store.read("ses_soft", 0).await.unwrap().len(), 1);
    }

    // ---- Permission / question gating ----

    struct FixedGate(Decision);

    #[async_trait]
    impl PermissionGate for FixedGate {
        async fn check(&self, _session_id: &str, _tool: &str, _input: &Value) -> Decision {
            self.0.clone()
        }
    }

    fn weather_session() -> Session {
        let mut session = Session::new("claude-haiku-4-5-20251001", 8);
        session.id = "ses_gate".into();
        session.tools = vec![ToolDefinition {
            name: "get_weather".into(),
            description: None,
            input_schema: json!({ "type": "object" }),
        }];
        session
    }

    #[tokio::test]
    async fn allow_runs_the_tool() {
        let engine = ScriptedEngine::new(vec![
            tool_turn(
                "call_1",
                "get_weather",
                json!({ "city": "Paris" }),
                usage(10, 5),
            ),
            text_turn("It's sunny.", FinishReason::Stop, usage(20, 4)),
        ]);
        let run = run_gated(
            &engine,
            Arc::new(WeatherTools),
            &NoopSink,
            &FixedGate(Decision::Allow),
            &weather_session(),
            vec![Message::user_text("weather?")],
        )
        .await
        .unwrap();
        assert_eq!(run.outcome, SessionOutcome::Completed { steps: 2 });
        // The tool ran: its executed output is fed back (not a denial).
        match &run.messages[2].content[0] {
            ContentPart::ToolResult { result, .. } => {
                assert!(result.as_str().unwrap().contains("sunny"))
            }
            other => panic!("expected tool result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn deny_skips_the_tool_and_audits() {
        let engine = ScriptedEngine::new(vec![
            tool_turn(
                "call_1",
                "get_weather",
                json!({ "city": "Paris" }),
                usage(10, 5),
            ),
            text_turn("Understood.", FinishReason::Stop, usage(20, 4)),
        ]);
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());
        let sink = EventStoreSink::new(store.clone(), "ses_gate")
            .await
            .unwrap();
        let run = run_gated(
            &engine,
            Arc::new(WeatherTools),
            &sink,
            &FixedGate(Decision::Deny("not allowed".into())),
            &weather_session(),
            vec![Message::user_text("weather?")],
        )
        .await
        .unwrap();
        assert_eq!(run.outcome, SessionOutcome::Completed { steps: 2 });
        // The model sees the denial instead of an executed result.
        match &run.messages[2].content[0] {
            ContentPart::ToolResult { result, .. } => {
                assert_eq!(result.as_str().unwrap(), "Permission denied: not allowed")
            }
            other => panic!("expected tool result, got {other:?}"),
        }
        // The denial is audited in the event log.
        let stored = store.read("ses_gate", 0).await.unwrap();
        assert!(
            stored
                .iter()
                .any(|e| e.kind == event_kinds::PERMISSION_DENIED
                    && e.data["reason"] == "not allowed")
        );
    }

    #[tokio::test]
    async fn ask_suspends_and_records_request() {
        // Turn 1 must NOT be consumed — the run suspends after turn 0's tool call.
        let engine = ScriptedEngine::new(vec![
            tool_turn(
                "call_1",
                "get_weather",
                json!({ "city": "Paris" }),
                usage(10, 5),
            ),
            text_turn("should not run", FinishReason::Stop, usage(99, 99)),
        ]);
        let store: Arc<dyn EventStore> = Arc::new(MemoryEventStore::new());
        let sink = EventStoreSink::new(store.clone(), "ses_gate")
            .await
            .unwrap();
        let run = run_gated(
            &engine,
            Arc::new(WeatherTools),
            &sink,
            &FixedGate(Decision::Ask),
            &weather_session(),
            vec![Message::user_text("weather?")],
        )
        .await
        .unwrap();
        assert_eq!(run.outcome, SessionOutcome::AwaitingPermission { steps: 1 });
        // No tool message was produced (the tool did not run); the conversation ends at the assistant
        // turn that requested the call.
        assert_eq!(run.messages.last().unwrap().role, Role::Assistant);
        // The pending request is recorded; no `session.finished` (the run is suspended, not finished).
        let stored = store.read("ses_gate", 0).await.unwrap();
        let kinds: Vec<&str> = stored.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                event_kinds::ASSISTANT_MESSAGE,
                event_kinds::PERMISSION_REQUESTED
            ]
        );
        assert_eq!(stored[1].data["calls"][0]["name"], "get_weather");
        // Only turn 0 was consumed, so usage reflects just that turn.
        assert_eq!(run.usage, usage(10, 5));
    }
}
