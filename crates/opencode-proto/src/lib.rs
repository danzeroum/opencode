//! Wire types for the **external HTTP contract** — these mirror `packages/sdk/openapi.json`
//! and are the strangler-fig migration contract. Every type here derives `Serialize`,
//! `Deserialize` and `ToSchema` so the generated OpenAPI can be diffed against the golden spec.
//!
//! Keep this crate free of server/runtime dependencies: it is the single target of the
//! contract tests (`xtask openapi-diff`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

mod session_message;
pub use session_message::*;

/// Response of `GET /health` — the first contract route cut over to Rust (Phase 1).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Health {
    /// Whether the backend is serving requests.
    pub ok: bool,
    /// Which implementation served this route (`"rust"` or `"typescript"`).
    pub backend: String,
    /// Server version string.
    pub version: String,
}

/// Tagged error envelope mirroring Effect `Schema.TaggedError` serialization (`_tag` + message).
///
/// Rust libraries return their own `thiserror` enums; at the HTTP edge they are serialized
/// into this shape so the OpenAPI error schemas stay identical to the TypeScript server.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ErrorEnvelope {
    /// Discriminator tag, matching the TS `_tag` (e.g. `"SessionNotFoundError"`).
    #[serde(rename = "_tag")]
    pub tag: String,
    /// Human-readable message.
    pub message: String,
}

/// Response body of `GET /global/health` (inline in the contract). `healthy` is always `true`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct GlobalHealth {
    /// Whether the server is healthy (always `true` when it answers).
    pub healthy: bool,
    /// Server version string.
    pub version: String,
}

/// Effect HttpApi `BadRequestError` envelope: `{ name: "BadRequest", data: { message, kind? } }`.
/// (The contract's typed errors use `{ name, data }`, distinct from [`ErrorEnvelope`]'s `_tag` form.)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct BadRequestError {
    /// Always `"BadRequest"`.
    pub name: String,
    /// Error details.
    pub data: BadRequestData,
}

/// The `data` field of [`BadRequestError`].
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct BadRequestData {
    /// Human-readable message.
    pub message: String,
    /// Which part of the request was invalid (`Params`/`Headers`/`Query`/`Body`/`Payload`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// `GET /path` response (`Path` component): the resolved opencode paths for a directory.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Path {
    /// User home directory.
    pub home: String,
    /// opencode state directory.
    pub state: String,
    /// opencode config directory.
    pub config: String,
    /// Git worktree root for `directory` (falls back to `directory` when not a repo).
    pub worktree: String,
    /// The resolved working directory.
    pub directory: String,
}

/// `{ text }` wrapper used throughout the `find.text` match shape.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct TextWrap {
    /// The text value.
    pub text: String,
}

/// A submatch within a `find.text` result line.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct TextSubmatch {
    /// The matched text.
    #[serde(rename = "match")]
    pub r#match: TextWrap,
    /// Start byte offset within the line.
    pub start: u64,
    /// End byte offset within the line.
    pub end: u64,
}

/// One `find.text` match (the ripgrep JSON match shape).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct TextSearchMatch {
    /// File path (`{ text }`).
    pub path: TextWrap,
    /// The matching line (`{ text }`).
    pub lines: TextWrap,
    /// 1-based line number.
    pub line_number: u64,
    /// Absolute byte offset of the line within the file.
    pub absolute_offset: u64,
    /// Submatch ranges.
    pub submatches: Vec<TextSubmatch>,
}

/// Request body of `POST /log` (`app.log`). (An optional `extra` object in the golden body is
/// accepted but ignored by serde, so it's not modeled here.)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct LogEntry {
    /// Service name for the log entry.
    pub service: String,
    /// Log level: `debug` | `info` | `warn` | `error`.
    pub level: String,
    /// Log message.
    pub message: String,
}

/// `effect_HttpApiError_BadRequest` — Effect's generic HttpApi bad-request error (`{ _tag }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[schema(as = effect_HttpApiError_BadRequest)]
pub struct EffectHttpApiBadRequest {
    /// Always `"BadRequest"`.
    #[serde(rename = "_tag")]
    pub tag: String,
}

/// `InvalidRequestError` — schema-validation error (`{ _tag, message, kind?, field? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct InvalidRequestError {
    /// Always `"InvalidRequestError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// Human-readable message.
    pub message: String,
    /// Which part of the request was invalid.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The offending field, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
}

/// The 400 union used by mutation routes: `anyOf[effect_HttpApiError_BadRequest, InvalidRequestError]`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum RequestError {
    /// Generic bad request.
    BadRequest(EffectHttpApiBadRequest),
    /// Schema validation error.
    Invalid(InvalidRequestError),
}

/// Payload of a [`NotFoundError`] (`{ message }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct NotFoundData {
    /// Human-readable message.
    pub message: String,
}

/// `{ name: "NotFoundError", data: { message } }` — the Effect TaggedError wire format (name + nested
/// data), used as the generic 404 body by the v1 session subroutes.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct NotFoundError {
    /// Always `"NotFoundError"`.
    pub name: String,
    /// Error payload.
    pub data: NotFoundData,
}

/// A session todo item (`{ content, status, priority }`) — `packages/core/src/session/todo`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Todo {
    /// Brief description of the task.
    pub content: String,
    /// Current status: `pending` | `in_progress` | `completed` | `cancelled`.
    pub status: String,
    /// Priority level: `high` | `medium` | `low`.
    pub priority: String,
}

// ---------------------------------------------------------------------------
// V2 session read contract (`v2.session.get` — GET /api/session/{sessionID}).
// SessionV2Info mirrors `packages/core/src/session/schema.ts`; the projection mapping it comes from
// is `packages/core/src/session/info.ts` (`fromRow`). Numeric fields are `number` in the contract
// (Effect `Finite`/`DateTime`), so they are `f64` here even where the DB stores integers.
// ---------------------------------------------------------------------------

