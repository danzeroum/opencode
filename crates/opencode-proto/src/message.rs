//! The V1 conversation contract — `Message` (user/assistant) and `Part` (the 12 content-part
//! variants) plus their nested error/source/state unions. Mirrors `packages/core/src/v1/message.ts`
//! and is the shape returned by `session.message`/`session.messages`/`session.command`/`session.shell`
//! (`{ info: Message, parts: [Part] }`).
//!
//! Reuses [`crate::Range`], [`crate::SnapshotFileDiff`] and [`crate::TokenCache`] from the crate root.
//! Discriminator fields (`type`/`role`/`status`/`name`) are modelled as plain `String` — the contract
//! diff drops the single-value `enum` constraint, so a `String` matches the golden literal-enum field.
//! Free-form objects (`metadata`/`input`/`schema`/empty `data`) are `serde_json::Value` with
//! `#[schema(value_type = Object)]`; the truly-any `structured` field is a bare `Value` (golden `{}`).

use crate::{Range, SnapshotFileDiff, TokenCache};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

// ---------------------------------------------------------------------------
// Shared nested objects.
// ---------------------------------------------------------------------------

/// Token usage on a message/step (`{ total?, input, output, reasoning, cache }`). Like
/// [`crate::SessionTokens`] but with an optional `total`; reuses [`TokenCache`].
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct MessageTokens {
    /// Total tokens (input + output), when computed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<f64>,
    /// Input tokens.
    pub input: f64,
    /// Output tokens.
    pub output: f64,
    /// Reasoning tokens.
    pub reasoning: f64,
    /// Cache-token usage.
    pub cache: TokenCache,
}

// ---------------------------------------------------------------------------
// UserMessage.
// ---------------------------------------------------------------------------

/// Creation timestamp of a user message (`{ created }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct UserMessageTime {
    /// Unix-ms creation time.
    pub created: f64,
}

/// The model a user message targeted (`{ providerID, modelID, variant? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct MessageModelRef {
    /// Provider id.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// Model id.
    #[serde(rename = "modelID")]
    pub model_id: String,
    /// Variant id, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// A user message's optional change summary (`{ title?, body?, diffs }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct UserMessageSummary {
    /// Summary title.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Summary body.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Per-file diffs.
    pub diffs: Vec<SnapshotFileDiff>,
}

/// A user-authored message (`role: "user"`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct UserMessage {
    /// Message id (`msg_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Always `"user"`.
    pub role: String,
    /// Creation time.
    pub time: UserMessageTime,
    /// Requested output format, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<OutputFormat>,
    /// Change summary, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<UserMessageSummary>,
    /// Agent name.
    pub agent: String,
    /// Target model.
    pub model: MessageModelRef,
    /// System prompt override, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Per-tool enable map (`{ [tool]: boolean }`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub tools: Option<serde_json::Value>,
}

/// The requested output format of a turn (`text` | `json_schema`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(untagged)]
pub enum OutputFormat {
    /// Free-form text.
    Text(OutputFormatText),
    /// Structured output validated against a JSON Schema.
    JsonSchema(OutputFormatJsonSchema),
}

/// `{ type: "text" }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct OutputFormatText {
    /// Always `"text"`.
    pub r#type: String,
}

/// `{ type: "json_schema", schema, retryCount? }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct OutputFormatJsonSchema {
    /// Always `"json_schema"`.
    pub r#type: String,
    /// The JSON Schema the output must satisfy (free-form schema object).
    #[schema(value_type = Object)]
    pub schema: serde_json::Value,
    /// How many structured-output retries have been attempted.
    #[serde(rename = "retryCount", skip_serializing_if = "Option::is_none")]
    pub retry_count: Option<i64>,
}

// ---------------------------------------------------------------------------
// AssistantMessage.
// ---------------------------------------------------------------------------

/// Creation/completion timestamps of an assistant message (`{ created, completed? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct AssistantMessageTime {
    /// Unix-ms creation time.
    pub created: i64,
    /// Unix-ms completion time, once the turn finishes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed: Option<i64>,
}

