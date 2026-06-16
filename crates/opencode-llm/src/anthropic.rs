//! `anthropic-messages` protocol — the default provider, ported from
//! `packages/llm/src/protocols/anthropic-messages.ts` (decode side).
//!
//! Decodes the Anthropic Messages SSE stream (`message_start`, `content_block_start/delta/stop`,
//! `message_delta`, `message_stop`, `ping`, `error`) into the normalized [`LlmEvent`] stream, threading
//! a [`AnthropicState`] for open content blocks, streaming tool-argument buffers, and usage. Request
//! lowering (`body.from`) and the live transport are the next increment.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ContentPart, FinishReason, Generation, LlmError, LlmEvent, LlmRequest, Message, Protocol, Role,
    ToolChoice, Usage,
};

/// The `anthropic-messages` protocol.
pub struct AnthropicMessages;

/// Anthropic requires `max_tokens`; default it when the request leaves it unset.
const DEFAULT_MAX_TOKENS: u64 = 4096;

// ---- Request body (the `body.from` lowering target) ----

/// The Anthropic Messages request body (`AnthropicMessagesBody`), core subset.
#[derive(Debug, Serialize, PartialEq)]
pub struct AnthropicBody {
    model: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<AnthropicText>,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<AnthropicTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice>,
    stream: bool,
    max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_k: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq)]
struct AnthropicText {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

#[derive(Debug, Serialize, PartialEq)]
struct AnthropicMessage {
    role: &'static str,
    content: Vec<AnthropicContentBlock>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "type")]
enum AnthropicContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
    },
}

#[derive(Debug, Serialize, PartialEq)]
struct AnthropicTool {
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    input_schema: Value,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "type")]
enum AnthropicToolChoice {
    #[serde(rename = "auto")]
    Auto,
    #[serde(rename = "any")]
    Any,
    #[serde(rename = "tool")]
    Tool { name: String },
}

fn text_block(text: String) -> AnthropicText {
    AnthropicText { kind: "text", text }
}

/// Render a tool result as Anthropic `tool_result` content (a string; rich/media results deferred).
fn result_to_string(result: &Value) -> String {
    match result {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn lower_tool_choice(choice: &ToolChoice) -> Option<AnthropicToolChoice> {
    match choice {
        ToolChoice::Auto => Some(AnthropicToolChoice::Auto),
        ToolChoice::Required => Some(AnthropicToolChoice::Any),
        ToolChoice::Tool(name) => Some(AnthropicToolChoice::Tool { name: name.clone() }),
        // "none" is expressed by simply omitting tools/tool_choice.
        ToolChoice::None => None,
    }
}

fn lower_message(message: &Message) -> AnthropicMessage {
    // Anthropic messages are only `user`/`assistant`; system prompt is top-level and tool results
    // are delivered in a `user` message.
    let role = match message.role {
        Role::Assistant => "assistant",
        Role::System | Role::User | Role::Tool => "user",
    };
    let content = message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text(text) => AnthropicContentBlock::Text { text: text.clone() },
            ContentPart::ToolCall { id, name, input } => AnthropicContentBlock::ToolUse {
                id: id.clone(),
                name: name.clone(),
                input: input.clone(),
            },
            ContentPart::ToolResult { id, result, .. } => AnthropicContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: result_to_string(result),
            },
        })
        .collect();
    AnthropicMessage { role, content }
}

fn lower_body(request: &LlmRequest) -> AnthropicBody {
    let Generation {
        max_tokens,
        temperature,
        top_p,
        top_k,
        stop,
    } = request.generation.clone();
    AnthropicBody {
        model: request.model.clone(),
        system: request.system.iter().cloned().map(text_block).collect(),
        messages: request.messages.iter().map(lower_message).collect(),
        tools: request
            .tools
            .iter()
            .map(|t| AnthropicTool {
                name: t.name.clone(),
                description: t.description.clone(),
                input_schema: t.input_schema.clone(),
            })
            .collect(),
        tool_choice: request.tool_choice.as_ref().and_then(lower_tool_choice),
        stream: true,
        max_tokens: max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        temperature,
        top_p,
        top_k,
        stop_sequences: stop,
    }
}

