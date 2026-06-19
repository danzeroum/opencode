//! V2 session-message **timeline** types — the `SessionMessagesResponse` model returned by
//! `v2.session.messages` (`GET /api/session/{sessionID}/message`).
//!
//! Port target: the TS `SessionMessage` union (`packages/core/src/session/projector.ts`) — a *derived*
//! timeline of typed entries (not the raw event-sourced message/part rows). The union is discriminated
//! on `type`; the assistant entry carries a `content` array (text / reasoning / tool) and each tool
//! carries a `state` discriminated on `status`. Free-form `{ type: "object" }` blocks (`metadata`,
//! tool `input`/`structured`/`result`) are kept as `serde_json::Value`.
//!
//! These are pure wire types (no projection logic yet); the read store, the projector that builds this
//! timeline, and the route cutover are follow-up slices.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use utoipa::ToSchema;

// ===================================================================================================
// Shared leaves
// ===================================================================================================

/// `{ created }` — the timestamp block carried by most timeline entries.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct MessageTime {
    /// Creation time (epoch-millis).
    pub created: f64,
}

/// `{ created, completed? }` — the timestamp block for shell + assistant entries (which can finish).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct MessageTimeCompleted {
    /// Creation time (epoch-millis).
    pub created: f64,
    /// Completion time (epoch-millis), once finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<f64>,
}

/// `{ created, ran?, completed?, pruned? }` — a tool call's lifecycle timestamps.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ToolTime {
    /// When the call was created (epoch-millis).
    pub created: f64,
    /// When execution started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ran: Option<f64>,
    /// When execution completed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<f64>,
    /// When the call's output was pruned from context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pruned: Option<f64>,
}

/// `{ id, providerID, variant? }` — a model reference (used by model-switched + assistant entries).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SessionModelRef {
    /// Model id.
    pub id: String,
    /// Provider id.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// Experimental-mode variant id, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// `{ start, end, text }` — the source span a prompt attachment was parsed from.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct PromptSource {
    /// Start offset in the prompt.
    pub start: f64,
    /// End offset in the prompt.
    pub end: f64,
    /// The matched text.
    pub text: String,
}

/// A file attached to a user prompt (`{ uri, mime, name?, description?, source? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct PromptFileAttachment {
    /// File URI.
    pub uri: String,
    /// MIME type.
    pub mime: String,
    /// Display name, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Description, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where in the prompt this attachment was referenced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PromptSource>,
}

/// An agent mentioned in a user prompt (`{ name, source? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct PromptAgentAttachment {
    /// Agent name.
    pub name: String,
    /// Where in the prompt this `@agent` was referenced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PromptSource>,
}

/// `{ type: "unknown", message }` — the catch-all session error attached to assistant/tool entries.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SessionErrorUnknown {
    /// Always `"unknown"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Human-readable error message.
    pub message: String,
}

// ===================================================================================================
// Tool content + state
// ===================================================================================================

/// A tool-output content block (`text` or `file`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ToolContent {
    /// `{ type: "text", text }`.
    Text {
        /// The text content.
        text: String,
    },
    /// `{ type: "file", uri, mime, name? }`.
    File {
        /// File URI.
        uri: String,
        /// MIME type.
        mime: String,
        /// Display name, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        name: Option<String>,
    },
}

/// A tool call's state, discriminated on `status` (`pending → running → completed | error`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum SessionMessageToolState {
    /// Awaiting execution; `input` is the raw (still-streaming) argument string.
    Pending {
        /// Raw input string as streamed so far.
        input: String,
    },
    /// Executing; `input` is the parsed argument object.
    Running {
        /// Parsed input arguments.
        #[schema(value_type = Object)]
        input: Value,
        /// Structured partial output.
        #[schema(value_type = Object)]
        structured: Value,
        /// Output content blocks.
        content: Vec<ToolContent>,
    },
    /// Finished successfully.
    Completed {
        /// Parsed input arguments.
        #[schema(value_type = Object)]
        input: Value,
        /// Files produced by the tool, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        attachments: Option<Vec<PromptFileAttachment>>,
        /// Output content blocks.
        content: Vec<ToolContent>,
        /// Paths written by the tool, if any.
        #[serde(rename = "outputPaths", skip_serializing_if = "Option::is_none")]
        output_paths: Option<Vec<String>>,
        /// Structured output.
        #[schema(value_type = Object)]
        structured: Value,
        /// Raw result (free-form), if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
    /// Failed.
    Error {
        /// Parsed input arguments.
        #[schema(value_type = Object)]
        input: Value,
        /// Output content blocks captured before failure.
        content: Vec<ToolContent>,
        /// Structured output.
        #[schema(value_type = Object)]
        structured: Value,
        /// The failure.
        error: SessionErrorUnknown,
        /// Raw result (free-form), if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
    },
}