/// A model reference (`{ id, providerID, variant? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ModelRef {
    /// Model id.
    pub id: String,
    /// Provider id.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// Variant id (defaults to `"default"` in the projection).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// Cache-token usage (`{ read, write }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct TokenCache {
    /// Cache-read tokens.
    pub read: f64,
    /// Cache-write tokens.
    pub write: f64,
}

/// Token usage for a session.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SessionTokens {
    /// Input tokens.
    pub input: f64,
    /// Output tokens.
    pub output: f64,
    /// Reasoning tokens.
    pub reasoning: f64,
    /// Cache tokens.
    pub cache: TokenCache,
}

/// Session lifecycle timestamps (ms since epoch, `number` in the contract).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SessionTime {
    /// Creation time.
    pub created: f64,
    /// Last-updated time.
    pub updated: f64,
    /// Archival time, if archived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<f64>,
}

/// `LocationRef` — where a session runs (`{ directory, workspaceID? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct LocationRef {
    /// Absolute working directory.
    pub directory: String,
    /// Workspace id (`wrk_…`), if any.
    #[serde(rename = "workspaceID", skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

/// `SessionV2Info` — the V2 session projection returned by `v2.session.get`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SessionV2Info {
    /// Session id (`ses_…`).
    pub id: String,
    /// Parent session id, if this is a child session.
    #[serde(rename = "parentID", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Owning project id.
    #[serde(rename = "projectID")]
    pub project_id: String,
    /// Agent id, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Selected model, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// Accumulated cost (USD).
    pub cost: f64,
    /// Token usage.
    pub tokens: SessionTokens,
    /// Lifecycle timestamps.
    pub time: SessionTime,
    /// Session title.
    pub title: String,
    /// Where the session runs.
    pub location: LocationRef,
    /// Sub-path within the workspace, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subpath: Option<String>,
}

/// 200 body of `v2.session.get`: `{ data: SessionV2Info }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SessionGetResponse {
    /// The session projection.
    pub data: SessionV2Info,
}

/// `SessionNotFoundError` — 404 for `v2.session.get` (`{ _tag, sessionID, message }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SessionNotFoundError {
    /// Always `"SessionNotFoundError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// The session id that was not found.
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Human-readable message.
    pub message: String,
}

/// `UnauthorizedError` — 401 for V2 routes (`{ _tag, message }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct UnauthorizedError {
    /// Always `"UnauthorizedError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// Human-readable message.
    pub message: String,
}

/// `InvalidCursorError` — 400 for paginated reads when the cursor can't be decoded.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct InvalidCursorError {
    /// Always `"InvalidCursorError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// Human-readable message.
    pub message: String,
}

/// A generic server-error envelope (the golden `UnknownError1`): the 500 body for routes that can fail
/// opaquely, with an optional log-reference id for correlating with server logs.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct UnknownError {
    /// Always `"UnknownError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// Human-readable message.
    pub message: String,
    /// Optional log-reference id (`err_…`) to correlate with the server logs.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

/// Keyset pagination cursor for `v2.session.list` (`{ previous?, next? }`); always present (possibly
/// empty) in the response.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SessionCursor {
    /// Cursor to the previous page, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    /// Cursor to the next page, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// 200 body of `v2.session.list`: `{ data: SessionV2Info[], cursor }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SessionsResponse {
    /// The page of sessions.
    pub data: Vec<SessionV2Info>,
    /// Pagination cursor (always present; fields omitted when there's no adjacent page).
    pub cursor: SessionCursor,
}

/// The 400 union for `v2.session.list`: `anyOf[InvalidCursorError, InvalidRequestError]`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum SessionListError {
    /// The pagination cursor could not be decoded.
    Cursor(InvalidCursorError),
    /// The request was otherwise invalid.
    Invalid(InvalidRequestError),
}

// ---------------------------------------------------------------------------
// Project read contract (`project.list` — GET /project). `Project` mirrors
// `packages/core/src/project/sql.ts`; the `icon_*` columns fold into `icon`.
// ---------------------------------------------------------------------------

/// A project's icon (`{ url?, override?, color? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProjectIcon {
    /// Icon URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Icon URL override.
    #[serde(rename = "override", skip_serializing_if = "Option::is_none")]
    pub override_: Option<String>,
    /// Icon color.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
}

/// Project commands (`{ start? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProjectCommands {
    /// Startup script run when creating a new workspace (worktree).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
}

/// Project timestamps (ms since epoch; `integer` in the contract).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProjectTime {
    /// Creation time.
    pub created: i64,
    /// Last-updated time.
    pub updated: i64,
    /// Initialization time, if initialized.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initialized: Option<i64>,
}

/// `Project` — an entry of `project.list`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Project {
    /// Project id.
    pub id: String,
    /// Absolute worktree path.
    pub worktree: String,
    /// Version-control system (e.g. `"git"`), if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcs: Option<String>,
    /// Display name, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Icon, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<ProjectIcon>,
    /// Commands, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commands: Option<ProjectCommands>,
    /// Timestamps.
    pub time: ProjectTime,
    /// Sandbox worktree paths.
    pub sandboxes: Vec<String>,
}

// ---------------------------------------------------------------------------
// V2 session prompt contract (`v2.session.prompt` — POST /api/session/{sessionID}/prompt).
// `Prompt` and the `SessionInput.Admitted` projection mirror `packages/core/src/session/input.ts`
// and the golden `Prompt` / `SessionInputAdmitted` schemas. These are the request/response/error wire
// types for the upcoming `/prompt` cutover; that PR wires them to the route and the contract
// `openapi-diff` (which finalizes the JSON-Schema constraints — `id`/`sessionID` patterns, integer
// minimums — that don't affect the Rust struct shape or its JSON serialization).
// ---------------------------------------------------------------------------