// ---- Provider event shapes (only the fields the decoder reads; unknown fields are ignored) ----

/// One decoded Anthropic SSE event.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum AnthropicEvent {
    #[serde(rename = "message_start")]
    MessageStart { message: MessageStartBody },
    #[serde(rename = "content_block_start")]
    ContentBlockStart {
        index: u32,
        content_block: ContentBlock,
    },
    #[serde(rename = "content_block_delta")]
    ContentBlockDelta { index: u32, delta: BlockDelta },
    #[serde(rename = "content_block_stop")]
    ContentBlockStop { index: u32 },
    #[serde(rename = "message_delta")]
    MessageDelta {
        delta: MessageDeltaBody,
        #[serde(default)]
        usage: Option<AnthropicUsage>,
    },
    #[serde(rename = "message_stop")]
    MessageStop,
    #[serde(rename = "ping")]
    Ping,
    #[serde(rename = "error")]
    Error {
        #[serde(default)]
        error: AnthropicErrorBody,
    },
    /// Any other (e.g. server-tool) event — ignored by this increment.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
pub struct MessageStartBody {
    #[serde(default)]
    usage: Option<AnthropicUsage>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text,
    #[serde(rename = "thinking")]
    Thinking,
    #[serde(rename = "tool_use")]
    ToolUse { id: String, name: String },
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum BlockDelta {
    #[serde(rename = "text_delta")]
    Text {
        #[serde(default)]
        text: String,
    },
    #[serde(rename = "thinking_delta")]
    Thinking {
        #[serde(default)]
        thinking: String,
    },
    #[serde(rename = "input_json_delta")]
    InputJson {
        #[serde(default)]
        partial_json: String,
    },
    /// `signature_delta` and others — ignored.
    #[serde(other)]
    Other,
}

#[derive(Debug, Deserialize)]
pub struct MessageDeltaBody {
    #[serde(default)]
    stop_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AnthropicUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
}

#[derive(Debug, Default, Deserialize)]
pub struct AnthropicErrorBody {
    #[serde(default)]
    message: String,
}

// ---- Streaming state ----

#[derive(Debug, Clone)]
enum Block {
    Text { id: String },
    Reasoning { id: String },
    Tool { id: String, name: String },
}

/// Accumulator for an Anthropic stream: open blocks (by content-block index), streaming tool-argument
/// buffers, and merged usage.
#[derive(Default)]
pub struct AnthropicState {
    usage: Usage,
    blocks: HashMap<u32, Block>,
    tool_buffers: HashMap<u32, String>,
}

/// Content blocks are keyed by index in the stream; derive a stable id for text/reasoning blocks
/// (tool blocks carry their own `toolu_…` id).
fn block_id(index: u32) -> String {
    format!("block_{index}")
}

fn map_stop_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("end_turn") | Some("stop_sequence") => FinishReason::Stop,
        Some("max_tokens") => FinishReason::Length,
        Some("tool_use") => FinishReason::ToolCalls,
        Some("refusal") => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

fn parse_tool_input(buffer: &str) -> serde_json::Value {
    let trimmed = buffer.trim();
    if trimmed.is_empty() {
        return serde_json::json!({});
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| serde_json::json!({}))
}

/// Merge an Anthropic usage report into the running total. Anthropic's `input_tokens` excludes cached
/// tokens, so the inclusive input is recomputed from the breakdown; `message_delta` is authoritative
/// for output.
fn merge_usage(into: &mut Usage, usage: &AnthropicUsage) {
    let input =
        usage.input_tokens + usage.cache_read_input_tokens + usage.cache_creation_input_tokens;
    if input > 0 {
        into.input = input;
    }
    if usage.output_tokens > 0 {
        into.output = usage.output_tokens;
    }
    if usage.cache_read_input_tokens > 0 {
        into.cache_read = usage.cache_read_input_tokens;
    }
    if usage.cache_creation_input_tokens > 0 {
        into.cache_write = usage.cache_creation_input_tokens;
    }
}

impl Protocol for AnthropicMessages {
    type Body = AnthropicBody;
    type Event = AnthropicEvent;
    type State = AnthropicState;

