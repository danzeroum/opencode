//! OpenAI Responses protocol — ported from `packages/llm/src/protocols/openai-responses.ts`.
//!
//! The Responses API (distinct from Chat Completions — see [`openai_chat`](crate::openai_chat)) streams
//! over **text SSE** like the others, so it reuses [`sse_frames`](crate::sse_frames)/[`decode_sse`].
//! What differs is the *shape*: the request is an `input` array of typed items (not chat `messages`),
//! tools are flat (`{type:"function", name, …}`, not nested under `function`), tool calls and tool
//! results are **top-level** `function_call` / `function_call_output` items (not message content), and
//! the stream is a sequence of typed lifecycle events (`response.output_text.delta`,
//! `response.function_call_arguments.delta`, `response.output_item.{added,done}`, `response.completed`)
//! rather than chat deltas. The single `response.completed` event carries the final status + usage, so
//! the terminal `Finish` is emitted there (no `on_halt` flush needed).
//!
//! This increment ports the parity-critical core (text + tool-calling + the multi-turn loop). Reasoning
//! summary streaming, media, and the provider-option knobs (`store`/`reasoning`/`include`/…) are
//! deferred — reasoning `output_item`s are decoded but their (empty here) summaries are ignored.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    ContentPart, FinishReason, Generation, LlmError, LlmEvent, LlmRequest, Protocol, Role,
    ToolChoice, ToolDefinition, Usage,
};

/// The OpenAI Responses protocol.
pub struct OpenAiResponses;

// ---- Request body (Responses API shape; serialized to JSON by the transport) ----

/// The Responses request body. Field order mirrors the TS `fromRequest`; JSON object key order is
/// irrelevant to the provider, so `input`/`tools` items are built as [`Value`] for brevity.
#[derive(Debug, serde::Serialize)]
pub struct ResponsesBody {
    model: String,
    input: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
}

/// Collect the plain-text parts of a message into one string (system items take string content).
fn collect_text(content: &[ContentPart]) -> String {
    content
        .iter()
        .filter_map(|p| match p {
            ContentPart::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// A `function_call_output.output` is a string: a string result passes through, anything structured is
/// JSON-encoded (mirrors the TS `toolResultText`).
fn result_to_output(result: &Value) -> String {
    match result {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn lower_body(request: &LlmRequest) -> ResponsesBody {
    let mut items: Vec<Value> = Vec::new();

    // Top-level system prompt → a single `system` item with joined string content.
    if !request.system.is_empty() {
        items.push(json!({ "role": "system", "content": request.system.join("\n\n") }));
    }

    for message in &request.messages {
        match message.role {
            // Mid-conversation system updates are delivered as a `user` `input_text` (matches the TS).
            Role::System => {
                items.push(json!({
                    "role": "user",
                    "content": [{ "type": "input_text", "text": collect_text(&message.content) }],
                }));
            }
            Role::User => {
                let content: Vec<Value> = message
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text(t) => Some(json!({ "type": "input_text", "text": t })),
                        _ => None,
                    })
                    .collect();
                items.push(json!({ "role": "user", "content": content }));
            }
            Role::Assistant => {
                // Assistant text → an `output_text` message; tool calls → top-level `function_call`s.
                let texts: Vec<Value> = message
                    .content
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text(t) => Some(json!({ "type": "output_text", "text": t })),
                        _ => None,
                    })
                    .collect();
                if !texts.is_empty() {
                    items.push(json!({ "role": "assistant", "content": texts }));
                }
                for part in &message.content {
                    if let ContentPart::ToolCall { id, name, input } = part {
                        items.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": input.to_string(),
                        }));
                    }
                }
            }
            Role::Tool => {
                for part in &message.content {
                    if let ContentPart::ToolResult { id, result, .. } = part {
                        items.push(json!({
                            "type": "function_call_output",
                            "call_id": id,
                            "output": result_to_output(result),
                        }));
                    }
                }
            }
        }
    }

    let tools = if request.tools.is_empty() {
        None
    } else {
        Some(request.tools.iter().map(lower_tool).collect())
    };
    let tool_choice = request.tool_choice.as_ref().map(lower_tool_choice);

    let Generation {
        max_tokens,
        temperature,
        top_p,
        top_k: _,
        stop: _,
    } = request.generation.clone();

    ResponsesBody {
        model: request.model.clone(),
        input: items,
        tools,
        tool_choice,
        stream: true,
        max_output_tokens: max_tokens,
        temperature,
        top_p,
    }
}

