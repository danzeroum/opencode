//! LLM protocol router — ported from `packages/llm` (Phase 3).
//!
//! `packages/llm` is **not** a thin AI-SDK wrapper: it models each provider family as a pipeline
//! `Protocol<Body, Frame, Event, State>` (`route/protocol.ts`). Off-the-shelf crates (`reqwest`,
//! `aws-sdk-bedrockruntime`) sit *behind* the [`Protocol`] trait.
//!
//! This first increment ports the **decode pipeline** — the parity-critical, network-free core:
//! provider SSE bytes → frames (`data:` payloads) → a provider event → [`Protocol::step`] →
//! normalized [`LlmEvent`]s. Request-body lowering (`body.from`) and the reqwest/SSE transport are
//! the next increment; here the decode is proven against recorded `http-recorder` cassettes
//! ([`anthropic`] tests). Implementation order: anthropic-messages (default) → openai-chat →
//! openai-responses → gemini → bedrock-converse.

pub mod anthropic;
pub mod executor;
pub mod openai_chat;
pub mod transport;

use serde::{Deserialize, Serialize};

/// Why a generation stopped (`packages/llm/src/schema/events.ts` `FinishReason`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FinishReason {
    /// Natural stop / end of turn.
    Stop,
    /// Hit the max-tokens limit.
    Length,
    /// Stopped to run tool calls.
    ToolCalls,
    /// Stopped by a content filter / refusal.
    ContentFilter,
    /// Stopped due to an error.
    Error,
    /// Unknown / unmapped reason.
    Unknown,
}

/// Token usage for a generation. Token counts are *inclusive* (e.g. `input` includes cached tokens),
/// mirroring the normalized `Usage` in `packages/llm`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Total input tokens (including cache read + cache write).
    pub input: u64,
    /// Output tokens (including reasoning, which Anthropic does not break out).
    pub output: u64,
    /// Cache-read input tokens (subset of `input`).
    pub cache_read: u64,
    /// Cache-write (creation) input tokens (subset of `input`).
    pub cache_write: u64,
}

/// The normalized streamed event every protocol emits (`packages/llm/src/schema/events.ts`
/// `LLMEvent`). Content blocks (text/reasoning) and tool calls are identified by a stable `id` shared
/// across their start/delta/end events.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    /// A text block begins.
    TextStart { id: String },
    /// Incremental assistant text.
    TextDelta { id: String, text: String },
    /// A text block ends.
    TextEnd { id: String },
    /// A reasoning/thinking block begins.
    ReasoningStart { id: String },
    /// Incremental reasoning text.
    ReasoningDelta { id: String, text: String },
    /// A reasoning block ends.
    ReasoningEnd { id: String },
    /// A tool call begins streaming its JSON arguments.
    ToolInputStart {
        /// Tool-call id.
        id: String,
        /// Tool name.
        name: String,
    },
    /// Incremental (partial) JSON for a tool call's arguments.
    ToolInputDelta {
        /// Tool-call id.
        id: String,
        /// Partial JSON text.
        text: String,
    },
    /// A tool call's arguments finished streaming.
    ToolInputEnd {
        /// Tool-call id.
        id: String,
    },
    /// A complete, parsed tool call.
    ToolCall {
        /// Tool-call id.
        id: String,
        /// Tool name.
        name: String,
        /// Parsed arguments.
        input: serde_json::Value,
    },
    /// A generation step finished (before the terminal [`LlmEvent::Finish`]).
    StepFinish {
        /// Why the step stopped.
        reason: FinishReason,
        /// Usage so far, if reported.
        usage: Option<Usage>,
    },
    /// The stream terminated.
    Finish {
        /// Why the generation stopped.
        reason: FinishReason,
        /// Final usage, if reported.
        usage: Option<Usage>,
    },
    /// A provider-side error surfaced mid-stream.
    ProviderError {
        /// Human-readable message.
        message: String,
    },
}

/// Errors building, sending, or decoding a protocol stream.
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    /// A frame could not be parsed into the protocol's event type.
    #[error("failed to decode frame: {0}")]
    Decode(String),
    /// A network/transport error reaching the provider.
    #[error("http transport error: {0}")]
    Http(String),
    /// The provider returned a non-success HTTP status.
    #[error("provider returned status {code}: {message}")]
    Status {
        /// HTTP status code.
        code: u16,
        /// Whether the request is worth retrying (the executor consumes this).
        retryable: bool,
        /// `Retry-After` delay in seconds, if the provider sent the header.
        retry_after: Option<u64>,
        /// Response body / error message (secret-redacted).
        message: String,
    },
}

/// A provider-family protocol pipeline. This increment covers the **decode** side: parse one frame
/// into a provider [`Event`](Protocol::Event), fold it into [`State`](Protocol::State) emitting
/// normalized [`LlmEvent`]s, and detect termination — plus the **request** side
/// ([`build_body`](Protocol::build_body)). The reqwest/SSE transport that ties them together lands
/// next.
pub trait Protocol {
    /// The provider-native request body (serialized to JSON by the transport).
    type Body: serde::Serialize;
    /// The provider-native event decoded from one frame (e.g. an Anthropic SSE event).
    type Event;
    /// The streaming accumulator (open blocks, tool buffers, usage).
    type State;

    /// Stable protocol id, e.g. `"anthropic-messages"`.
    fn name(&self) -> &'static str;

    /// Lower a normalized [`LlmRequest`] to the provider-native request body (`body.from`).
    fn build_body(&self, request: &LlmRequest) -> Result<Self::Body, LlmError>;

    /// The initial streaming state.
    fn initial(&self) -> Self::State;