    fn name(&self) -> &'static str {
        "anthropic-messages"
    }

    fn build_body(&self, request: &LlmRequest) -> Result<AnthropicBody, LlmError> {
        Ok(lower_body(request))
    }

    fn initial(&self) -> AnthropicState {
        AnthropicState::default()
    }

    fn decode_frame(&self, frame: &str) -> Result<AnthropicEvent, LlmError> {
        serde_json::from_str(frame.trim()).map_err(|e| LlmError::Decode(e.to_string()))
    }

    fn step(&self, state: &mut AnthropicState, event: AnthropicEvent) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        match event {
            AnthropicEvent::MessageStart { message } => {
                if let Some(usage) = message.usage {
                    merge_usage(&mut state.usage, &usage);
                }
            }
            AnthropicEvent::ContentBlockStart {
                index,
                content_block,
            } => match content_block {
                ContentBlock::Text => {
                    let id = block_id(index);
                    state.blocks.insert(index, Block::Text { id: id.clone() });
                    out.push(LlmEvent::TextStart { id });
                }
                ContentBlock::Thinking => {
                    let id = block_id(index);
                    state
                        .blocks
                        .insert(index, Block::Reasoning { id: id.clone() });
                    out.push(LlmEvent::ReasoningStart { id });
                }
                ContentBlock::ToolUse { id, name } => {
                    state.blocks.insert(
                        index,
                        Block::Tool {
                            id: id.clone(),
                            name: name.clone(),
                        },
                    );
                    state.tool_buffers.insert(index, String::new());
                    out.push(LlmEvent::ToolInputStart { id, name });
                }
                ContentBlock::Other => {}
            },
            AnthropicEvent::ContentBlockDelta { index, delta } => {
                match (state.blocks.get(&index).cloned(), delta) {
                    (Some(Block::Text { id }), BlockDelta::Text { text }) => {
                        out.push(LlmEvent::TextDelta { id, text });
                    }
                    (Some(Block::Reasoning { id }), BlockDelta::Thinking { thinking }) => {
                        out.push(LlmEvent::ReasoningDelta { id, text: thinking });
                    }
                    (Some(Block::Tool { id, .. }), BlockDelta::InputJson { partial_json }) => {
                        state
                            .tool_buffers
                            .entry(index)
                            .or_default()
                            .push_str(&partial_json);
                        out.push(LlmEvent::ToolInputDelta {
                            id,
                            text: partial_json,
                        });
                    }
                    _ => {}
                }
            }
            AnthropicEvent::ContentBlockStop { index } => {
                if let Some(block) = state.blocks.remove(&index) {
                    match block {
                        Block::Text { id } => out.push(LlmEvent::TextEnd { id }),
                        Block::Reasoning { id } => out.push(LlmEvent::ReasoningEnd { id }),
                        Block::Tool { id, name } => {
                            let buffer = state.tool_buffers.remove(&index).unwrap_or_default();
                            let input = parse_tool_input(&buffer);
                            out.push(LlmEvent::ToolInputEnd { id: id.clone() });
                            out.push(LlmEvent::ToolCall { id, name, input });
                        }
                    }
                }
            }
            AnthropicEvent::MessageDelta { delta, usage } => {
                if let Some(usage) = usage {
                    merge_usage(&mut state.usage, &usage);
                }
                let reason = map_stop_reason(delta.stop_reason.as_deref());
                out.push(LlmEvent::StepFinish {
                    reason,
                    usage: Some(state.usage),
                });
                out.push(LlmEvent::Finish {
                    reason,
                    usage: Some(state.usage),
                });
            }
            AnthropicEvent::Error { error } => out.push(LlmEvent::ProviderError {
                message: error.message,
            }),
            AnthropicEvent::MessageStop | AnthropicEvent::Ping | AnthropicEvent::Unknown => {}
        }
        out
    }

    fn terminal(&self, event: &AnthropicEvent) -> bool {
        matches!(event, AnthropicEvent::MessageStop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_sse, Generation, LlmRequest, Message, ToolChoice, ToolDefinition};
    use serde_json::json;

    // Recorded SSE bodies from `packages/llm/test/fixtures/recordings/anthropic-messages/`
    // (`streams-text.json` and `streams-tool-call.json`) — the http-recorder cassettes, byte-faithful.

    const STREAMS_TEXT: &str = r#"event: message_start
data: {"type":"message_start","message":{"model":"claude-haiku-4-5-20251001","id":"msg_01UodR8c3ezAK8rAfi8HAs8g","type":"message","role":"assistant","content":[],"stop_reason":null,"stop_sequence":null,"stop_details":null,"usage":{"input_tokens":18,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0},"output_tokens":2,"service_tier":"standard","inference_geo":"not_available"}}          }

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}    }