/// A source range within the user's raw input that produced an attachment (`{ start, end, text }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct PromptSource {
    /// Start offset within the raw input.
    pub start: f64,
    /// End offset within the raw input.
    pub end: f64,
    /// The slice of raw input text.
    pub text: String,
}

/// A file/media attachment on a [`Prompt`] (`{ uri, mime, name?, description?, source? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct PromptFileAttachment {
    /// Resource URI (e.g. `file://…`).
    pub uri: String,
    /// MIME type.
    pub mime: String,
    /// Display name, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Description, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Where in the raw input this attachment was referenced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PromptSource>,
}

/// An agent mention on a [`Prompt`] (`{ name, source? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct PromptAgentAttachment {
    /// Agent name.
    pub name: String,
    /// Where in the raw input this agent was mentioned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PromptSource>,
}

/// `Prompt` — a user prompt: required `text` plus optional file/media and agent attachments.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct Prompt {
    /// The prompt text.
    pub text: String,
    /// File/media attachments (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files: Option<Vec<PromptFileAttachment>>,
    /// Agent mentions (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<PromptAgentAttachment>>,
}

/// How an admitted input is folded into the session's turn loop.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    /// Steer the running turn (the default): folded at the next step boundary.
    Steer,
    /// Queue behind the running turn: run after it completes.
    Queue,
}

/// `SessionInput.Admitted` — the durable result of admitting a prompt (the `v2.session.prompt` 200
/// payload). `admittedSeq` is the event sequence of the admission; `promotedSeq` is set once the input
/// is folded into a turn (absent while pending).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SessionInputAdmitted {
    /// Event sequence of the admission.
    #[serde(rename = "admittedSeq")]
    pub admitted_seq: i64,
    /// Message id (`msg_…`).
    pub id: String,
    /// Session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// The admitted prompt.
    pub prompt: Prompt,
    /// Delivery semantics.
    pub delivery: Delivery,
    /// Admission time (ms since epoch; `number` in the contract).
    #[serde(rename = "timeCreated")]
    pub time_created: f64,
    /// Sequence at which the input was folded into a turn (absent while pending).
    #[serde(rename = "promotedSeq", skip_serializing_if = "Option::is_none")]
    pub promoted_seq: Option<i64>,
}

/// Request body of `v2.session.prompt`: a required `prompt`, an optional caller-supplied message `id`
/// (generated when omitted), `delivery` (defaults to `steer`), and `resume` (defaults to `true`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SessionPromptRequest {
    /// Caller-supplied message id (`msg_…`); generated when omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The prompt to admit.
    pub prompt: Prompt,
    /// Delivery semantics (defaults to `steer` when omitted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery: Option<Delivery>,
    /// Whether to schedule execution (defaults to `true` when omitted).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resume: Option<bool>,
}

/// 200 body of `v2.session.prompt`: `{ data: SessionInputAdmitted }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SessionPromptResponse {
    /// The admitted input.
    pub data: SessionInputAdmitted,
}

/// `ConflictError` — 409 for `v2.session.prompt` when a different prompt was already admitted under the
/// same message `id` (`{ _tag, message, resource? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ConflictError {
    /// Always `"ConflictError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// Human-readable message.
    pub message: String,
    /// The conflicting resource, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource: Option<String>,
}

// ---------------------------------------------------------------------------
// V2 model & provider catalog contract (`v2.model.list` — GET /api/model;
// `v2.provider.list` — GET /api/provider) plus the resolved `LocationInfo`.
// `ModelV2Info`/`ProviderV2Info` mirror `packages/core/src/model.ts` & `provider.ts` (which compose
// `model-request.ts`); `LocationInfo` mirrors the server's location group. Fields sourced from Effect
// `Schema.Finite`/`Schema.Int` are `f64`/`i64`; the sampling knobs and `time.released` use Effect
// `Schema.Number`, whose JSON form is a number *or* a special-value string — modeled by
// [`EffectNumber`]. Free-form `Record<string, …>` payloads (`body`, `options`, `settings`, `data`) are
// kept verbatim as `serde_json::Value`. These are the wire types for the upcoming /api/model +
// /api/provider cutover; that PR registers the schemas, wires the routes, and finalizes the contract
// `openapi-diff` (which pins the JSON-Schema constraints — `anyOf` number unions, `^wrk` patterns —
// that don't affect the Rust struct shape or its JSON serialization).
// ---------------------------------------------------------------------------

/// The JSON form of an Effect `Schema.Number`: a finite JSON number, or — for non-finite floats — one
/// of the special strings `"NaN"` / `"Infinity"` / `"-Infinity"`. (Effect's `Schema.Finite` forbids
/// the non-finite arm and stays a plain number; this type is only for the `Schema.Number` fields.) In
/// practice opencode only ever produces finite values here; the string arms exist so the type
/// round-trips anything the TypeScript server can emit.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(untagged)]
pub enum EffectNumber {
    /// A finite number.
    Finite(f64),
    /// A non-finite value, serialized as `"NaN"` | `"Infinity"` | `"-Infinity"`.
    NonFinite(EffectNonFinite),
}

/// The non-finite arm of [`EffectNumber`] — the three special-value strings Effect emits for floats
/// that JSON can't represent.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub enum EffectNonFinite {
    /// `"NaN"`.
    #[serde(rename = "NaN")]
    Nan,
    /// `"Infinity"` (positive infinity).
    #[serde(rename = "Infinity")]
    Infinity,
    /// `"-Infinity"` (negative infinity).
    #[serde(rename = "-Infinity")]
    NegInfinity,
}