/// The working directory pair of an assistant turn (`{ cwd, root }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct MessagePath {
    /// Working directory the turn ran in.
    pub cwd: String,
    /// Project root.
    pub root: String,
}

/// An assistant-authored message (`role: "assistant"`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct AssistantMessage {
    /// Message id (`msg_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Always `"assistant"`.
    pub role: String,
    /// Creation/completion time.
    pub time: AssistantMessageTime,
    /// A typed error that ended the turn, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<MessageError>,
    /// The user message that prompted this turn (`msg_…`).
    #[serde(rename = "parentID")]
    pub parent_id: String,
    /// Model id.
    #[serde(rename = "modelID")]
    pub model_id: String,
    /// Provider id.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// Resolved agent mode.
    pub mode: String,
    /// Agent name.
    pub agent: String,
    /// Working directories.
    pub path: MessagePath,
    /// Whether this message is a compaction summary.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<bool>,
    /// Accumulated cost in USD.
    pub cost: f64,
    /// Token usage.
    pub tokens: MessageTokens,
    /// Structured output value, when an output schema was requested (any JSON value).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured: Option<serde_json::Value>,
    /// Model variant, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// Finish reason reported by the provider, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finish: Option<String>,
}

// ---------------------------------------------------------------------------
// AssistantMessage.error — the typed error union (`{ name, data }` per arm).
// ---------------------------------------------------------------------------

/// A typed error that ended an assistant turn. Each arm is `{ name, data }` with `name` the
/// discriminator literal.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(untagged)]
pub enum MessageError {
    /// Provider authentication failed.
    ProviderAuth(ProviderAuthError),
    /// Unknown/uncategorised error.
    Unknown(MessageUnknownError),
    /// The model hit its output-length limit.
    OutputLength(MessageOutputLengthError),
    /// The turn was aborted.
    Aborted(MessageAbortedError),
    /// Structured output could not be produced.
    StructuredOutput(StructuredOutputError),
    /// The context window overflowed.
    ContextOverflow(ContextOverflowError),
    /// The provider's content filter blocked the response.
    ContentFilter(ContentFilterError),
    /// A provider API error.
    Api(ApiError),
}

/// `{ name: "ProviderAuthError", data }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProviderAuthError {
    /// Always `"ProviderAuthError"`.
    pub name: String,
    /// Error payload.
    pub data: ProviderAuthErrorData,
}

/// `{ providerID, message }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProviderAuthErrorData {
    /// The provider that failed authentication.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// Human-readable message.
    pub message: String,
}

/// `{ name: "UnknownError", data }` — the message-level unknown error (distinct from the crate-root
/// [`crate::UnknownError`] 500 envelope).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct MessageUnknownError {
    /// Always `"UnknownError"`.
    pub name: String,
    /// Error payload.
    pub data: MessageUnknownErrorData,
}

/// `{ message, ref? }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct MessageUnknownErrorData {
    /// Human-readable message.
    pub message: String,
    /// Optional log-reference id.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// `{ name: "MessageOutputLengthError", data: {} }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct MessageOutputLengthError {
    /// Always `"MessageOutputLengthError"`.
    pub name: String,
    /// Empty payload object.
    #[schema(value_type = Object)]
    pub data: serde_json::Value,
}

/// `{ name: "MessageAbortedError", data }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct MessageAbortedError {
    /// Always `"MessageAbortedError"`.
    pub name: String,
    /// Error payload.
    pub data: MessageAbortedErrorData,
}

/// `{ message }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct MessageAbortedErrorData {
    /// Human-readable message.
    pub message: String,
}

/// `{ name: "StructuredOutputError", data }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct StructuredOutputError {
    /// Always `"StructuredOutputError"`.
    pub name: String,
    /// Error payload.
    pub data: StructuredOutputErrorData,
}

/// `{ message, retries }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct StructuredOutputErrorData {
    /// Human-readable message.
    pub message: String,
    /// How many retries were attempted.
    pub retries: i64,
}

/// `{ name: "ContextOverflowError", data }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ContextOverflowError {
    /// Always `"ContextOverflowError"`.
    pub name: String,
    /// Error payload.
    pub data: ContextOverflowErrorData,
}

