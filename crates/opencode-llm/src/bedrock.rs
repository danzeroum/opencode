//! Bedrock Converse protocol — ported from `packages/llm/src/protocols/bedrock-converse.ts`.
//!
//! Structurally this is the [`anthropic`](crate::anthropic) shape (content blocks keyed by index, with
//! explicit start/stop) carried over the AWS [`eventstream`](crate::eventstream) binary framing instead
//! of text SSE, with the finish split across two trailing frames: `messageStop` carries the stop reason
//! and `metadata` carries usage, so the terminal [`LlmEvent::Finish`] is flushed in [`on_halt`].
//!
//! This increment ports the parity-critical core: request-body lowering (`body.from`) and the streaming
//! decode (`step` + `onHalt`), proven against the recorded `bedrock-converse` cassettes. SigV4 request
//! signing + the live binary transport are the next increment (see `utils/bedrock-auth.ts`); the
//! decode is transport-agnostic, fed [`crate::decode_eventstream`] here and live frames later.

use std::collections::HashMap;

use serde::Deserialize;
use serde_json::{json, Value};

use crate::{
    ContentPart, FinishReason, Generation, LlmError, LlmEvent, LlmRequest, Message, Protocol, Role,
    ToolChoice, ToolDefinition, Usage,
};

/// The Bedrock Converse protocol.
pub struct BedrockConverse;

// ---- Request body (provider-native shape; serialized to JSON by the transport) ----

/// The Converse request body. Field order mirrors `BedrockBodyFields` in the TS; JSON object key
/// order is irrelevant to the provider, so blocks/tools are built as [`Value`] for brevity.
#[derive(Debug, serde::Serialize)]
pub struct BedrockBody {
    #[serde(rename = "modelId")]
    model_id: String,
    messages: Vec<BedrockMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    system: Vec<Value>,
    #[serde(rename = "inferenceConfig", skip_serializing_if = "Option::is_none")]
    inference_config: Option<InferenceConfig>,
    #[serde(rename = "toolConfig", skip_serializing_if = "Option::is_none")]
    tool_config: Option<ToolConfig>,
}

#[derive(Debug, serde::Serialize)]
struct BedrockMessage {
    role: &'static str,
    content: Vec<Value>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct InferenceConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    stop_sequences: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolConfig {
    tools: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<Value>,
}

/// Render a tool result as Converse `toolResult` content: a string result becomes `{text}`, anything
/// structured becomes `{json}` (mirrors the TS `text`/`error` vs `json` split; media is deferred).
fn result_to_content(result: &Value) -> Value {
    match result {
        Value::String(s) => json!({ "text": s }),
        other => json!({ "json": other }),
    }
}

fn lower_message(message: &Message) -> BedrockMessage {
    // Converse messages are only `user`/`assistant`; system prompts are top-level and tool results are
    // delivered as `user` `toolResult` blocks.
    let role = match message.role {
        Role::Assistant => "assistant",
        Role::System | Role::User | Role::Tool => "user",
    };
    let content = message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text(text) => json!({ "text": text }),
            ContentPart::ToolCall { id, name, input } => json!({
                "toolUse": { "toolUseId": id, "name": name, "input": input }
            }),
            ContentPart::ToolResult { id, result, .. } => json!({
                "toolResult": {
                    "toolUseId": id,
                    "content": [result_to_content(result)],
                    "status": "success",
                }
            }),
        })
        .collect();
    BedrockMessage { role, content }
}

fn lower_tool(tool: &ToolDefinition) -> Value {
    json!({
        "toolSpec": {
            "name": tool.name,
            "description": tool.description.clone().unwrap_or_default(),
            "inputSchema": { "json": tool.input_schema },
        }
    })
}

