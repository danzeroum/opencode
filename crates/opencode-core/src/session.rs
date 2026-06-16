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

/// The static inputs of a session run plus its limits — the conversation itself is passed to [`run`].
pub struct Session {
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
    /// A session for `model` with a step limit and otherwise empty inputs.
    pub fn new(model: impl Into<String>, step_limit: usize) -> Self {
        Self {
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

/// Drive a session to completion: run turns while the model keeps issuing tool calls, stopping when a
/// turn finishes with none ([`TurnOutcome::Done`](crate::runner::TurnOutcome::Done)) or the step limit
/// is hit. `seed` is the initial conversation (e.g. the user's prompt).
pub async fn run(
    engine: &dyn LlmEngine,
    tools: Arc<dyn ToolBox>,
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

        // No tool calls ⇒ the model is done.
        if calls.is_empty() {
            return Ok(SessionRun {
                outcome: SessionOutcome::Completed { steps: step + 1 },
                messages,
                transcript,
                usage,
            });
        }

        // Run the tool calls and feed the results back as a `tool` message, then continue.
        let results = execute_tools(&tools, &calls).await?;
        messages.push(Message {
            role: Role::Tool,
            content: results,
        });
    }

    Ok(SessionRun {
        outcome: SessionOutcome::StepLimitReached {
            steps: session.step_limit,
        },
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

/// Run all of a turn's tool calls concurrently on the [`ToolExecutor`], returning their results as
/// `tool_result` content in the original call order. A panicking tool task aborts the run; a tool that
/// returns an error is fed back to the model as an error result.
async fn execute_tools(
    tools: &Arc<dyn ToolBox>,
    calls: &[ToolCallRecord],
) -> Result<Vec<ContentPart>, RunError> {
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

    Ok(calls
        .iter()
        .map(|call| ContentPart::ToolResult {
            id: call.id.clone(),
            name: call.name.clone(),
            result: Value::String(outputs.remove(&call.id).unwrap_or_default()),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
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
}