/// A model's API binding (`ModelV2.Api`) — a `{ type, … }` union tagged on `type`, carrying the
/// resolved model `id`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ModelApi {
    /// Vercel AI SDK binding (`{ type: "aisdk", id, package, url?, settings? }`).
    Aisdk {
        /// Resolved model id.
        id: String,
        /// npm package implementing the AI-SDK provider.
        package: String,
        /// Base API URL override, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// Free-form provider settings.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Object)]
        settings: Option<serde_json::Value>,
    },
    /// Native (built-in) binding (`{ type: "native", id, url?, settings }`).
    Native {
        /// Resolved model id.
        id: String,
        /// Base API URL override, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// Free-form provider settings.
        #[schema(value_type = Object)]
        settings: serde_json::Value,
    },
}

/// Model capabilities (`{ tools, input, output }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ModelCapabilities {
    /// Whether the model supports tool calls.
    pub tools: bool,
    /// Accepted input modalities (mime patterns / `image` / `audio` / `video/*` / `text/*`).
    pub input: Vec<String>,
    /// Produced output modalities.
    pub output: Vec<String>,
}

/// Sampling/generation knobs (`ModelRequest.Generation`); every field optional. The numeric knobs use
/// Effect `Schema.Number`, hence [`EffectNumber`].
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ModelGeneration {
    /// Maximum output tokens.
    #[serde(rename = "maxTokens", skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<EffectNumber>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<EffectNumber>,
    /// Nucleus-sampling `top_p`.
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<EffectNumber>,
    /// Top-k sampling.
    #[serde(rename = "topK", skip_serializing_if = "Option::is_none")]
    pub top_k: Option<EffectNumber>,
    /// Frequency penalty.
    #[serde(rename = "frequencyPenalty", skip_serializing_if = "Option::is_none")]
    pub frequency_penalty: Option<EffectNumber>,
    /// Presence penalty.
    #[serde(rename = "presencePenalty", skip_serializing_if = "Option::is_none")]
    pub presence_penalty: Option<EffectNumber>,
    /// Sampling seed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed: Option<EffectNumber>,
    /// Stop sequences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
}

/// A model's request configuration (`ModelRequest.Request` plus an optional selected `variant`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ModelRequest {
    /// Extra request headers.
    pub headers: BTreeMap<String, String>,
    /// Extra request body fields (free-form object).
    #[schema(value_type = Object)]
    pub body: serde_json::Value,
    /// Generation knobs (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<ModelGeneration>,
    /// Provider-specific options (free-form object; omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub options: Option<serde_json::Value>,
    /// Selected variant id, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

/// One entry of [`ModelV2Info::variants`] (`ModelRequest.Request` plus a required `id`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ModelVariant {
    /// Variant id.
    pub id: String,
    /// Extra request headers for this variant.
    pub headers: BTreeMap<String, String>,
    /// Extra request body fields (free-form object).
    #[schema(value_type = Object)]
    pub body: serde_json::Value,
    /// Generation knobs (omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<ModelGeneration>,
    /// Provider-specific options (free-form object; omitted when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub options: Option<serde_json::Value>,
}

/// Cache-token pricing within a [`ModelCost`] entry (`{ read, write }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ModelCostCache {
    /// Cache-read price.
    pub read: f64,
    /// Cache-write price.
    pub write: f64,
}

/// The `{ type: "context", size }` tier selector of a [`ModelCost`] entry.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ModelCostTier {
    /// Tier kind (currently always `"context"`; kept as a string so a new kind never fails parsing).
    #[serde(rename = "type")]
    pub kind: String,
    /// Context-size threshold for this tier.
    pub size: i64,
}

/// A pricing entry (`{ tier?, input, output, cache }`); models publish one per context tier.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ModelCost {
    /// The context tier this price applies to (absent for the base price).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<ModelCostTier>,
    /// Input price.
    pub input: f64,
    /// Output price.
    pub output: f64,
    /// Cache-token pricing.
    pub cache: ModelCostCache,
}

/// Model lifecycle timestamps (`{ released }`, ms since epoch via Effect `DateTimeUtcFromMillis`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ModelTime {
    /// Release time (ms since epoch).
    pub released: EffectNumber,
}

/// Token limits (`{ context, input?, output }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ModelLimit {
    /// Maximum context window (tokens).
    pub context: i64,
    /// Maximum input tokens, when distinct from `context`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<i64>,
    /// Maximum output tokens.
    pub output: i64,
}

/// Model release status.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ModelStatus {
    /// Pre-release / experimental.
    Alpha,
    /// Beta.
    Beta,
    /// Deprecated (still served, scheduled for removal).
    Deprecated,
    /// Generally available.
    Active,
}

/// `ModelV2.Info` — a resolved model entry returned by `v2.model.list` (GET /api/model).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ModelV2Info {
    /// Model id (e.g. `claude-sonnet-4-6`).
    pub id: String,
    /// Owning provider id.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// Model family (e.g. `claude-sonnet`), when grouped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Display name.
    pub name: String,
    /// How the model is reached.
    pub api: ModelApi,
    /// Capability flags.
    pub capabilities: ModelCapabilities,
    /// Base request configuration.
    pub request: ModelRequest,
    /// Named request variants.
    pub variants: Vec<ModelVariant>,
    /// Lifecycle timestamps.
    pub time: ModelTime,
    /// Pricing entries (one per context tier).
    pub cost: Vec<ModelCost>,
    /// Release status.
    pub status: ModelStatus,
    /// Whether the model is enabled for use.
    pub enabled: bool,
    /// Token limits.
    pub limit: ModelLimit,
}