/// `{ message, responseBody? }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ContextOverflowErrorData {
    /// Human-readable message.
    pub message: String,
    /// The provider response body, if captured.
    #[serde(rename = "responseBody", skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
}

/// `{ name: "ContentFilterError", data }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ContentFilterError {
    /// Always `"ContentFilterError"`.
    pub name: String,
    /// Error payload.
    pub data: ContentFilterErrorData,
}

/// `{ message }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ContentFilterErrorData {
    /// Human-readable message.
    pub message: String,
}

/// `{ name: "APIError", data }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ApiError {
    /// Always `"APIError"`.
    pub name: String,
    /// Error payload.
    pub data: ApiErrorData,
}

/// `{ message, statusCode?, isRetryable, responseHeaders?, responseBody?, metadata? }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ApiErrorData {
    /// Human-readable message.
    pub message: String,
    /// HTTP status code, if any.
    #[serde(rename = "statusCode", skip_serializing_if = "Option::is_none")]
    pub status_code: Option<i64>,
    /// Whether the request may be retried.
    #[serde(rename = "isRetryable")]
    pub is_retryable: bool,
    /// Response headers (`{ [name]: value }`), if captured.
    #[serde(rename = "responseHeaders", skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub response_headers: Option<serde_json::Value>,
    /// Response body, if captured.
    #[serde(rename = "responseBody", skip_serializing_if = "Option::is_none")]
    pub response_body: Option<String>,
    /// Provider-specific metadata (`{ [key]: value }`), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub metadata: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Message union.
// ---------------------------------------------------------------------------

/// A conversation message — a user prompt or an assistant turn.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(untagged)]
pub enum Message {
    /// A user-authored message.
    User(UserMessage),
    /// An assistant-authored message.
    Assistant(AssistantMessage),
}

// ---------------------------------------------------------------------------
// Part variants.
// ---------------------------------------------------------------------------

/// Start/end timestamps shared by text/reasoning parts (`{ start, end? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PartTime {
    /// Unix-ms start.
    pub start: i64,
    /// Unix-ms end, once finished.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
}

/// A text content part.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct TextPart {
    /// Part id (`prt_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Owning message id (`msg_…`).
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Always `"text"`.
    pub r#type: String,
    /// The text.
    pub text: String,
    /// Whether this part was synthesised (not produced by the model).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub synthetic: Option<bool>,
    /// Whether this part is ignored for context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ignored: Option<bool>,
    /// Timing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<PartTime>,
    /// Free-form metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub metadata: Option<serde_json::Value>,
}

/// The model a subtask targeted (`{ providerID, modelID }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SubtaskModelRef {
    /// Provider id.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// Model id.
    #[serde(rename = "modelID")]
    pub model_id: String,
}

/// A subtask (sub-agent invocation) part.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SubtaskPart {
    /// Part id (`prt_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Owning message id (`msg_…`).
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Always `"subtask"`.
    pub r#type: String,
    /// The subtask prompt.
    pub prompt: String,
    /// Human-readable description.
    pub description: String,
    /// Agent that runs the subtask.
    pub agent: String,
    /// Target model, if pinned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<SubtaskModelRef>,
    /// Originating command, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

/// A reasoning content part.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ReasoningPart {
    /// Part id (`prt_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Owning message id (`msg_…`).
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Always `"reasoning"`.
    pub r#type: String,
    /// The reasoning text.
    pub text: String,
    /// Free-form metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub metadata: Option<serde_json::Value>,
    /// Timing.
    pub time: PartTime,
}

/// A range of bytes within a referenced file's text (`{ value, start, end }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct FilePartSourceText {
    /// The referenced text.
    pub value: String,
    /// Start offset.
    pub start: f64,
    /// End offset.
    pub end: f64,
}

/// A plain file source (`{ text, type: "file", path }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct FileSource {
    /// The referenced text span.
    pub text: FilePartSourceText,
    /// Always `"file"`.
    pub r#type: String,
    /// File path.
    pub path: String,
}