event: ping
data: {"type": "ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello!"}    }

event: content_block_stop
data: {"type":"content_block_stop","index":0   }

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null,"stop_details":null},"usage":{"input_tokens":18,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":5}             }

event: message_stop
data: {"type":"message_stop"         }
"#;

    const STREAMS_TOOL_CALL: &str = r#"event: message_start
data: {"type":"message_start","message":{"model":"claude-haiku-4-5-20251001","id":"msg_01RYgU7NUPMK4B9v8S7gVpCS","type":"message","role":"assistant","content":[],"stop_reason":null,"stop_sequence":null,"stop_details":null,"usage":{"input_tokens":677,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"cache_creation":{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":0},"output_tokens":16,"service_tier":"standard","inference_geo":"not_available"}}             }

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_012rmAruviySvUXSjgCPWVRu","name":"get_weather","input":{},"caller":{"type":"direct"}}      }

event: ping
data: {"type": "ping"}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":""}             }

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}    }

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":" \"Paris\"}"}     }

event: content_block_stop
data: {"type":"content_block_stop","index":0     }

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null,"stop_details":null},"usage":{"input_tokens":677,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":33}              }

event: message_stop
data: {"type":"message_stop"     }
"#;

    #[test]
    fn decodes_text_stream() {
        let events = decode_sse(&AnthropicMessages, STREAMS_TEXT).unwrap();

        // Text deltas concatenate to the assistant message.
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "Hello!");

        // A start/end brackets the deltas, sharing one id.
        assert!(matches!(events.first(), Some(LlmEvent::TextStart { .. })));
        assert!(events.iter().any(|e| matches!(e, LlmEvent::TextEnd { .. })));

        // The terminal Finish carries the stop reason + merged usage (output authoritative).
        match events.last() {
            Some(LlmEvent::Finish { reason, usage }) => {
                assert_eq!(*reason, FinishReason::Stop);
                let usage = usage.unwrap();
                assert_eq!(usage.input, 18);
                assert_eq!(usage.output, 5);
            }
            other => panic!("expected Finish, got {other:?}"),
        }
        // `ping` / `message_start` / `message_stop` emit nothing.
        assert!(!events
            .iter()
            .any(|e| matches!(e, LlmEvent::ProviderError { .. })));
    }

    #[test]
    fn decodes_tool_call_stream() {
        let events = decode_sse(&AnthropicMessages, STREAMS_TOOL_CALL).unwrap();

        // The streamed partial JSON parses into the complete tool call.
        let tool_call = events
            .iter()
            .find_map(|e| match e {
                LlmEvent::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
                _ => None,
            })
            .expect("a tool-call event");
        assert_eq!(tool_call.0, "get_weather");
        assert_eq!(tool_call.1, serde_json::json!({ "city": "Paris" }));

        // The tool block streamed start → deltas → end, all sharing the tool-use id.
        let start = events.iter().find_map(|e| match e {
            LlmEvent::ToolInputStart { id, name } => Some((id.clone(), name.clone())),
            _ => None,
        });
        assert_eq!(
            start,
            Some((
                "toolu_012rmAruviySvUXSjgCPWVRu".to_string(),
                "get_weather".to_string()
            ))
        );
        assert!(events
            .iter()
            .any(|e| matches!(e, LlmEvent::ToolInputDelta { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, LlmEvent::ToolInputEnd { .. })));

        // Finish: tool-calls reason + usage.
        match events.last() {
            Some(LlmEvent::Finish { reason, usage }) => {
                assert_eq!(*reason, FinishReason::ToolCalls);
                let usage = usage.unwrap();
                assert_eq!(usage.input, 677);
                assert_eq!(usage.output, 33);
            }
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    #[test]
    fn decode_error_event_surfaces_provider_error() {
        let body = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";
        let events = decode_sse(&AnthropicMessages, body).unwrap();
        assert_eq!(
            events,
            vec![LlmEvent::ProviderError {
                message: "Overloaded".to_string()
            }]
        );
    }

    // ---- request lowering (`body.from`), parity with the cassettes' recorded request bodies ----

    fn body_value(request: &LlmRequest) -> serde_json::Value {
        serde_json::to_value(AnthropicMessages.build_body(request).unwrap()).unwrap()
    }

    #[test]
    fn lowers_text_request_to_cassette_body() {
        // Matches `streams-text.json` interactions[0].request.body.
        let request = LlmRequest {
            model: "claude-haiku-4-5-20251001".into(),
            system: vec!["You are concise.".into()],
            messages: vec![Message::user_text("Reply with exactly: Hello!")],
            generation: Generation {
                max_tokens: Some(20),
                temperature: Some(0.0),
                ..Default::default()
            },
            ..Default::default()
        };
        let body = body_value(&request);
        assert_eq!(body["model"], "claude-haiku-4-5-20251001");
        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "You are concise."}])
        );
        assert_eq!(
            body["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "Reply with exactly: Hello!"}]}])
        );
        assert_eq!(body["stream"], true);
        assert_eq!(body["max_tokens"], 20);
        assert_eq!(body["temperature"].as_f64(), Some(0.0));
        // No tools → tools/tool_choice omitted.
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn lowers_tool_request_to_cassette_body() {
        // Matches `streams-tool-call.json` interactions[0].request.body.
        let schema = json!({
            "type": "object",
            "properties": { "city": { "type": "string" } },
            "required": ["city"],
            "additionalProperties": false
        });
        let request = LlmRequest {
            model: "claude-haiku-4-5-20251001".into(),
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
                temperature: Some(0.0),
                ..Default::default()
            },
        };
        let body = body_value(&request);
        assert_eq!(
            body["tools"],
            json!([{
                "name": "get_weather",
                "description": "Get current weather for a city.",
                "input_schema": schema
            }])
        );
        assert_eq!(
            body["tool_choice"],
            json!({"type": "tool", "name": "get_weather"})
        );
        assert_eq!(body["max_tokens"], 80);
    }

    #[test]
    fn lowers_assistant_tool_use_and_tool_result() {
        let request = LlmRequest {
            model: "m".into(),
            messages: vec![
                Message::user_text("weather?"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolCall {
                        id: "toolu_1".into(),
                        name: "get_weather".into(),
                        input: json!({ "city": "Paris" }),
                    }],
                },
                Message {
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        id: "toolu_1".into(),
                        name: "get_weather".into(),
                        result: json!("sunny"),
                    }],
                },
            ],
            ..Default::default()
        };
        let body = body_value(&request);
        assert_eq!(
            body["messages"][1],
            json!({
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "Paris"}}]
            })
        );
        assert_eq!(
            body["messages"][2],
            json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "sunny"}]
            })
        );
        // max_tokens defaulted when unset.
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn tool_choice_mapping() {
        let with = |choice: Option<ToolChoice>| {
            body_value(&LlmRequest {
                model: "m".into(),
                tool_choice: choice,
                ..Default::default()
            })
        };
        assert_eq!(
            with(Some(ToolChoice::Auto))["tool_choice"],
            json!({"type": "auto"})
        );
        assert_eq!(
            with(Some(ToolChoice::Required))["tool_choice"],
            json!({"type": "any"})
        );
        // `none` and absent both omit tool_choice.
        assert!(with(Some(ToolChoice::None)).get("tool_choice").is_none());
        assert!(with(None).get("tool_choice").is_none());
    }
}