fn lower_tool_choice(choice: &ToolChoice) -> Option<Value> {
    match choice {
        ToolChoice::Auto => Some(json!({ "auto": {} })),
        ToolChoice::Required => Some(json!({ "any": {} })),
        ToolChoice::Tool(name) => Some(json!({ "tool": { "name": name } })),
        // "none" is expressed by omitting `toolConfig` entirely.
        ToolChoice::None => None,
    }
}

fn lower_body(request: &LlmRequest) -> BedrockBody {
    let Generation {
        max_tokens,
        temperature,
        top_p,
        top_k: _,
        stop,
    } = request.generation.clone();

    // Converse omits `inferenceConfig` unless at least one knob is set (Converse has no `topK`).
    let inference_config =
        if max_tokens.is_none() && temperature.is_none() && top_p.is_none() && stop.is_empty() {
            None
        } else {
            Some(InferenceConfig {
                max_tokens,
                temperature,
                top_p,
                stop_sequences: stop,
            })
        };

    // Converse omits `toolConfig` when there are no tools or tool choice is "none".
    let tool_choice = request.tool_choice.as_ref();
    let tool_config = if request.tools.is_empty() || matches!(tool_choice, Some(ToolChoice::None)) {
        None
    } else {
        Some(ToolConfig {
            tools: request.tools.iter().map(lower_tool).collect(),
            tool_choice: tool_choice.and_then(lower_tool_choice),
        })
    };

    BedrockBody {
        model_id: request.model.clone(),
        messages: request.messages.iter().map(lower_message).collect(),
        system: request
            .system
            .iter()
            .map(|s| json!({ "text": s }))
            .collect(),
        inference_config,
        tool_config,
    }
}

// ---- Provider event shapes (only the fields the decoder reads; unknown fields, incl. the AWS `p`
// padding, are ignored). Each frame is the `{ "<eventType>": payload }` rewrapping from `eventstream`. ----

/// One decoded Converse stream event (exactly one field is `Some`, keyed by the frame's event type).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BedrockEvent {
    // `messageStart` carries only the role, which we do not need.
    #[serde(default)]
    message_start: Option<Value>,
    #[serde(default)]
    content_block_start: Option<ContentBlockStart>,
    #[serde(default)]
    content_block_delta: Option<ContentBlockDelta>,
    #[serde(default)]
    content_block_stop: Option<ContentBlockStop>,
    #[serde(default)]
    message_stop: Option<MessageStop>,
    #[serde(default)]
    metadata: Option<Metadata>,
    // Stream-error events — surfaced as a `ProviderError`.
    #[serde(default)]
    internal_server_exception: Option<ExceptionBody>,
    #[serde(default)]
    model_stream_error_exception: Option<ExceptionBody>,
    #[serde(default)]
    service_unavailable_exception: Option<ExceptionBody>,
    #[serde(default)]
    validation_exception: Option<ExceptionBody>,
    #[serde(default)]
    throttling_exception: Option<ExceptionBody>,
}