/// A symbol source (`{ text, type: "symbol", path, range, name, kind }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SymbolSource {
    /// The referenced text span.
    pub text: FilePartSourceText,
    /// Always `"symbol"`.
    pub r#type: String,
    /// File path.
    pub path: String,
    /// Symbol range.
    pub range: Range,
    /// Symbol name.
    pub name: String,
    /// LSP symbol-kind code.
    pub kind: i64,
}

/// An MCP resource source (`{ text, type: "resource", clientName, uri }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ResourceSource {
    /// The referenced text span.
    pub text: FilePartSourceText,
    /// Always `"resource"`.
    pub r#type: String,
    /// MCP client name.
    #[serde(rename = "clientName")]
    pub client_name: String,
    /// Resource URI.
    pub uri: String,
}

/// Where a [`FilePart`] came from — a file, a symbol, or an MCP resource.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(untagged)]
pub enum FilePartSource {
    /// A file on disk.
    File(FileSource),
    /// A code symbol.
    Symbol(SymbolSource),
    /// An MCP resource.
    Resource(ResourceSource),
}

/// A file-attachment part.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct FilePart {
    /// Part id (`prt_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Owning message id (`msg_…`).
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Always `"file"`.
    pub r#type: String,
    /// MIME type.
    pub mime: String,
    /// File name, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    /// File URL/data URI.
    pub url: String,
    /// Where the file came from, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<FilePartSource>,
}

/// `{ start }` — the running tool's start time.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ToolTimeStart {
    /// Unix-ms start.
    pub start: i64,
}

/// `{ start, end }` — an errored tool's timing.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ToolTimeStartEnd {
    /// Unix-ms start.
    pub start: i64,
    /// Unix-ms end.
    pub end: i64,
}

/// `{ start, end, compacted? }` — a completed tool's timing.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ToolTimeCompleted {
    /// Unix-ms start.
    pub start: i64,
    /// Unix-ms end.
    pub end: i64,
    /// Unix-ms time the output was compacted away, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compacted: Option<i64>,
}

/// The tool call has been parsed but not yet started.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ToolStatePending {
    /// Always `"pending"`.
    pub status: String,
    /// Parsed tool input (free-form object).
    #[schema(value_type = Object)]
    pub input: serde_json::Value,
    /// Raw, unparsed input string.
    pub raw: String,
}

/// The tool call is running.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ToolStateRunning {
    /// Always `"running"`.
    pub status: String,
    /// Parsed tool input (free-form object).
    #[schema(value_type = Object)]
    pub input: serde_json::Value,
    /// Display title, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Free-form metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub metadata: Option<serde_json::Value>,
    /// Timing.
    pub time: ToolTimeStart,
}

/// The tool call completed successfully.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ToolStateCompleted {
    /// Always `"completed"`.
    pub status: String,
    /// Parsed tool input (free-form object).
    #[schema(value_type = Object)]
    pub input: serde_json::Value,
    /// Tool output.
    pub output: String,
    /// Display title.
    pub title: String,
    /// Free-form metadata.
    #[schema(value_type = Object)]
    pub metadata: serde_json::Value,
    /// Timing.
    pub time: ToolTimeCompleted,
    /// File parts produced as attachments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachments: Option<Vec<FilePart>>,
}

/// The tool call failed.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ToolStateError {
    /// Always `"error"`.
    pub status: String,
    /// Parsed tool input (free-form object).
    #[schema(value_type = Object)]
    pub input: serde_json::Value,
    /// Error message.
    pub error: String,
    /// Free-form metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub metadata: Option<serde_json::Value>,
    /// Timing.
    pub time: ToolTimeStartEnd,
}

/// The lifecycle state of a tool call.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(untagged)]
pub enum ToolState {
    /// Parsed, not yet started.
    Pending(ToolStatePending),
    /// Running.
    Running(ToolStateRunning),
    /// Completed successfully.
    Completed(ToolStateCompleted),
    /// Failed.
    Error(ToolStateError),
}

