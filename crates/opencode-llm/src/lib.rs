//! LLM protocol router — ported from `packages/llm` (Phase 3).
//!
//! `packages/llm` is **not** a thin AI-SDK wrapper: it models each provider family as a pipeline
//! `Protocol<Body, Frame, Event, State>` (see `route/protocol.ts`). The order of implementation is
//! anthropic-messages (default) → openai-chat → openai-responses → gemini → bedrock-converse.
//! Off-the-shelf crates (`reqwest`, `aws-sdk-bedrockruntime`) sit *behind* the [`Protocol`] trait.
//!
//! This is a Phase 0 placeholder fixing the canonical event type and trait shape.

#![allow(dead_code)]

/// Canonical streamed event, ported from `packages/llm/src/schema/events.ts`.
#[derive(Debug, Clone, PartialEq)]
pub enum LlmEvent {
    /// Incremental assistant text.
    TextDelta(String),
    /// Incremental reasoning/thinking text.
    ReasoningDelta(String),
    /// A tool call requested by the model (name + raw JSON arguments).
    ToolCall {
        id: String,
        name: String,
        arguments: String,
    },
    /// Terminal event for a turn.
    Done,
}

/// A provider-family protocol pipeline. Each impl handles body construction, frame→event decoding,
/// the streaming state machine, halt handling, and terminal detection.
pub trait Protocol: Send + Sync {
    /// Stable protocol identifier, e.g. `"anthropic-messages"`.
    fn name(&self) -> &'static str;
}