/// A provider's API binding (`ProviderV2.Api`) — a `{ type, … }` union tagged on `type` (no model
/// `id`, unlike [`ModelApi`]).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ProviderApi {
    /// Vercel AI SDK binding (`{ type: "aisdk", package, url?, settings? }`).
    Aisdk {
        /// npm package implementing the AI-SDK provider.
        package: String,
        /// Base API URL override, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// Free-form provider settings.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schema(value_type = Object)]
        settings: Option<serde_json::Value>,
    },
    /// Native (built-in) binding (`{ type: "native", url?, settings }`).
    Native {
        /// Base API URL override, if any.
        #[serde(skip_serializing_if = "Option::is_none")]
        url: Option<String>,
        /// Free-form provider settings.
        #[schema(value_type = Object)]
        settings: serde_json::Value,
    },
}

/// Whether/how a provider is enabled: the literal `false` (disabled), or one of the `{ via, … }`
/// credential sources. Modeled as a **flat** untagged union (rather than nesting the three `via` arms
/// under one variant) so the generated schema matches the golden contract's 4-way `anyOf`. Each `via`
/// field is the corresponding literal string (`"env"` / `"credential"` / `"custom"`); the arms are
/// distinguished on the wire by their payload field.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(untagged)]
pub enum ProviderEnabled {
    /// Disabled (always `false` on the wire).
    Disabled(bool),
    /// Enabled by an environment variable (`{ via: "env", name }`).
    Env {
        /// Always `"env"`.
        via: String,
        /// The environment variable that supplied the key.
        name: String,
    },
    /// Enabled by a stored credential (`{ via: "credential", credentialID }`).
    Credential {
        /// Always `"credential"`.
        via: String,
        /// The credential id (`cred_…`).
        #[serde(rename = "credentialID")]
        credential_id: String,
    },
    /// Enabled by a custom config block (`{ via: "custom", data }`).
    Custom {
        /// Always `"custom"`.
        via: String,
        /// Free-form provider config.
        #[schema(value_type = Object)]
        data: serde_json::Value,
    },
}

/// A provider's base request configuration (`{ headers, body }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ProviderRequest {
    /// Extra request headers.
    pub headers: BTreeMap<String, String>,
    /// Extra request body fields (free-form object).
    #[schema(value_type = Object)]
    pub body: serde_json::Value,
}

/// `ProviderV2.Info` — a resolved provider entry returned by `v2.provider.list` (GET /api/provider).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ProviderV2Info {
    /// Provider id (e.g. `anthropic`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Whether/how the provider is enabled.
    pub enabled: ProviderEnabled,
    /// Environment variables that can supply this provider's API key.
    pub env: Vec<String>,
    /// How the provider is reached.
    pub api: ProviderApi,
    /// Base request configuration.
    pub request: ProviderRequest,
}

/// The project a [`LocationInfo`] belongs to (`{ id, directory }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct LocationProject {
    /// Project id (`prj_…`).
    pub id: String,
    /// Absolute project (worktree) directory.
    pub directory: String,
}

/// `LocationInfo` — a fully-resolved location (`{ directory, workspaceID?, project }`). Distinct from
/// [`LocationRef`], the lighter session-projection form (`{ directory, workspaceID? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct LocationInfo {
    /// Absolute working directory.
    pub directory: String,
    /// Workspace id (`wrk_…`), if any.
    #[serde(rename = "workspaceID", skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// The owning project.
    pub project: LocationProject,
}

/// `ServiceUnavailableError` — 503 for `v2.model.list` / `v2.provider.list` when the catalog can't be
/// served (`{ _tag, message, service? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ServiceUnavailableError {
    /// Always `"ServiceUnavailableError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// Human-readable message.
    pub message: String,
    /// Which service was unavailable, if specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
}

/// 200 body of `v2.model.list` (GET /api/model): the `Location.response` wrapper `{ location, data }`
/// around the model list.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ModelListResponse {
    /// The resolved request location.
    pub location: LocationInfo,
    /// The models.
    pub data: Vec<ModelV2Info>,
}

/// 200 body of `v2.provider.list` (GET /api/provider): the `Location.response` wrapper
/// `{ location, data }` around the provider list.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ProviderListResponse {
    /// The resolved request location.
    pub location: LocationInfo,
    /// The providers.
    pub data: Vec<ProviderV2Info>,
}

/// 200 body of `v2.provider.get` (GET /api/provider/{providerID}): the `Location.response` wrapper
/// `{ location, data }` around a single provider.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ProviderGetResponse {
    /// The resolved request location.
    pub location: LocationInfo,
    /// The provider.
    pub data: ProviderV2Info,
}