fn lower_tool(tool: &ToolDefinition) -> Value {
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description.clone().unwrap_or_default(),
        "parameters": tool.input_schema,
    })
}

fn lower_tool_choice(choice: &ToolChoice) -> Value {
    match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool(name) => json!({ "type": "function", "name": name }),
    }
}

// ---- Provider event shapes (only the fields the decoder reads; unknown events fold to `Other`) ----

/// One decoded Responses stream event (internally tagged by `type`; unhandled events → [`Other`]).
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ResponsesEvent {
    #[serde(rename = "response.output_item.added")]
    OutputItemAdded { item: OutputItem },
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta { item_id: String, delta: String },
    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta { item_id: String, delta: String },
    #[serde(rename = "response.output_item.done")]
    OutputItemDone { item: OutputItem },
    #[serde(rename = "response.completed")]
    Completed { response: ResponseBody },
    #[serde(rename = "response.incomplete")]
    Incomplete { response: ResponseBody },
    #[serde(rename = "response.failed")]
    Failed { response: ResponseBody },
    /// Setup / reasoning / text-done / arguments-done / content-part events — no normalized output.
    #[serde(other)]
    Other,
}

/// An output item carried by `response.output_item.{added,done}`. Only `function_call` drives output;
/// `message` and `reasoning` items fold to [`OutputItem::Other`].
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum OutputItem {
    #[serde(rename = "function_call")]
    FunctionCall {
        /// Stream-internal item id (`fc_…`) — the key deltas reference.
        id: String,
        /// The id tool results reference (`call_…`) — the normalized tool-call id.
        call_id: String,
        name: String,
        #[serde(default)]
        arguments: String,
    },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct ResponseBody {
    #[serde(default)]
    incomplete_details: Option<IncompleteDetails>,
    #[serde(default)]
    usage: Option<ResponsesUsage>,
    #[serde(default)]
    error: Option<ResponseError>,
}

#[derive(Debug, Deserialize)]
struct IncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ResponsesUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<InputTokensDetails>,
}