    /// Decode one frame (an SSE `data:` payload) into a provider event.
    fn decode_frame(&self, frame: &str) -> Result<Self::Event, LlmError>;

    /// Fold one event into `state`, emitting normalized events.
    fn step(&self, state: &mut Self::State, event: Self::Event) -> Vec<LlmEvent>;

    /// Whether `event` terminates the stream.
    fn terminal(&self, event: &Self::Event) -> bool;

    /// Flush any pending state at stream end (`onHalt` in TS) — e.g. close an open text block or emit
    /// the terminal `Finish` for protocols (like openai-chat) that split finish/usage across the last
    /// chunks. Default: nothing (protocols that emit everything inline need no flush).
    fn on_halt(&self, _state: &Self::State) -> Vec<LlmEvent> {
        Vec::new()
    }
}

/// A normalized LLM request (`packages/llm/src/schema/messages.ts` `LLMRequest`) — the core subset:
/// model, system text, conversation messages, tools, tool choice, and generation params. Media /
/// reasoning / cache parts and provider options are deferred to a later increment.
#[derive(Debug, Clone, Default)]
pub struct LlmRequest {
    /// Model id (provider-native, e.g. `"claude-haiku-4-5-20251001"`).
    pub model: String,
    /// System prompt parts (each becomes a system text block).
    pub system: Vec<String>,
    /// Conversation messages.
    pub messages: Vec<Message>,
    /// Tool definitions offered to the model.
    pub tools: Vec<ToolDefinition>,
    /// Tool-choice policy.
    pub tool_choice: Option<ToolChoice>,
    /// Generation parameters.
    pub generation: Generation,
}

/// One conversation message.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// The message author.
    pub role: Role,
    /// Ordered content parts.
    pub content: Vec<ContentPart>,
}

impl Message {
    /// A user message with a single text part.
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentPart::Text(text.into())],
        }
    }

    /// An assistant message with a single text part.
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentPart::Text(text.into())],
        }
    }
}

/// A message author (`packages/llm` message roles).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// System role.
    System,
    /// End user.
    User,
    /// The model.
    Assistant,
    /// A tool result author.
    Tool,
}

/// A message content part (core subset).
#[derive(Debug, Clone, PartialEq)]
pub enum ContentPart {
    /// Plain text.
    Text(String),
    /// A tool call the assistant requested.
    ToolCall {
        /// Tool-call id.
        id: String,
        /// Tool name.
        name: String,
        /// Arguments.
        input: serde_json::Value,
    },
    /// A tool result fed back to the model.
    ToolResult {
        /// The tool-call id this result is for.
        id: String,
        /// Tool name.
        name: String,
        /// The result payload.
        result: serde_json::Value,
    },
}

/// A tool definition offered to the model.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDefinition {
    /// Tool name.
    pub name: String,
    /// Human/model-facing description.
    pub description: Option<String>,
    /// JSON Schema for the tool's arguments.
    pub input_schema: serde_json::Value,
}

/// Tool-choice policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoice {
    /// The model decides.
    Auto,
    /// No tools.
    None,
    /// The model must call some tool.
    Required,
    /// The model must call this specific tool.
    Tool(String),
}

/// Generation parameters (`packages/llm` `GenerationOptions`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Generation {
    /// Max output tokens (providers that require it default it when `None`).
    pub max_tokens: Option<u64>,
    /// Sampling temperature.
    pub temperature: Option<f64>,
    /// Nucleus sampling.
    pub top_p: Option<f64>,
    /// Top-k sampling.
    pub top_k: Option<u64>,
    /// Stop sequences.
    pub stop: Vec<String>,
}

/// Extract SSE `data:` payloads from a raw event-stream body (`packages/llm` `sseFraming`): events
/// are separated by blank lines; `data:` lines (one optional leading space stripped) are concatenated
/// with newlines; empty payloads and the `[DONE]` sentinel are dropped.
pub fn sse_frames(body: &str) -> Vec<String> {
    let mut frames = Vec::new();
    let mut data = String::new();
    let mut dispatch = |data: &mut String| {
        let trimmed = data.trim();
        if !trimmed.is_empty() && trimmed != "[DONE]" {
            frames.push(std::mem::take(data));
        } else {
            data.clear();
        }
    };
    for line in body.lines() {
        if line.is_empty() {
            dispatch(&mut data);
            continue;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest);
        }
        // `event:` / `id:` / comment lines are ignored — `step`/`terminal` key off the JSON payload.
    }
    dispatch(&mut data);
    frames
}

/// Run a recorded/streamed SSE `body` through `protocol`, returning the full normalized event stream.
/// This is the network-free decode pipeline used by the parity tests (and, later, fed live frames by
/// the transport).
pub fn decode_sse<P: Protocol>(protocol: &P, body: &str) -> Result<Vec<LlmEvent>, LlmError> {
    let mut state = protocol.initial();
    let mut out = Vec::new();
    for frame in sse_frames(body) {
        let event = protocol.decode_frame(&frame)?;
        let terminal = protocol.terminal(&event);
        out.extend(protocol.step(&mut state, event));
        if terminal {
            break;
        }
    }
    out.extend(protocol.on_halt(&state));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_frames_extracts_data_payloads() {
        let body =
            "event: a\ndata: {\"x\":1}\n\nevent: ping\ndata: {\"type\":\"ping\"}\n\ndata: [DONE]\n\n";
        let frames = sse_frames(body);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].trim(), "{\"x\":1}");
        assert_eq!(frames[1].trim(), "{\"type\":\"ping\"}");
    }

    #[test]
    fn finish_reason_serde_is_kebab_case() {
        assert_eq!(
            serde_json::to_string(&FinishReason::ToolCalls).unwrap(),
            "\"tool-calls\""
        );
    }
}