/// A tool-call part.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ToolPart {
    /// Part id (`prt_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Owning message id (`msg_…`).
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Always `"tool"`.
    pub r#type: String,
    /// Provider tool-call id.
    #[serde(rename = "callID")]
    pub call_id: String,
    /// Tool name.
    pub tool: String,
    /// Call lifecycle state.
    pub state: ToolState,
    /// Free-form metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub metadata: Option<serde_json::Value>,
}

/// A step-start marker part.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct StepStartPart {
    /// Part id (`prt_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Owning message id (`msg_…`).
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Always `"step-start"`.
    pub r#type: String,
    /// Snapshot id taken at step start, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
}

/// A step-finish marker part (carries the step's cost/usage).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct StepFinishPart {
    /// Part id (`prt_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Owning message id (`msg_…`).
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Always `"step-finish"`.
    pub r#type: String,
    /// Finish reason.
    pub reason: String,
    /// Snapshot id taken at step finish, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// Step cost in USD.
    pub cost: f64,
    /// Step token usage.
    pub tokens: MessageTokens,
}

/// A filesystem-snapshot marker part.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SnapshotPart {
    /// Part id (`prt_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Owning message id (`msg_…`).
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Always `"snapshot"`.
    pub r#type: String,
    /// Snapshot id.
    pub snapshot: String,
}

/// A patch part (a set of files changed under one hash).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PatchPart {
    /// Part id (`prt_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Owning message id (`msg_…`).
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Always `"patch"`.
    pub r#type: String,
    /// Patch hash.
    pub hash: String,
    /// Files touched by the patch.
    pub files: Vec<String>,
}

/// The text span an [`AgentPart`] references (`{ value, start, end }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct AgentSource {
    /// The referenced text.
    pub value: String,
    /// Start offset.
    pub start: i64,
    /// End offset.
    pub end: i64,
}

/// An agent-mention part.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct AgentPart {
    /// Part id (`prt_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Owning message id (`msg_…`).
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Always `"agent"`.
    pub r#type: String,
    /// Agent name.
    pub name: String,
    /// Source span of the mention, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<AgentSource>,
}

/// `{ created }` — a retry's timestamp.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct RetryTime {
    /// Unix-ms creation time.
    pub created: i64,
}

/// A retry part (records a provider error and the attempt it triggered).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct RetryPart {
    /// Part id (`prt_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Owning message id (`msg_…`).
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Always `"retry"`.
    pub r#type: String,
    /// Attempt number.
    pub attempt: i64,
    /// The API error that triggered the retry.
    pub error: ApiError,
    /// Timing.
    pub time: RetryTime,
}

/// A compaction part (marks where the conversation was summarised/compacted).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct CompactionPart {
    /// Part id (`prt_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Owning message id (`msg_…`).
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Always `"compaction"`.
    pub r#type: String,
    /// Whether the compaction was automatic.
    pub auto: bool,
    /// Whether it was triggered by context overflow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overflow: Option<bool>,
    /// First message id kept in the retained tail (`msg_…`), if any.
    #[serde(rename = "tail_start_id", skip_serializing_if = "Option::is_none")]
    pub tail_start_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Part union + the `{ info, parts }` response wrapper.
// ---------------------------------------------------------------------------

/// One content part of a message.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(untagged)]
pub enum Part {
    /// Text.
    Text(TextPart),
    /// Subtask invocation.
    Subtask(SubtaskPart),
    /// Reasoning.
    Reasoning(ReasoningPart),
    /// File attachment.
    File(FilePart),
    /// Tool call.
    Tool(ToolPart),
    /// Step start.
    StepStart(StepStartPart),
    /// Step finish.
    StepFinish(StepFinishPart),
    /// Filesystem snapshot.
    Snapshot(SnapshotPart),
    /// Patch.
    Patch(PatchPart),
    /// Agent mention.
    Agent(AgentPart),
    /// Retry.
    Retry(RetryPart),
    /// Compaction.
    Compaction(CompactionPart),
}

/// A message together with its content parts (`{ info, parts }`) — the unit returned by
/// `session.message`/`session.messages`/`session.command`/`session.shell`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct MessageWithParts {
    /// The message envelope.
    pub info: Message,
    /// Its content parts.
    pub parts: Vec<Part>,
}