#[derive(Debug, Deserialize)]
struct InputTokensDetails {
    #[serde(default)]
    cached_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ResponseError {
    #[serde(default)]
    message: String,
}

// ---- Streaming state ----

#[derive(Debug)]
struct ToolBuf {
    /// The `call_…` id the streamed argument deltas are reported under.
    call_id: String,
    /// Streamed arguments — a fallback if `output_item.done` omits the full `arguments`.
    arguments: String,
}

/// Accumulator for a Responses stream: open text blocks (by item id, in open order) and streaming tool
/// buffers (keyed by the `fc_…` item id), plus whether any tool call was seen (drives the finish reason).
#[derive(Default)]
pub struct ResponsesState {
    open_text: Vec<String>,
    tools: HashMap<String, ToolBuf>,
    has_function_call: bool,
}

/// OpenAI reports `input_tokens` (inclusive total, with a `cached_tokens` subset) and `output_tokens`
/// (inclusive of reasoning) — matching our inclusive [`Usage`].
fn map_usage(usage: &ResponsesUsage) -> Usage {
    Usage {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage
            .input_tokens_details
            .as_ref()
            .map(|d| d.cached_tokens)
            .unwrap_or(0),
        cache_write: 0,
    }
}

fn map_finish_reason(response: &ResponseBody, has_function_call: bool) -> FinishReason {
    match response
        .incomplete_details
        .as_ref()
        .and_then(|d| d.reason.as_deref())
    {
        None => {
            if has_function_call {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            }
        }
        Some("max_output_tokens") => FinishReason::Length,
        Some("content_filter") => FinishReason::ContentFilter,
        Some(_) => {
            if has_function_call {
                FinishReason::ToolCalls
            } else {
                FinishReason::Unknown
            }
        }
    }
}

fn parse_arguments(arguments: &str) -> Value {
    let trimmed = arguments.trim();
    if trimmed.is_empty() {
        return json!({});
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| json!({}))
}

/// Close any open text blocks and emit the terminal `StepFinish` + `Finish` (the Responses API has no
/// per-block text-end we act on — `response.completed` is authoritative).
fn finish(state: &mut ResponsesState, response: &ResponseBody) -> Vec<LlmEvent> {
    let mut out = Vec::new();
    for id in std::mem::take(&mut state.open_text) {
        out.push(LlmEvent::TextEnd { id });
    }
    let reason = map_finish_reason(response, state.has_function_call);
    let usage = response.usage.as_ref().map(map_usage);
    out.push(LlmEvent::StepFinish { reason, usage });
    out.push(LlmEvent::Finish { reason, usage });
    out
}

impl Protocol for OpenAiResponses {
    type Body = ResponsesBody;
    type Event = ResponsesEvent;
    type State = ResponsesState;

    fn name(&self) -> &'static str {
        "openai-responses"
    }

    fn build_body(&self, request: &LlmRequest) -> Result<ResponsesBody, LlmError> {
        Ok(lower_body(request))
    }

    fn initial(&self) -> ResponsesState {
        ResponsesState::default()
    }

    fn decode_frame(&self, frame: &str) -> Result<ResponsesEvent, LlmError> {
        serde_json::from_str(frame)
            .map_err(|e| LlmError::Decode(format!("openai-responses event: {e}")))
    }