impl BedrockEvent {
    /// The message of whichever stream-error event this is, if any.
    fn error_message(&self) -> Option<String> {
        self.internal_server_exception
            .as_ref()
            .or(self.model_stream_error_exception.as_ref())
            .or(self.service_unavailable_exception.as_ref())
            .or(self.validation_exception.as_ref())
            .or(self.throttling_exception.as_ref())
            .map(|e| e.message.clone())
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockStart {
    content_block_index: u32,
    #[serde(default)]
    start: Option<BlockStart>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockStart {
    #[serde(default)]
    tool_use: Option<ToolUseStart>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolUseStart {
    tool_use_id: String,
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockDelta {
    content_block_index: u32,
    #[serde(default)]
    delta: Option<BlockDelta>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BlockDelta {
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    tool_use: Option<ToolUseDelta>,
    #[serde(default)]
    reasoning_content: Option<ReasoningContent>,
}

#[derive(Debug, Deserialize)]
struct ToolUseDelta {
    #[serde(default)]
    input: String,
}

#[derive(Debug, Deserialize)]
struct ReasoningContent {
    #[serde(default)]
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentBlockStop {
    content_block_index: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MessageStop {
    stop_reason: String,
}

#[derive(Debug, Deserialize)]
struct Metadata {
    #[serde(default)]
    usage: Option<BedrockUsage>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BedrockUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
    #[serde(default)]
    cache_write_input_tokens: u64,
}

#[derive(Debug, Deserialize)]
struct ExceptionBody {
    #[serde(default)]
    message: String,
}

// ---- Streaming state ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    Text,
    Reasoning,
}

#[derive(Debug)]
struct ToolBuf {
    id: String,
    name: String,
    input: String,
}

/// Accumulator for a Converse stream: open text/reasoning blocks and streaming tool buffers (both keyed
/// by content-block index), plus the finish reason + usage held back until [`on_halt`].
#[derive(Default)]
pub struct BedrockState {
    blocks: HashMap<u32, OpenBlock>,
    tools: HashMap<u32, ToolBuf>,
    pending_reason: Option<FinishReason>,
    pending_usage: Option<Usage>,
    has_tool_calls: bool,
}

fn text_id(index: u32) -> String {
    format!("text_{index}")
}

fn reasoning_id(index: u32) -> String {
    format!("reasoning_{index}")
}

fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" | "stop_sequence" => FinishReason::Stop,
        "max_tokens" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "content_filtered" | "guardrail_intervened" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

/// Converse reports `inputTokens` as the inclusive total (cache read/write are subsets), which matches
/// our inclusive [`Usage`]; reasoning is not broken out of `outputTokens` for any current model.
fn map_usage(usage: BedrockUsage) -> Usage {
    Usage {
        input: usage.input_tokens,
        output: usage.output_tokens,
        cache_read: usage.cache_read_input_tokens,
        cache_write: usage.cache_write_input_tokens,
    }
}

fn parse_tool_input(buffer: &str) -> Value {
    let trimmed = buffer.trim();
    if trimmed.is_empty() {
        return json!({});
    }
    serde_json::from_str(trimmed).unwrap_or_else(|_| json!({}))
}

impl Protocol for BedrockConverse {
    type Body = BedrockBody;
    type Event = BedrockEvent;
    type State = BedrockState;

    fn name(&self) -> &'static str {
        "bedrock-converse"
    }

    fn build_body(&self, request: &LlmRequest) -> Result<BedrockBody, LlmError> {
        Ok(lower_body(request))
    }

    fn initial(&self) -> BedrockState {
        BedrockState::default()
    }

    fn decode_frame(&self, frame: &str) -> Result<BedrockEvent, LlmError> {
        serde_json::from_str(frame).map_err(|e| LlmError::Decode(format!("bedrock event: {e}")))
    }

    fn step(&self, state: &mut BedrockState, event: BedrockEvent) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        let _ = &event.message_start; // `messageStart` is a no-op (role only).

        // A tool block opens with its id + name.
        if let Some(start) = event.content_block_start {
            if let Some(tool) = start.start.and_then(|s| s.tool_use) {
                out.push(LlmEvent::ToolInputStart {
                    id: tool.tool_use_id.clone(),
                    name: tool.name.clone(),
                });
                state.tools.insert(
                    start.content_block_index,
                    ToolBuf {
                        id: tool.tool_use_id,
                        name: tool.name,
                        input: String::new(),
                    },
                );
            }
            return out;
        }

        // Deltas: text, reasoning, or streaming tool-argument JSON.
        if let Some(delta_event) = event.content_block_delta {
            let index = delta_event.content_block_index;
            if let Some(delta) = delta_event.delta {
                if let Some(text) = delta.text {
                    // Empty text deltas (`{"text":""}`) carry no content — drop them, as the TS does.
                    if !text.is_empty() {
                        if state.blocks.get(&index) != Some(&OpenBlock::Text) {
                            state.blocks.insert(index, OpenBlock::Text);
                            out.push(LlmEvent::TextStart { id: text_id(index) });
                        }
                        out.push(LlmEvent::TextDelta {
                            id: text_id(index),
                            text,
                        });
                    }
                }
                if let Some(reasoning) = delta.reasoning_content {
                    if let Some(text) = reasoning.text.filter(|t| !t.is_empty()) {
                        if state.blocks.get(&index) != Some(&OpenBlock::Reasoning) {
                            state.blocks.insert(index, OpenBlock::Reasoning);
                            out.push(LlmEvent::ReasoningStart {
                                id: reasoning_id(index),
                            });
                        }
                        out.push(LlmEvent::ReasoningDelta {
                            id: reasoning_id(index),
                            text,
                        });
                    }
                }
                if let Some(tool) = delta.tool_use {
                    if let Some(buf) = state.tools.get_mut(&index) {
                        if !tool.input.is_empty() {
                            buf.input.push_str(&tool.input);
                            out.push(LlmEvent::ToolInputDelta {
                                id: buf.id.clone(),
                                text: tool.input,
                            });
                        }
                    }
                }
            }
            return out;
        }

        // A block closes: finalize a tool call, or end an open text/reasoning block.
        if let Some(stop) = event.content_block_stop {
            let index = stop.content_block_index;
            if let Some(buf) = state.tools.remove(&index) {
                state.has_tool_calls = true;
                out.push(LlmEvent::ToolInputEnd { id: buf.id.clone() });
                out.push(LlmEvent::ToolCall {
                    id: buf.id,
                    name: buf.name,
                    input: parse_tool_input(&buf.input),
                });
            } else if let Some(block) = state.blocks.remove(&index) {
                match block {
                    OpenBlock::Text => out.push(LlmEvent::TextEnd { id: text_id(index) }),
                    OpenBlock::Reasoning => out.push(LlmEvent::ReasoningEnd {
                        id: reasoning_id(index),
                    }),
                }
            }
            return out;
        }

        // Finish is split: `messageStop` carries the reason, `metadata` the usage — both held for
        // `on_halt` so exactly one terminal `Finish` is emitted after both have had a chance to arrive.
        if let Some(stop) = event.message_stop {
            state.pending_reason = Some(map_finish_reason(&stop.stop_reason));
            return out;
        }
        if let Some(metadata) = event.metadata {
            state.pending_usage = metadata.usage.map(map_usage);
            state.pending_reason.get_or_insert(FinishReason::Stop);
            return out;
        }

        if let Some(message) = event.error_message() {
            out.push(LlmEvent::ProviderError { message });
        }
        out
    }

    fn terminal(&self, _event: &BedrockEvent) -> bool {
        // No terminal frame: `metadata` (usage) follows `messageStop`, so we drain to end of stream and
        // flush the terminal `Finish` in `on_halt`.
        false
    }

    fn on_halt(&self, state: &BedrockState) -> Vec<LlmEvent> {
        let mut out = Vec::new();
        // Close any block left open (defensive — `contentBlockStop` normally closes them); ordered by
        // index for determinism.
        let mut open: Vec<(u32, OpenBlock)> = state.blocks.iter().map(|(k, v)| (*k, *v)).collect();
        open.sort_by_key(|(index, _)| *index);
        for (index, block) in open {
            match block {
                OpenBlock::Text => out.push(LlmEvent::TextEnd { id: text_id(index) }),
                OpenBlock::Reasoning => out.push(LlmEvent::ReasoningEnd {
                    id: reasoning_id(index),
                }),
            }
        }
        // Only finish once `messageStop`/`metadata` arrived.
        if let Some(reason) = state.pending_reason {
            // A model that emits tool calls but reports a plain stop still finished to run tools.
            let reason = if reason == FinishReason::Stop && state.has_tool_calls {
                FinishReason::ToolCalls
            } else {
                reason
            };
            let usage = state.pending_usage;
            out.push(LlmEvent::StepFinish { reason, usage });
            out.push(LlmEvent::Finish { reason, usage });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{decode_eventstream, Message};
    use base64::Engine;

    // Real recorded cassettes (request.body + response.body) from
    // `packages/llm/test/fixtures/recordings/bedrock-converse/`.
    const STREAMS_TEXT: &str =
        include_str!("../fixtures/recordings/bedrock-converse/streams-text.json");
    const STREAMS_TOOL_CALL: &str =
        include_str!("../fixtures/recordings/bedrock-converse/streams-a-tool-call.json");
    const DRIVES_TOOL_LOOP: &str =
        include_str!("../fixtures/recordings/bedrock-converse/drives-a-tool-loop.json");

    /// Base64-decode the `response.body` of the nth interaction in a cassette.
    fn response_bytes(cassette: &str, interaction: usize) -> Vec<u8> {
        let doc: Value = serde_json::from_str(cassette).unwrap();
        let body = doc["interactions"][interaction]["response"]["body"]
            .as_str()
            .unwrap();
        base64::engine::general_purpose::STANDARD
            .decode(body)
            .unwrap()
    }

    fn decode(cassette: &str, interaction: usize) -> Vec<LlmEvent> {
        decode_eventstream(&BedrockConverse, &response_bytes(cassette, interaction)).unwrap()
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
        serde_json::to_value(BedrockConverse.build_body(request).unwrap()).unwrap()
    }

    #[test]
    fn decodes_text_stream() {
        let events = decode(STREAMS_TEXT, 0);
        assert_eq!(collected_text(&events), "Hello");
        assert!(matches!(events.first(), Some(LlmEvent::TextStart { .. })));
        assert!(events.iter().any(|e| matches!(e, LlmEvent::TextEnd { .. })));
        match events.last() {
            Some(LlmEvent::Finish { reason, usage }) => {
                assert_eq!(*reason, FinishReason::Stop);
                let usage = usage.unwrap();
                assert_eq!(usage.input, 12);
                assert_eq!(usage.output, 2);
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
                LlmEvent::ToolCall { name, input, .. } => Some((name.clone(), input.clone())),
                _ => None,
            })
            .expect("a tool-call event");
        assert_eq!(call.0, "get_weather");
        assert_eq!(call.1, json!({ "city": "Paris" }));
        assert!(events
            .iter()
            .any(|e| matches!(e, LlmEvent::ToolInputStart { .. })));
        assert!(events
            .iter()
            .any(|e| matches!(e, LlmEvent::ToolInputEnd { .. })));
        match events.last() {
            Some(LlmEvent::Finish { reason, usage }) => {
                assert_eq!(*reason, FinishReason::ToolCalls);
                let usage = usage.unwrap();
                assert_eq!(usage.input, 419);
                assert_eq!(usage.output, 16);
            }
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    #[test]
    fn decodes_tool_loop_both_turns() {
        // Turn 0: a `<thinking>` text block, then a tool call → finishes for tool-calls.
        let first = decode(DRIVES_TOOL_LOOP, 0);
        assert!(first.iter().any(|e| matches!(e, LlmEvent::ToolCall { .. })));
        assert!(first.iter().any(|e| matches!(e, LlmEvent::TextEnd { .. })));
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
            collected_text(&second).contains("sunny"),
            "answer was {:?}",
            collected_text(&second)
        );
        match second.last() {
            Some(LlmEvent::Finish { reason, usage }) => {
                assert_eq!(*reason, FinishReason::Stop);
                assert_eq!(usage.unwrap().input, 510);
            }
            other => panic!("expected Finish, got {other:?}"),
        }
    }

    #[test]
    fn lowers_text_request_to_cassette_body() {
        let request = LlmRequest {
            model: "us.amazon.nova-micro-v1:0".into(),
            system: vec!["Reply with the single word 'Hello'.".into()],
            messages: vec![Message::user_text("Say hello.")],
            generation: Generation {
                max_tokens: Some(16),
                temperature: Some(0.0),
                ..Default::default()
            },
            ..Default::default()
        };
        let body = body_value(&request);
        assert_eq!(body["modelId"], "us.amazon.nova-micro-v1:0");
        assert_eq!(
            body["messages"],
            json!([{ "role": "user", "content": [{ "text": "Say hello." }] }])
        );
        assert_eq!(
            body["system"],
            json!([{ "text": "Reply with the single word 'Hello'." }])
        );
        assert_eq!(body["inferenceConfig"]["maxTokens"], 16);
        assert_eq!(body["inferenceConfig"]["temperature"].as_f64(), Some(0.0));
        assert!(body.get("toolConfig").is_none());
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
            model: "us.amazon.nova-micro-v1:0".into(),
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
            body["toolConfig"]["tools"],
            json!([{
                "toolSpec": {
                    "name": "get_weather",
                    "description": "Get current weather for a city.",
                    "inputSchema": { "json": schema }
                }
            }])
        );
        assert_eq!(
            body["toolConfig"]["toolChoice"],
            json!({ "tool": { "name": "get_weather" } })
        );
    }

    #[test]
    fn lowers_tool_loop_messages_to_cassette_body() {
        // The second turn replays the assistant text + toolUse and the user toolResult.
        let request = LlmRequest {
            model: "us.amazon.nova-micro-v1:0".into(),
            system: vec!["Use the get_weather tool, then answer in one short sentence.".into()],
            messages: vec![
                Message::user_text("What is the weather in Paris?"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentPart::Text(
                            "<thinking> To determine the weather in Paris, I will use the \
                             get_weather tool and provide the city as \"Paris\". </thinking>\n"
                                .into(),
                        ),
                        ContentPart::ToolCall {
                            id: "tooluse_a8nlf2bqGLcZvaSoBpQ1sH".into(),
                            name: "get_weather".into(),
                            input: json!({ "city": "Paris" }),
                        },
                    ],
                },
                Message {
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult {
                        id: "tooluse_a8nlf2bqGLcZvaSoBpQ1sH".into(),
                        name: "get_weather".into(),
                        result: json!({ "temperature": 22, "condition": "sunny" }),
                    }],
                },
            ],
            tools: vec![ToolDefinition {
                name: "get_weather".into(),
                description: Some("Get current weather for a city.".into()),
                input_schema: json!({
                    "type": "object",
                    "properties": { "city": { "type": "string" } },
                    "required": ["city"],
                    "additionalProperties": false
                }),
            }],
            generation: Generation {
                max_tokens: Some(80),
                temperature: Some(0.0),
                ..Default::default()
            },
            ..Default::default()
        };
        let body = body_value(&request);
        assert_eq!(
            body["messages"],
            json!([
                { "role": "user", "content": [{ "text": "What is the weather in Paris?" }] },
                { "role": "assistant", "content": [
                    { "text": "<thinking> To determine the weather in Paris, I will use the get_weather tool and provide the city as \"Paris\". </thinking>\n" },
                    { "toolUse": { "toolUseId": "tooluse_a8nlf2bqGLcZvaSoBpQ1sH", "name": "get_weather", "input": { "city": "Paris" } } }
                ]},
                { "role": "user", "content": [
                    { "toolResult": { "toolUseId": "tooluse_a8nlf2bqGLcZvaSoBpQ1sH", "content": [{ "json": { "temperature": 22, "condition": "sunny" } }], "status": "success" } }
                ]}
            ])
        );
        // No tool choice this turn → omitted; the tools list is still present.
        assert!(body["toolConfig"]["toolChoice"].is_null());
        assert_eq!(
            body["toolConfig"]["tools"][0]["toolSpec"]["name"],
            "get_weather"
        );
    }
}