/// The AI-SDK provider annotations on an assistant tool call (`{ executed, metadata?, resultMetadata? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct AssistantToolProvider {
    /// Whether the provider executed the tool itself (server-side tool use).
    pub executed: bool,
    /// Provider call metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub metadata: Option<Value>,
    /// Provider result metadata.
    #[serde(rename = "resultMetadata", skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub result_metadata: Option<Value>,
}

/// A content block of an assistant message (`text` / `reasoning` / `tool`).
// Variant sizes are dictated by the wire contract (the `tool` block is far larger than `text`); these
// are short-lived deserialized DTOs, so the stack-size lint doesn't apply — boxing would only add
// indirection without changing the wire/schema.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SessionMessageAssistantContent {
    /// `{ type: "text", id, text }`.
    Text {
        /// Content-block id.
        id: String,
        /// The text.
        text: String,
    },
    /// `{ type: "reasoning", id, text, providerMetadata? }`.
    Reasoning {
        /// Content-block id.
        id: String,
        /// The reasoning text.
        text: String,
        /// Provider-specific reasoning metadata.
        #[serde(rename = "providerMetadata", skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Object)]
        provider_metadata: Option<Value>,
    },
    /// `{ type: "tool", id, name, provider?, state, time }`.
    Tool {
        /// Content-block id.
        id: String,
        /// Tool name.
        name: String,
        /// Provider tool annotations, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<AssistantToolProvider>,
        /// Call state.
        state: SessionMessageToolState,
        /// Call lifecycle timestamps.
        time: ToolTime,
    },
}

// ===================================================================================================
// Assistant sub-objects
// ===================================================================================================

/// `{ start?, end? }` — the snapshot span an assistant turn produced.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct AssistantSnapshot {
    /// Snapshot id at turn start.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// Snapshot id at turn end.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
}

/// `{ read, write }` — prompt-cache token counts.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct TokenCache {
    /// Cache-read tokens.
    pub read: f64,
    /// Cache-write tokens.
    pub write: f64,
}

/// `{ input, output, reasoning, cache }` — an assistant turn's token usage.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct AssistantTokens {
    /// Input tokens.
    pub input: f64,
    /// Output tokens.
    pub output: f64,
    /// Reasoning tokens.
    pub reasoning: f64,
    /// Prompt-cache token counts.
    pub cache: TokenCache,
}

// ===================================================================================================
// The SessionMessage timeline union
// ===================================================================================================

/// A single entry in the V2 session timeline, discriminated on `type`. Mirrors the TS `SessionMessage`
/// union built by the projector.
// As above: the `assistant` variant is inherently far larger than markers like `agent-switched`; these
// are short-lived wire DTOs, so boxing to satisfy the stack-size lint would only add indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SessionMessage {
    /// The active agent changed.
    AgentSwitched {
        /// Message id (`msg_…`).
        id: String,
        /// Free-form metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Object)]
        metadata: Option<Value>,
        /// Timestamp.
        time: MessageTime,
        /// The agent switched to.
        agent: String,
    },
    /// The active model changed.
    ModelSwitched {
        /// Message id (`msg_…`).
        id: String,
        /// Free-form metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Object)]
        metadata: Option<Value>,
        /// Timestamp.
        time: MessageTime,
        /// The model switched to.
        model: SessionModelRef,
    },
    /// A user prompt.
    User {
        /// Message id (`msg_…`).
        id: String,
        /// Free-form metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Object)]
        metadata: Option<Value>,
        /// Timestamp.
        time: MessageTime,
        /// The prompt text.
        text: String,
        /// File attachments, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        files: Option<Vec<PromptFileAttachment>>,
        /// `@agent` mentions, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        agents: Option<Vec<PromptAgentAttachment>>,
    },
    /// A synthetic (injected) message.
    Synthetic {
        /// Message id (`msg_…`).
        id: String,
        /// Free-form metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Object)]
        metadata: Option<Value>,
        /// Timestamp.
        time: MessageTime,
        /// Owning session id (`ses_…`).
        #[serde(rename = "sessionID")]
        session_id: String,
        /// The synthetic text.
        text: String,
    },
    /// A system message.
    System {
        /// Message id (`msg_…`).
        id: String,
        /// Free-form metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Object)]
        metadata: Option<Value>,
        /// Timestamp.
        time: MessageTime,
        /// The system text.
        text: String,
    },
    /// A shell command run inline in the session.
    Shell {
        /// Message id (`msg_…`).
        id: String,
        /// Free-form metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Object)]
        metadata: Option<Value>,
        /// Timestamps.
        time: MessageTimeCompleted,
        /// Tool call id.
        #[serde(rename = "callID")]
        call_id: String,
        /// The command.
        command: String,
        /// Captured output.
        output: String,
    },
    /// An assistant turn.
    Assistant {
        /// Message id (`msg_…`).
        id: String,
        /// Free-form metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Object)]
        metadata: Option<Value>,
        /// Timestamps.
        time: MessageTimeCompleted,
        /// The agent that produced the turn.
        agent: String,
        /// The model used.
        model: SessionModelRef,
        /// The turn's content blocks (text / reasoning / tool).
        content: Vec<SessionMessageAssistantContent>,
        /// Snapshot span, if taken.
        #[serde(skip_serializing_if = "Option::is_none")]
        snapshot: Option<AssistantSnapshot>,
        /// Finish reason, once completed.
        #[serde(skip_serializing_if = "Option::is_none")]
        finish: Option<String>,
        /// Turn cost (USD), once completed.
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<f64>,
        /// Token usage, once completed.
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<AssistantTokens>,
        /// Turn error, if it failed.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<SessionErrorUnknown>,
    },
    /// A context compaction marker.
    Compaction {
        /// Message id (`msg_…`).
        id: String,
        /// Free-form metadata.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Object)]
        metadata: Option<Value>,
        /// Timestamp.
        time: MessageTime,
        /// Why compaction ran (`"auto"` | `"manual"`).
        reason: String,
        /// The compaction summary.
        summary: String,
        /// The retained recent tail.
        recent: String,
    },
}