/// `ProviderNotFoundError` — 404 for `v2.provider.get` (`{ _tag, providerID, message }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProviderNotFoundError {
    /// Always `"ProviderNotFoundError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// The provider id that was not found.
    #[serde(rename = "providerID")]
    pub provider_id: String,
    /// Human-readable message.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_envelope_uses_tag_key() {
        let json = serde_json::to_value(ErrorEnvelope {
            tag: "BadRequest".into(),
            message: "boom".into(),
        })
        .unwrap();
        assert_eq!(json["_tag"], "BadRequest");
        assert_eq!(json["message"], "boom");
    }

    #[test]
    fn prompt_omits_empty_attachment_arrays() {
        let json = serde_json::to_value(Prompt {
            text: "hi".into(),
            files: None,
            agents: None,
        })
        .unwrap();
        assert_eq!(json, serde_json::json!({ "text": "hi" }));
    }

    #[test]
    fn prompt_round_trips_with_attachments() {
        let prompt = Prompt {
            text: "see @agent and file".into(),
            files: Some(vec![PromptFileAttachment {
                uri: "file:///x".into(),
                mime: "text/plain".into(),
                name: Some("x".into()),
                description: None,
                source: Some(PromptSource {
                    start: 9.0,
                    end: 13.0,
                    text: "file".into(),
                }),
            }]),
            agents: Some(vec![PromptAgentAttachment {
                name: "agent".into(),
                source: None,
            }]),
        };
        let json = serde_json::to_value(&prompt).unwrap();
        assert_eq!(json["files"][0]["uri"], "file:///x");
        assert_eq!(json["files"][0]["source"]["start"], 9.0);
        assert!(json["files"][0].get("description").is_none());
        assert_eq!(json["agents"][0]["name"], "agent");
        let back: Prompt = serde_json::from_value(json).unwrap();
        assert_eq!(back, prompt);
    }

    #[test]
    fn delivery_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(Delivery::Steer).unwrap(),
            serde_json::json!("steer")
        );
        assert_eq!(
            serde_json::to_value(Delivery::Queue).unwrap(),
            serde_json::json!("queue")
        );
        let back: Delivery = serde_json::from_value(serde_json::json!("queue")).unwrap();
        assert_eq!(back, Delivery::Queue);
    }

    #[test]
    fn session_input_admitted_uses_camel_case_and_omits_promoted_when_pending() {
        let admitted = SessionInputAdmitted {
            admitted_seq: 3,
            id: "msg_1".into(),
            session_id: "ses_1".into(),
            prompt: Prompt {
                text: "hi".into(),
                files: None,
                agents: None,
            },
            delivery: Delivery::Steer,
            time_created: 1234.0,
            promoted_seq: None,
        };
        let json = serde_json::to_value(&admitted).unwrap();
        assert_eq!(json["admittedSeq"], 3);
        assert_eq!(json["sessionID"], "ses_1");
        assert_eq!(json["timeCreated"], 1234.0);
        assert_eq!(json["delivery"], "steer");
        assert!(json.get("promotedSeq").is_none());
        let back: SessionInputAdmitted = serde_json::from_value(json).unwrap();
        assert_eq!(back, admitted);
    }

    #[test]
    fn session_prompt_request_requires_only_prompt() {
        // A minimal body (just `prompt`) deserializes with the optional fields defaulting to None.
        let req: SessionPromptRequest =
            serde_json::from_value(serde_json::json!({ "prompt": { "text": "hi" } })).unwrap();
        assert_eq!(req.prompt.text, "hi");
        assert!(req.id.is_none());
        assert!(req.delivery.is_none());
        assert!(req.resume.is_none());
        // …and re-serializes back to just `{ prompt: { text } }`.
        let json = serde_json::to_value(&req).unwrap();
        assert_eq!(json, serde_json::json!({ "prompt": { "text": "hi" } }));
    }

    #[test]
    fn conflict_error_uses_tag_key() {
        let json = serde_json::to_value(ConflictError {
            tag: "ConflictError".into(),
            message: "already admitted".into(),
            resource: Some("msg_1".into()),
        })
        .unwrap();
        assert_eq!(json["_tag"], "ConflictError");
        assert_eq!(json["resource"], "msg_1");
    }

    fn sample_model() -> ModelV2Info {
        ModelV2Info {
            id: "claude-sonnet-4-6".into(),
            provider_id: "anthropic".into(),
            family: Some("claude-sonnet".into()),
            name: "Claude Sonnet 4.6".into(),
            api: ModelApi::Native {
                id: "claude-sonnet-4-6".into(),
                url: None,
                settings: serde_json::json!({}),
            },
            capabilities: ModelCapabilities {
                tools: true,
                input: vec!["text".into(), "image".into()],
                output: vec!["text".into()],
            },
            request: ModelRequest {
                headers: BTreeMap::new(),
                body: serde_json::json!({}),
                generation: Some(ModelGeneration::default()),
                options: Some(serde_json::json!({})),
                variant: None,
            },
            variants: vec![],
            time: ModelTime {
                released: EffectNumber::Finite(1_700_000_000_000.0),
            },
            cost: vec![ModelCost {
                tier: None,
                input: 3.0,
                output: 15.0,
                cache: ModelCostCache {
                    read: 0.3,
                    write: 3.75,
                },
            }],
            status: ModelStatus::Active,
            enabled: true,
            limit: ModelLimit {
                context: 200_000,
                input: None,
                output: 64_000,
            },
        }
    }

    #[test]
    fn model_v2_info_round_trips_and_uses_camel_case() {
        let model = sample_model();
        let json = serde_json::to_value(&model).unwrap();
        assert_eq!(json["providerID"], "anthropic");
        assert_eq!(json["family"], "claude-sonnet");
        assert_eq!(json["api"]["type"], "native");
        assert_eq!(json["api"]["id"], "claude-sonnet-4-6");
        assert!(json["api"].get("url").is_none());
        assert_eq!(json["capabilities"]["tools"], true);
        assert_eq!(json["cost"][0]["cache"]["read"], 0.3);
        assert!(json["cost"][0].get("tier").is_none());
        assert_eq!(json["status"], "active");
        assert_eq!(json["limit"]["context"], 200_000);
        assert!(json["limit"].get("input").is_none());
        assert_eq!(json["time"]["released"], 1_700_000_000_000.0_f64);
        let back: ModelV2Info = serde_json::from_value(json).unwrap();
        assert_eq!(back, model);
    }

    #[test]
    fn model_api_aisdk_carries_id_and_package() {
        let api = ModelApi::Aisdk {
            id: "gpt-5".into(),
            package: "@ai-sdk/openai".into(),
            url: Some("https://api.openai.com".into()),
            settings: Some(serde_json::json!({ "x": 1 })),
        };
        let json = serde_json::to_value(&api).unwrap();
        assert_eq!(json["type"], "aisdk");
        assert_eq!(json["id"], "gpt-5");
        assert_eq!(json["package"], "@ai-sdk/openai");
        assert_eq!(json["url"], "https://api.openai.com");
        assert_eq!(json["settings"]["x"], 1);
        let back: ModelApi = serde_json::from_value(json).unwrap();
        assert_eq!(back, api);
    }

    #[test]
    fn effect_number_finite_and_special_round_trip() {
        assert_eq!(
            serde_json::to_value(EffectNumber::Finite(0.5)).unwrap(),
            serde_json::json!(0.5)
        );
        assert_eq!(
            serde_json::to_value(EffectNumber::NonFinite(EffectNonFinite::Nan)).unwrap(),
            serde_json::json!("NaN")
        );
        assert_eq!(
            serde_json::to_value(EffectNumber::NonFinite(EffectNonFinite::Infinity)).unwrap(),
            serde_json::json!("Infinity")
        );
        assert_eq!(
            serde_json::to_value(EffectNumber::NonFinite(EffectNonFinite::NegInfinity)).unwrap(),
            serde_json::json!("-Infinity")
        );
        // An integer JSON number deserializes into the finite arm.
        let n: EffectNumber = serde_json::from_value(serde_json::json!(64000)).unwrap();
        assert_eq!(n, EffectNumber::Finite(64000.0));
        // The special strings deserialize into the non-finite arm.
        let n: EffectNumber = serde_json::from_value(serde_json::json!("-Infinity")).unwrap();
        assert_eq!(n, EffectNumber::NonFinite(EffectNonFinite::NegInfinity));
    }

    #[test]
    fn model_generation_renames_and_omits_absent_knobs() {
        let only_max = ModelGeneration {
            max_tokens: Some(EffectNumber::Finite(1024.0)),
            ..Default::default()
        };
        assert_eq!(
            serde_json::to_value(&only_max).unwrap(),
            serde_json::json!({ "maxTokens": 1024.0 })
        );
        let full = ModelGeneration {
            max_tokens: Some(EffectNumber::Finite(1.0)),
            temperature: Some(EffectNumber::Finite(0.7)),
            top_p: Some(EffectNumber::Finite(0.9)),
            top_k: Some(EffectNumber::Finite(40.0)),
            frequency_penalty: Some(EffectNumber::Finite(0.0)),
            presence_penalty: Some(EffectNumber::Finite(0.0)),
            seed: Some(EffectNumber::Finite(42.0)),
            stop: Some(vec!["\n".into()]),
        };
        let json = serde_json::to_value(&full).unwrap();
        assert!(json.get("topP").is_some());
        assert!(json.get("topK").is_some());
        assert!(json.get("frequencyPenalty").is_some());
        assert!(json.get("presencePenalty").is_some());
        let back: ModelGeneration = serde_json::from_value(json).unwrap();
        assert_eq!(back, full);
    }

    #[test]
    fn model_request_omits_optional_blocks() {
        let req = ModelRequest {
            headers: BTreeMap::new(),
            body: serde_json::json!({}),
            generation: None,
            options: None,
            variant: None,
        };
        assert_eq!(
            serde_json::to_value(&req).unwrap(),
            serde_json::json!({ "headers": {}, "body": {} })
        );
    }

    #[test]
    fn model_cost_tier_round_trips() {
        let cost = ModelCost {
            tier: Some(ModelCostTier {
                kind: "context".into(),
                size: 200_000,
            }),
            input: 6.0,
            output: 22.5,
            cache: ModelCostCache {
                read: 0.6,
                write: 7.5,
            },
        };
        let json = serde_json::to_value(&cost).unwrap();
        assert_eq!(json["tier"]["type"], "context");
        assert_eq!(json["tier"]["size"], 200_000);
        let back: ModelCost = serde_json::from_value(json).unwrap();
        assert_eq!(back, cost);
    }

    #[test]
    fn model_status_serializes_lowercase() {
        assert_eq!(
            serde_json::to_value(ModelStatus::Active).unwrap(),
            serde_json::json!("active")
        );
        assert_eq!(
            serde_json::to_value(ModelStatus::Deprecated).unwrap(),
            serde_json::json!("deprecated")
        );
        let back: ModelStatus = serde_json::from_value(serde_json::json!("alpha")).unwrap();
        assert_eq!(back, ModelStatus::Alpha);
    }

    #[test]
    fn provider_enabled_disabled_is_literal_false() {
        assert_eq!(
            serde_json::to_value(ProviderEnabled::Disabled(false)).unwrap(),
            serde_json::json!(false)
        );
        let back: ProviderEnabled = serde_json::from_value(serde_json::json!(false)).unwrap();
        assert_eq!(back, ProviderEnabled::Disabled(false));
    }

    #[test]
    fn provider_enabled_sources_are_tagged_on_via() {
        let env = ProviderEnabled::Env {
            via: "env".into(),
            name: "ANTHROPIC_API_KEY".into(),
        };
        assert_eq!(
            serde_json::to_value(&env).unwrap(),
            serde_json::json!({ "via": "env", "name": "ANTHROPIC_API_KEY" })
        );
        let cred = ProviderEnabled::Credential {
            via: "credential".into(),
            credential_id: "cred_1".into(),
        };
        assert_eq!(
            serde_json::to_value(&cred).unwrap(),
            serde_json::json!({ "via": "credential", "credentialID": "cred_1" })
        );
        let custom = ProviderEnabled::Custom {
            via: "custom".into(),
            data: serde_json::json!({ "k": "v" }),
        };
        assert_eq!(
            serde_json::to_value(&custom).unwrap(),
            serde_json::json!({ "via": "custom", "data": { "k": "v" } })
        );
        // The flat untagged enum routes each `{ via }` object to its arm by payload shape.
        let back: ProviderEnabled =
            serde_json::from_value(serde_json::json!({ "via": "env", "name": "X" })).unwrap();
        assert_eq!(
            back,
            ProviderEnabled::Env {
                via: "env".into(),
                name: "X".into()
            }
        );
        let back: ProviderEnabled =
            serde_json::from_value(serde_json::json!({ "via": "custom", "data": { "k": 1 } }))
                .unwrap();
        assert_eq!(
            back,
            ProviderEnabled::Custom {
                via: "custom".into(),
                data: serde_json::json!({ "k": 1 })
            }
        );
    }

    #[test]
    fn provider_v2_info_round_trips() {
        let provider = ProviderV2Info {
            id: "anthropic".into(),
            name: "Anthropic".into(),
            enabled: ProviderEnabled::Env {
                via: "env".into(),
                name: "ANTHROPIC_API_KEY".into(),
            },
            env: vec!["ANTHROPIC_API_KEY".into()],
            api: ProviderApi::Aisdk {
                package: "@ai-sdk/anthropic".into(),
                url: None,
                settings: None,
            },
            request: ProviderRequest {
                headers: BTreeMap::new(),
                body: serde_json::json!({}),
            },
        };
        let json = serde_json::to_value(&provider).unwrap();
        assert_eq!(json["api"]["type"], "aisdk");
        assert_eq!(json["api"]["package"], "@ai-sdk/anthropic");
        assert!(json["api"].get("url").is_none());
        assert!(json["api"].get("settings").is_none());
        assert_eq!(json["enabled"]["via"], "env");
        let back: ProviderV2Info = serde_json::from_value(json).unwrap();
        assert_eq!(back, provider);
    }

    #[test]
    fn location_info_omits_absent_workspace_and_renames() {
        let loc = LocationInfo {
            directory: "/home/u/proj".into(),
            workspace_id: None,
            project: LocationProject {
                id: "prj_1".into(),
                directory: "/home/u/proj".into(),
            },
        };
        let json = serde_json::to_value(&loc).unwrap();
        assert!(json.get("workspaceID").is_none());
        assert_eq!(json["project"]["id"], "prj_1");
        let with_ws = LocationInfo {
            workspace_id: Some("wrk_1".into()),
            ..loc
        };
        let json = serde_json::to_value(&with_ws).unwrap();
        assert_eq!(json["workspaceID"], "wrk_1");
        let back: LocationInfo = serde_json::from_value(json).unwrap();
        assert_eq!(back, with_ws);
    }

    #[test]
    fn service_unavailable_error_uses_tag_and_omits_absent_service() {
        let json = serde_json::to_value(ServiceUnavailableError {
            tag: "ServiceUnavailableError".into(),
            message: "Model catalog is unavailable".into(),
            service: None,
        })
        .unwrap();
        assert_eq!(json["_tag"], "ServiceUnavailableError");
        assert_eq!(json["message"], "Model catalog is unavailable");
        assert!(json.get("service").is_none());
        let with_service = ServiceUnavailableError {
            tag: "ServiceUnavailableError".into(),
            message: "boom".into(),
            service: Some("catalog".into()),
        };
        let json = serde_json::to_value(&with_service).unwrap();
        assert_eq!(json["service"], "catalog");
        let back: ServiceUnavailableError = serde_json::from_value(json).unwrap();
        assert_eq!(back, with_service);
    }

    fn sample_location() -> LocationInfo {
        LocationInfo {
            directory: "/home/u/proj".into(),
            workspace_id: None,
            project: LocationProject {
                id: "prj_1".into(),
                directory: "/home/u/proj".into(),
            },
        }
    }

    #[test]
    fn model_list_response_round_trips() {
        let resp = ModelListResponse {
            location: sample_location(),
            data: vec![sample_model()],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["location"]["project"]["id"], "prj_1");
        assert_eq!(json["data"][0]["providerID"], "anthropic");
        let back: ModelListResponse = serde_json::from_value(json).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn provider_list_response_round_trips() {
        let resp = ProviderListResponse {
            location: sample_location(),
            data: vec![ProviderV2Info {
                id: "anthropic".into(),
                name: "Anthropic".into(),
                enabled: ProviderEnabled::Disabled(false),
                env: vec!["ANTHROPIC_API_KEY".into()],
                api: ProviderApi::Native {
                    url: None,
                    settings: serde_json::json!({}),
                },
                request: ProviderRequest {
                    headers: BTreeMap::new(),
                    body: serde_json::json!({}),
                },
            }],
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["data"][0]["id"], "anthropic");
        assert_eq!(json["location"]["directory"], "/home/u/proj");
        let back: ProviderListResponse = serde_json::from_value(json).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn provider_get_response_round_trips() {
        let resp = ProviderGetResponse {
            location: sample_location(),
            data: ProviderV2Info {
                id: "anthropic".into(),
                name: "Anthropic".into(),
                enabled: ProviderEnabled::Env {
                    via: "env".into(),
                    name: "ANTHROPIC_API_KEY".into(),
                },
                env: vec!["ANTHROPIC_API_KEY".into()],
                api: ProviderApi::Native {
                    url: None,
                    settings: serde_json::json!({}),
                },
                request: ProviderRequest {
                    headers: BTreeMap::new(),
                    body: serde_json::json!({}),
                },
            },
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["data"]["id"], "anthropic"); // single object, not an array
        assert_eq!(json["data"]["enabled"]["via"], "env");
        let back: ProviderGetResponse = serde_json::from_value(json).unwrap();
        assert_eq!(back, resp);
    }

    #[test]
    fn provider_not_found_error_uses_tag_and_provider_id_keys() {
        let json = serde_json::to_value(ProviderNotFoundError {
            tag: "ProviderNotFoundError".into(),
            provider_id: "anthropic".into(),
            message: "Provider not found: anthropic".into(),
        })
        .unwrap();
        assert_eq!(json["_tag"], "ProviderNotFoundError");
        assert_eq!(json["providerID"], "anthropic");
        assert_eq!(json["message"], "Provider not found: anthropic");
    }
}