    fn step(&self, state: &mut ResponsesState, event: ResponsesEvent) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        match event {
            ResponsesEvent::OutputItemAdded { item } => {
                if let OutputItem::FunctionCall {
                    id, call_id, name, ..
                } = item
                {
                    state.has_function_call = true;
                    out.push(LlmEvent::ToolInputStart {
                        id: call_id.clone(),
                        name,
                    });
                    state.tools.insert(
                        id,
                        ToolBuf {
                            call_id,
                            arguments: String::new(),
                        },
                    );
                }
            }
            ResponsesEvent::OutputTextDelta { item_id, delta } => {
                if !state.open_text.contains(&item_id) {
                    state.open_text.push(item_id.clone());
                    out.push(LlmEvent::TextStart {
                        id: item_id.clone(),
                    });
                }
                if !delta.is_empty() {
                    out.push(LlmEvent::TextDelta {
                        id: item_id,
                        text: delta,
                    });
                }
            }
            ResponsesEvent::FunctionCallArgumentsDelta { item_id, delta } => {
                if let Some(buf) = state.tools.get_mut(&item_id) {
                    if !delta.is_empty() {
                        buf.arguments.push_str(&delta);
                        out.push(LlmEvent::ToolInputDelta {
                            id: buf.call_id.clone(),
                            text: delta,
                        });
                    }
                }
            }
            ResponsesEvent::OutputItemDone { item } => {
                if let OutputItem::FunctionCall {
                    id,
                    call_id,
                    name,
                    arguments,
                } = item
                {
                    // The done item carries the full arguments; fall back to the streamed buffer.
                    let buffered = state.tools.remove(&id);
                    let raw = if arguments.is_empty() {
                        buffered.map(|b| b.arguments).unwrap_or_default()
                    } else {
                        arguments
                    };
                    out.push(LlmEvent::ToolInputEnd {
                        id: call_id.clone(),
                    });
                    out.push(LlmEvent::ToolCall {
                        id: call_id,
                        name,
                        input: parse_arguments(&raw),
                    });
                }
            }
            ResponsesEvent::Completed { response } | ResponsesEvent::Incomplete { response } => {
                out.extend(finish(state, &response));
            }
            ResponsesEvent::Failed { response } => {
                let message = response
                    .error
                    .map(|e| e.message)
                    .unwrap_or_else(|| "OpenAI Responses response failed".to_string());
                out.push(LlmEvent::ProviderError { message });
            }
            ResponsesEvent::Other => {}
        }
        out
    }

    fn terminal(&self, event: &ResponsesEvent) -> bool {
        matches!(
            event,
            ResponsesEvent::Completed { .. }
                | ResponsesEvent::Incomplete { .. }
                | ResponsesEvent::Failed { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_sse, Message};

    // Real recorded cassettes from `packages/llm/test/fixtures/recordings/openai-responses/`.
    const STREAMS_TEXT: &str =
        include_str!("../fixtures/recordings/openai-responses/gpt-5-5-streams-text.json");
    const STREAMS_TOOL_CALL: &str =
        include_str!("../fixtures/recordings/openai-responses/gpt-5-5-streams-tool-call.json");
    const DRIVES_TOOL_LOOP: &str =
        include_str!("../fixtures/recordings/openai-responses/gpt-5-5-drives-a-tool-loop.json");

    fn response_body(cassette: &str, interaction: usize) -> String {
        let doc: Value = serde_json::from_str(cassette).unwrap();
        doc["interactions"][interaction]["response"]["body"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn decode(cassette: &str, interaction: usize) -> Vec<LlmEvent> {
        decode_sse(&OpenAiResponses, &response_body(cassette, interaction)).unwrap()
    }

    fn collected_text(events: &[LlmEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    fn body_value(request: &LlmRequest) -> Value {
        serde_json::to_value(OpenAiResponses.build_body(request).unwrap()).unwrap()
    }

    #[test]
    fn decodes_text_stream() {
        let events = decode(STREAMS_TEXT, 0);
        assert_eq!(collected_text(&events), "Hello!");
        assert!(matches!(events.first(), Some(LlmEvent::TextStart { .. })));
        assert!(events.iter().any(|e| matches!(e, LlmEvent::TextEnd { .. })));
        match events.last() {
            Some(LlmEvent::Finish { reason, usage }) => {
                assert_eq!(*reason, FinishReason::Stop);
                let usage = usage.unwrap();
                assert_eq!(usage.input, 20);
                assert_eq!(usage.output, 18); // reasoning-inclusive
            }
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    #[test]
    fn decodes_tool_call_stream() {
        let events = decode(STREAMS_TOOL_CALL, 0);
        let call = events
            .iter()
            .find_map(|e| match e {
                LlmEvent::ToolCall { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .expect("a tool-call event");
        // The normalized id is the `call_…` id (what tool results reference), not the `fc_…` item id.
        assert!(call.0.starts_with("call_"));
        assert_eq!(call.1, "get_weather");
        assert_eq!(call.2, json!({ "city": "Paris" }));
        assert!(events
            .iter()
            .any(|e| matches!(e, LlmEvent::ToolInputStart { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, LlmEvent::ToolInputDelta { .. })));
        match events.last() {
            Some(LlmEvent::Finish { reason, usage }) => {
                assert_eq!(*reason, FinishReason::ToolCalls);
                assert_eq!(usage.unwrap().input, 61);
            }
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    #[test]
    fn decodes_tool_loop_both_turns() {
        // Turn 0: a tool call, no text → finishes for tool-calls.
        let first = decode(DRIVES_TOOL_LOOP, 0);
        assert!(first.iter().any(|e| matches!(e, LlmEvent::ToolCall { .. })));
        assert!(matches!(
            first.last(),
            Some(LlmEvent::Finish {
                reason: FinishReason::ToolCalls,
                ..
            })
        ));
        // Turn 1: the model answers in text after the tool result → finishes for stop.
        let second = decode(DRIVES_TOOL_LOOP, 1);
        assert!(
            collected_text(&second).contains("Paris"),
            "answer was {:?}",
            collected_text(&second)
        );
        match second.last() {
            Some(LlmEvent::Finish { reason, usage }) => {
                assert_eq!(*reason, FinishReason::Stop);
                assert_eq!(usage.unwrap().input, 106);
            }
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    #[test]
    fn lowers_text_request_to_cassette_body() {
        let request = LlmRequest {
            model: "gpt-5.5".into(),
            system: vec!["You are concise.".into()],
            messages: vec![Message::user_text("Reply with exactly: Hello!")],
            generation: Generation {
                max_tokens: Some(80),
                ..Default::default()
            },
            ..Default::default()
        };
        let body = body_value(&request);
        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(
            body["input"],
            json!([
                { "role": "system", "content": "You are concise." },
                { "role": "user", "content": [{ "type": "input_text", "text": "Reply with exactly: Hello!" }] }
            ])
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_output_tokens"], 80);
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn lowers_tool_request_to_cassette_body() {
        let schema = json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
            "additionalProperties": false
        });
        let request = LlmRequest {
            model: "gpt-5.5".into(),
            system: vec!["Call tools exactly as requested.".into()],
            messages: vec![Message::user_text(
                "Call get_weather with city exactly Paris.",
            )],
            tools: vec![ToolDefinition {
                name: "get_weather".into(),
                description: Some("Get current weather for a city.".into()),
                input_schema: schema.clone(),
            }],
            tool_choice: Some(ToolChoice::Tool("get_weather".into())),
            generation: Generation {
                max_tokens: Some(80),
                ..Default::default()
            },
        };
        let body = body_value(&request);
        assert_eq!(
            body["tools"],
            json!([{
                "type": "function",
                "name": "get_weather",
                "description": "Get current weather for a city.",
                "parameters": schema
            }])
        );
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "function", "name": "get_weather" })
        );
    }

    #[test]
    fn lowers_tool_loop_messages_to_cassette_body() {
        // The second turn replays the assistant `function_call` and the user `function_call_output`.
        let request = LlmRequest {
            model: "gpt-5.5".into(),
            system: vec!["Use the get_weather tool, then answer in one short sentence.".into()],
            messages: vec![
                Message::user_text("What is the weather in Paris?"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolCall {
                        id: "call_JCuVTkQxVB3cCmFWx52adJKZ".into(),
                        name: "get_weather".into(),
                        input: json!({ "city": "Paris" }),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        id: "call_JCuVTkQxVB3cCmFWx52adJKZ".into(),
                        name: "get_weather".into(),
                        result: json!({ "temperature": 22, "condition": "sunny" }),
                    }],
                },
            ],
            tools: vec![ToolDefinition {
                name: "get_weather".into(),
                description: Some("Get current weather for a city.".into()),
                input_schema: json!({ "type": "object" }),
            }],
            generation: Generation {
                max_tokens: Some(80),
                ..Default::default()
            },
            ..Default::default()
        };
        let body = body_value(&request);
        let input = body["input"].as_array().unwrap();
        // system, user, function_call, function_call_output
        assert_eq!(input.len(), 4);
        assert_eq!(
            input[2],
            json!({
                "type": "function_call",
                "call_id": "call_JCuVTkQxVB3cCmFWx52adJKZ",
                "name": "get_weather",
                "arguments": "{\"city\":\"Paris\"}"
            })
        );
        // The tool result is a top-level `function_call_output`; its `output` is a JSON-encoded string,
        // compared structurally (object key order in the encoded string is not significant).
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_JCuVTkQxVB3cCmFWx52adJKZ");
        let output: Value = serde_json::from_str(input[3]["output"].as_str().unwrap()).unwrap();
        assert_eq!(output, json!({ "temperature": 22, "condition": "sunny" }));
    }
}