/// Pagination cursor for the timeline (`{ previous?, next? }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct MessageCursor {
    /// Cursor for the previous (older) page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// Cursor for the next (newer) page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// `v2.session.messages` response (`GET /api/session/{sessionID}/message`): a page of timeline entries
/// plus the pagination cursor.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SessionMessagesResponse {
    /// The timeline entries (oldest → newest within the page).
    pub data: Vec<SessionMessage>,
    /// Pagination cursor.
    pub cursor: MessageCursor,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_message_round_trips_with_camelcase_and_kebab_type() {
        let msg = SessionMessage::User {
            id: "msg_1".into(),
            metadata: None,
            time: MessageTime { created: 100.0 },
            text: "hello".into(),
            files: None,
            agents: None,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["text"], "hello");
        assert_eq!(v["time"]["created"], 100.0);
        // Optional empty fields are omitted.
        assert!(v.get("files").is_none());
        assert!(v.get("metadata").is_none());
        let back: SessionMessage = serde_json::from_value(v).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn synthetic_renames_session_id() {
        let v = serde_json::to_value(SessionMessage::Synthetic {
            id: "msg_2".into(),
            metadata: None,
            time: MessageTime { created: 1.0 },
            session_id: "ses_x".into(),
            text: "note".into(),
        })
        .unwrap();
        assert_eq!(v["type"], "synthetic");
        assert_eq!(v["sessionID"], "ses_x");
    }

    #[test]
    fn assistant_with_tool_call_nests_state_on_status() {
        let msg = SessionMessage::Assistant {
            id: "msg_3".into(),
            metadata: None,
            time: MessageTimeCompleted {
                created: 1.0,
                completed: Some(2.0),
            },
            agent: "build".into(),
            model: SessionModelRef {
                id: "claude".into(),
                provider_id: "anthropic".into(),
                variant: None,
            },
            content: vec![SessionMessageAssistantContent::Tool {
                id: "c1".into(),
                name: "read".into(),
                provider: None,
                state: SessionMessageToolState::Completed {
                    input: json!({ "path": "a.rs" }),
                    attachments: None,
                    content: vec![ToolContent::Text { text: "ok".into() }],
                    output_paths: None,
                    structured: json!({}),
                    result: None,
                },
                time: ToolTime {
                    created: 1.0,
                    ran: Some(1.5),
                    completed: Some(2.0),
                    pruned: None,
                },
            }],
            snapshot: None,
            finish: Some("stop".into()),
            cost: Some(0.01),
            tokens: None,
            error: None,
        };
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["type"], "assistant");
        assert_eq!(v["model"]["providerID"], "anthropic");
        assert_eq!(v["content"][0]["type"], "tool");
        assert_eq!(v["content"][0]["state"]["status"], "completed");
        assert_eq!(v["content"][0]["state"]["content"][0]["type"], "text");
        let back: SessionMessage = serde_json::from_value(v).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn response_wraps_data_and_cursor() {
        let resp = SessionMessagesResponse {
            data: vec![],
            cursor: MessageCursor::default(),
        };
        let v = serde_json::to_value(&resp).unwrap();
        assert_eq!(v["data"], json!([]));
        // Empty cursor still serializes as an (empty) object.
        assert!(v["cursor"].is_object());
    }
}
