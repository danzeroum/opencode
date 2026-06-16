//! `anthropic-messages` protocol — the default provider, ported from
//! `packages/llm/src/protocols/anthropic-messages.ts` (decode side).
//!
//! Decodes the Anthropic Messages SSE stream (`message_start`, `content_block_start/delta/stop`,
//! `message_delta`, `message_stop`, `ping`, `error`) into the normalized [`LlmEvent`] stream, threading
//! a [`AnthropicState`] for open content blocks, streaming tool-argument buffers, and usage. Request
//! lowering (`body.from`) and the live transport are the next increment.

use std::collections::HashMap;

use serde::Deserialize;

use crate::{FinishReason, LlmError, LlmEvent, Protocol, Usage};

/// The `anthropic-messages` protocol.
pub struct AnthropicMessages;

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
    type Event = AnthropicEvent;
    type State = AnthropicState;

    fn name(&self) -> &'static str {
        "anthropic-messages"
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
    use crate::decode_sse;

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
}
