//! Wire types for the **external HTTP contract** — these mirror `packages/sdk/openapi.json`
//! and are the strangler-fig migration contract. Every type here derives `Serialize`,
//! `Deserialize` and `ToSchema` so the generated OpenAPI can be diffed against the golden spec.
//!
//! Keep this crate free of server/runtime dependencies: it is the single target of the
//! contract tests (`xtask openapi-diff`).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

mod config;
mod integration;
mod lsp;
mod mcp;
mod message;
mod session_message;
pub use config::*;
pub use integration::*;
pub use lsp::*;
pub use mcp::*;
pub use message::*;
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

/// 200 body of `v2.health.get` (GET /api/health): `{ healthy: true }`. (The golden constrains
/// `healthy` to the literal `true`; the contract normalizer drops the enum, so a `bool` matches.)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct HealthV2 {
    /// Always `true` while the server is responding.
    pub healthy: bool,
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

/// A filesystem entry returned by `file.list` (`{ name, path, absolute, type, ignored }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct FileNode {
    /// Entry file name.
    pub name: String,
    /// Path relative to the requested directory.
    pub path: String,
    /// Absolute filesystem path.
    pub absolute: String,
    /// `"file"` or `"directory"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Whether the entry is gitignored.
    pub ignored: bool,
}

/// A changed file in the working tree (`file.status` item). Mirrors the golden `File`: `{ path, added,
/// removed, status }`, all required (`status` is `"added"` | `"deleted"` | `"modified"`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct File {
    /// Path relative to the repository root.
    pub path: String,
    /// Lines added.
    pub added: i64,
    /// Lines removed.
    pub removed: i64,
    /// Change kind.
    pub status: String,
}

/// A zero-based position in a text document (`{ line, character }`) — LSP semantics.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Position {
    /// Zero-based line number.
    pub line: i64,
    /// Zero-based character offset.
    pub character: i64,
}

/// A range in a text document (`{ start, end }`) — LSP semantics.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Range {
    /// Start position (inclusive).
    pub start: Position,
    /// End position (exclusive).
    pub end: Position,
}

/// Where a [`Symbol`] is defined (`{ uri, range }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SymbolLocation {
    /// Document URI.
    pub uri: String,
    /// The symbol's range within the document.
    pub range: Range,
}

/// A workspace symbol (`find.symbols` item). Mirrors the golden `Symbol`: `{ name, kind, location }`
/// (`kind` is the LSP `SymbolKind` integer).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Symbol {
    /// Symbol name.
    pub name: String,
    /// LSP `SymbolKind` (integer).
    pub kind: i64,
    /// Where the symbol is defined.
    pub location: SymbolLocation,
}

/// One hunk of a unified diff (`FileContent.patch.hunks` item). Mirrors the golden hunk shape:
/// `{ oldStart, oldLines, newStart, newLines, lines }`, all required.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct FilePatchHunk {
    /// Start line in the old file.
    #[serde(rename = "oldStart")]
    pub old_start: i64,
    /// Line count in the old file.
    #[serde(rename = "oldLines")]
    pub old_lines: i64,
    /// Start line in the new file.
    #[serde(rename = "newStart")]
    pub new_start: i64,
    /// Line count in the new file.
    #[serde(rename = "newLines")]
    pub new_lines: i64,
    /// The hunk's diff lines.
    pub lines: Vec<String>,
}

/// A structured unified diff (`FileContent.patch`). Mirrors the golden patch shape:
/// `{ oldFileName, newFileName, oldHeader?, newHeader?, hunks, index? }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct FilePatch {
    /// Old file name.
    #[serde(rename = "oldFileName")]
    pub old_file_name: String,
    /// New file name.
    #[serde(rename = "newFileName")]
    pub new_file_name: String,
    /// Optional old-file header.
    #[serde(rename = "oldHeader", skip_serializing_if = "Option::is_none")]
    pub old_header: Option<String>,
    /// Optional new-file header.
    #[serde(rename = "newHeader", skip_serializing_if = "Option::is_none")]
    pub new_header: Option<String>,
    /// The diff hunks.
    pub hunks: Vec<FilePatchHunk>,
    /// Optional git index line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
}

/// 200 body of `file.read` (GET /file/content). Mirrors the golden `FileContent`: `{ type, content,
/// diff?, patch?, encoding?, mimeType? }` (`type` is `"text"` | `"binary"`; `encoding` is `"base64"`
/// for binary content).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct FileContent {
    /// `"text"` or `"binary"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// The file content (UTF-8 text, or base64 when `encoding` is set).
    pub content: String,
    /// Optional unified-diff text (when the file differs from its baseline).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Optional structured diff.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<FilePatch>,
    /// Content encoding (`"base64"` for binary).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// Optional MIME type.
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

/// A filesystem entry returned by the V2 fs routes (`v2.fs.list` / `v2.fs.find`). Mirrors the golden
/// `FileSystemEntry`: `{ path, type, mime }`, all required (`type` is `"file"` | `"directory"`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct FileSystemEntry {
    /// Path relative to the requested location/path.
    pub path: String,
    /// `"file"` or `"directory"`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Best-effort MIME type (`inode/directory` for directories).
    pub mime: String,
}

/// 200 body of `v2.fs.list` / `v2.fs.find`: the `Location.response` wrapper `{ location, data }` around
/// the filesystem entries.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct FsListResponse {
    /// The resolved request location.
    pub location: LocationInfo,
    /// The filesystem entries.
    pub data: Vec<FileSystemEntry>,
}

/// Version-control info for a directory (`vcs.get`): `{ branch?, default_branch? }`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct VcsInfo {
    /// Current branch (omitted when detached or not a repo).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The repository's default branch, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_branch: Option<String>,
}

/// A changed file's summary (`vcs.status`): `{ file, additions, deletions, status }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct VcsFileStatus {
    /// File path (repo-relative).
    pub file: String,
    /// Lines added.
    pub additions: f64,
    /// Lines deleted.
    pub deletions: f64,
    /// `added` | `deleted` | `modified`.
    pub status: String,
}

/// A changed file's diff (`vcs.diff`): `{ file, patch?, additions, deletions, status? }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct VcsFileDiff {
    /// File path (repo-relative).
    pub file: String,
    /// Unified diff for the file, if computed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    /// Lines added.
    pub additions: f64,
    /// Lines deleted.
    pub deletions: f64,
    /// `added` | `deleted` | `modified`, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// 200 body of `vcs.apply` (`{ applied }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct VcsApplyResult {
    /// Whether the patch applied cleanly.
    pub applied: bool,
}

/// `{ message, reason }` payload of [`VcsApplyError`] — `reason` is `non-git` | `not-clean`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct VcsApplyErrorData {
    /// Human-readable message.
    pub message: String,
    /// Why the apply failed (`non-git` = not a repo; `not-clean` = the patch didn't apply).
    pub reason: String,
}

/// The `vcs.apply` typed failure (`{ name: "VcsApplyError", data }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct VcsApplyError {
    /// Always `"VcsApplyError"`.
    pub name: String,
    /// Error payload.
    pub data: VcsApplyErrorData,
}

/// The `vcs.apply` 400 union: `anyOf[VcsApplyError, InvalidRequestError]`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum VcsApplyRequestError {
    /// The patch could not be applied.
    Apply(VcsApplyError),
    /// The request was otherwise invalid.
    Invalid(InvalidRequestError),
}

/// `{ message }` — the data payload shared by the simple `{ name, data }` experimental errors.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct NamedMessageData {
    /// Human-readable message.
    pub message: String,
}

/// A `{ name, data: { message } }` experimental error — structurally shared by `WorktreeError`,
/// `MoveSessionError`, and `WorkspaceWarpError` (the diff drops the per-error `name` enum).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct NamedMessageError {
    /// The error name.
    pub name: String,
    /// Error payload.
    pub data: NamedMessageData,
}

/// `{ message, forceRequired? }` — `ProjectCopyError`'s data.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProjectCopyErrorData {
    /// Human-readable message.
    pub message: String,
    /// Whether a forced copy is required to proceed.
    #[serde(rename = "forceRequired", skip_serializing_if = "Option::is_none")]
    pub force_required: Option<bool>,
}

/// `{ name: "ProjectCopyError", data }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProjectCopyError {
    /// Always `"ProjectCopyError"`.
    pub name: String,
    /// Error payload.
    pub data: ProjectCopyErrorData,
}

/// The 400 union `anyOf[<NamedMessageError>, InvalidRequestError]` — used by `worktree.*` and
/// `experimental.controlPlane.moveSession`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum NamedMessageRequestError {
    /// The named operation error.
    Named(NamedMessageError),
    /// The request was otherwise invalid.
    Invalid(InvalidRequestError),
}

/// The 400 union `anyOf[ProjectCopyError, InvalidRequestError]` — used by `v2.projectCopy.*`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum ProjectCopyRequestError {
    /// The project-copy error.
    Copy(ProjectCopyError),
    /// The request was otherwise invalid.
    Invalid(InvalidRequestError),
}

/// The 400 union `anyOf[WorkspaceWarpError, VcsApplyError, InvalidRequestError]` — `workspace.warp`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum WorkspaceWarpRequestError {
    /// The warp error.
    Warp(NamedMessageError),
    /// A VCS apply failure during warp.
    Vcs(VcsApplyError),
    /// The request was otherwise invalid.
    Invalid(InvalidRequestError),
}

/// The tool call a [`PermissionRequest`] is gating (`{ messageID, callID }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PermissionRequestTool {
    /// Message id of the call.
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Tool call id.
    #[serde(rename = "callID")]
    pub call_id: String,
}

/// A pending permission request (`permission.list`) — a tool call awaiting allow/deny.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PermissionRequest {
    /// Request id (`per_…`).
    pub id: String,
    /// Owning session (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// The permission being requested (e.g. `bash`).
    pub permission: String,
    /// Resource patterns the decision applies to.
    pub patterns: Vec<String>,
    /// Free-form metadata.
    #[schema(value_type = Object)]
    pub metadata: serde_json::Value,
    /// Patterns the user can choose to always-allow.
    pub always: Vec<String>,
    /// The gated tool call, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<PermissionRequestTool>,
}

/// `PermissionNotFoundError` — `{ _tag, requestID, message }`, one of the 404 arms of
/// `permission.respond` when the permission request id is unknown.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PermissionNotFoundError {
    /// Always `"PermissionNotFoundError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// The permission request id that was not found.
    #[serde(rename = "requestID")]
    pub request_id: String,
    /// Human-readable message.
    pub message: String,
}

/// The 404 union of `permission.respond`: `NotFoundError` (unknown session) | `PermissionNotFoundError`
/// (unknown permission request). Untagged so it serializes as whichever arm the handler returns.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum PermissionRespondNotFound {
    /// The session wasn't found.
    Session(NotFoundError),
    /// The permission request wasn't found.
    Permission(PermissionNotFoundError),
}

/// The 404 union of `v2.session.permission.reply`: `PermissionNotFoundError` | `SessionNotFoundError`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum PermissionReplyNotFound {
    /// The permission request wasn't found.
    Permission(PermissionNotFoundError),
    /// The session wasn't found.
    Session(SessionNotFoundError),
}

/// `QuestionNotFoundError` — `{ _tag, requestID, message }`, a 404 arm of the question reply/reject
/// routes when the question request id is unknown.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct QuestionNotFoundError {
    /// Always `"QuestionNotFoundError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// The question request id that was not found.
    #[serde(rename = "requestID")]
    pub request_id: String,
    /// Human-readable message.
    pub message: String,
}

/// The 404 union of `v2.session.question.reply`/`reject`: `QuestionNotFoundError` | `SessionNotFoundError`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum QuestionReplyNotFound {
    /// The question request wasn't found.
    Question(QuestionNotFoundError),
    /// The session wasn't found.
    Session(SessionNotFoundError),
}

/// One choice for a [`QuestionInfo`] (`{ label, description }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct QuestionOption {
    /// Display text (1–5 words).
    pub label: String,
    /// Explanation of the choice.
    pub description: String,
}

/// A single question in a [`QuestionRequest`].
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct QuestionInfo {
    /// The complete question.
    pub question: String,
    /// A very short label (≤ 30 chars).
    pub header: String,
    /// Available choices.
    pub options: Vec<QuestionOption>,
    /// Whether multiple options may be selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    /// Whether a custom (free-text) answer is allowed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<bool>,
}

/// The tool call a [`QuestionRequest`] originates from (`{ messageID, callID }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct QuestionTool {
    /// Message id (`msg_…`).
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// Tool call id.
    #[serde(rename = "callID")]
    pub call_id: String,
}

/// A pending question request (`question.list`) — a tool asking the user to choose.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct QuestionRequest {
    /// Request id (`que_…`).
    pub id: String,
    /// Owning session (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// The questions to ask.
    pub questions: Vec<QuestionInfo>,
    /// The originating tool call, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<QuestionTool>,
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

/// 200 body of `v2.session.context` (GET /api/session/{sessionID}/context): `{ data }` — the session's
/// prepared context as a list of [`SessionMessage`] timeline entries.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SessionContextResponse {
    /// The context timeline entries.
    pub data: Vec<SessionMessage>,
}

/// A user-facing action shown while a session retries (`SessionStatus::Retry.action`):
/// `{ reason, provider, title, message, label, link? }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SessionRetryAction {
    /// Why the retry is happening.
    pub reason: String,
    /// The provider involved.
    pub provider: String,
    /// A short title.
    pub title: String,
    /// A longer message.
    pub message: String,
    /// A call-to-action label.
    pub label: String,
    /// An optional link.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

/// Live status of a session (`session.status` map value). Mirrors the golden `SessionStatus` union,
/// internally tagged on `type`: `idle` | `retry` (with attempt/backoff details) | `busy`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SessionStatus {
    /// No turn in progress.
    Idle,
    /// A turn is retrying after a transient failure.
    Retry {
        /// The current attempt number.
        attempt: i64,
        /// A human-readable status message.
        message: String,
        /// An optional user-facing action.
        #[serde(skip_serializing_if = "Option::is_none")]
        action: Option<SessionRetryAction>,
        /// Milliseconds until the next attempt.
        next: i64,
    },
    /// A turn is actively running.
    Busy,
}

/// `UnknownError1` (golden) — the `_tag`-discriminated unknown-error envelope some V2 routes declare for
/// their 500 response: `{ _tag: "UnknownError", message, ref? }`. Distinct from [`UnknownError`]
/// (`{ name, data }`) and [`ErrorEnvelope`] (no `ref`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct TaggedUnknownError {
    /// Always `"UnknownError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// Human-readable message.
    pub message: String,
    /// Optional opaque error reference.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
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
// V1 session contract (`Session` — returned by session.update/share/revert/…).
// Mirrors `packages/core/src/v1/session.ts`; reuses `ModelRef` + `SessionTokens`.
// ---------------------------------------------------------------------------

/// A permission decision (`allow` | `deny` | `ask`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionAction {
    /// Allow without asking.
    Allow,
    /// Deny.
    Deny,
    /// Ask the user.
    Ask,
}

/// One permission rule (`{ permission, pattern, action }`). `PermissionRuleset` is `[PermissionRule]`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PermissionRule {
    /// The permission this rule governs (e.g. `bash`).
    pub permission: String,
    /// Resource glob the rule matches.
    pub pattern: String,
    /// The decision.
    pub action: PermissionAction,
}

/// A per-file diff in a session/snapshot summary (`{ file?, patch?, additions, deletions, status? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SnapshotFileDiff {
    /// File path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Unified diff patch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    /// Lines added.
    pub additions: f64,
    /// Lines deleted.
    pub deletions: f64,
    /// `added` | `deleted` | `modified`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
}

/// A session's change summary (`{ additions, deletions, files, diffs? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SessionSummary {
    /// Total lines added.
    pub additions: f64,
    /// Total lines deleted.
    pub deletions: f64,
    /// Files changed.
    pub files: f64,
    /// Per-file diffs, if computed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diffs: Option<Vec<SnapshotFileDiff>>,
}

/// A session's share link (`{ url }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SessionShare {
    /// Public share URL.
    pub url: String,
}

/// V1 session timestamps (`{ created, updated, compacting?, archived? }`). `created`/`updated`/
/// `compacting` are `integer` ms in the contract; `archived` is `number`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SessionV1Time {
    /// Creation time (ms).
    pub created: i64,
    /// Last-updated time (ms).
    pub updated: i64,
    /// When a compaction is in progress, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compacting: Option<i64>,
    /// Archival time, if archived.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archived: Option<f64>,
}

/// `{ _tag: "SessionBusyError", sessionID, message }` — the 409 for mutations that can't run while a
/// session is busy (`revert`/`unrevert`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SessionBusyError {
    /// Always `"SessionBusyError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// The busy session.
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// Human-readable message.
    pub message: String,
}

/// A session's revert pointer (`{ messageID, partID?, snapshot?, diff? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SessionRevert {
    /// The message reverted to.
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// The part within the message, if finer-grained.
    #[serde(rename = "partID", skip_serializing_if = "Option::is_none")]
    pub part_id: Option<String>,
    /// Snapshot id captured at the revert point.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    /// Diff from the revert point.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

/// `Session` (V1) — the full session object returned by `session.update`/`share`/`unshare`/`revert`/
/// `unrevert`. Mirrors `packages/core/src/v1/session.ts`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct Session {
    /// Session id (`ses_…`).
    pub id: String,
    /// URL-safe slug.
    pub slug: String,
    /// Owning project id.
    #[serde(rename = "projectID")]
    pub project_id: String,
    /// Owning workspace id, if any.
    #[serde(rename = "workspaceID", skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Working directory.
    pub directory: String,
    /// Session path, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Parent session id, if a child.
    #[serde(rename = "parentID", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Change summary, if computed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<SessionSummary>,
    /// Accumulated cost (USD).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// Accumulated token usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<SessionTokens>,
    /// Share link, if shared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<SessionShare>,
    /// Title.
    pub title: String,
    /// Active agent, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Active model, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// Schema/app version that wrote the session.
    pub version: String,
    /// Free-form metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub metadata: Option<serde_json::Value>,
    /// Timestamps.
    pub time: SessionV1Time,
    /// Permission ruleset (`[PermissionRule]`), if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<Vec<PermissionRule>>,
    /// Revert pointer, if the session is reverted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert: Option<SessionRevert>,
}

/// An agent's model pin (`{ modelID, providerID }`) — note `modelID`, distinct from [`ModelRef`]'s `id`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct AgentModel {
    /// Model id.
    #[serde(rename = "modelID")]
    pub model_id: String,
    /// Provider id.
    #[serde(rename = "providerID")]
    pub provider_id: String,
}

/// A resolved agent (`app.agents`) — `packages/core/src/agent`. Required: `name`, `mode`,
/// `permission`, `options`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct Agent {
    /// Agent name.
    pub name: String,
    /// Description, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// `subagent` | `primary` | `all`.
    pub mode: String,
    /// Whether this is a built-in (native) agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<bool>,
    /// Hidden from the picker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// Nucleus-sampling top-p.
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    /// Sampling temperature.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// Display color.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Permission ruleset (`[PermissionRule]`).
    pub permission: Vec<PermissionRule>,
    /// Pinned model, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<AgentModel>,
    /// Experimental-mode variant, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
    /// System prompt, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Provider/AI-SDK options (free-form).
    #[schema(value_type = Object)]
    pub options: serde_json::Value,
    /// Max steps per turn, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<f64>,
}

/// A resolved command (`command.list`) — `packages/core/src/command`. Required: `name`, `template`,
/// `hints`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Command {
    /// Command name (the `/name`).
    pub name: String,
    /// Description, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Agent that runs it, if pinned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Model override, if pinned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `command` | `mcp` | `skill`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The prompt template.
    pub template: String,
    /// Whether it runs as a subtask.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtask: Option<bool>,
    /// Argument hints.
    pub hints: Vec<String>,
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

/// 404 body of `project.update` (`{ _tag: "ProjectNotFoundError", projectID, message }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProjectNotFoundError {
    /// Always `"ProjectNotFoundError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// The project id that wasn't found.
    #[serde(rename = "projectID")]
    pub project_id: String,
    /// Human-readable message.
    pub message: String,
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

/// One entry in a project's directory list (`project.directories` item / `ProjectDirectories` array
/// element). Mirrors the golden shape `{ directory, strategy? }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProjectDirectory {
    /// Absolute directory path.
    pub directory: String,
    /// How the directory is associated with the project (e.g. worktree/sandbox), if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
}

/// A provider entry in the resolved config view (`config.providers` item). Mirrors the golden V1
/// `Provider`: `{ id, name, source, env, key?, options, models }`. `options` is free-form and `models`
/// is a `{ [modelID]: Model }` map; both are modeled as JSON values (the contract normalizer drops the
/// map's `additionalProperties`, so the nested `Model` type isn't needed here).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct Provider {
    /// Provider id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Where the provider config came from (`env` | `config` | `custom` | `api`).
    pub source: String,
    /// Environment variable names that enable the provider.
    pub env: Vec<String>,
    /// The active credential key, if resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Provider options (free-form).
    #[schema(value_type = Object)]
    pub options: serde_json::Value,
    /// The provider's models, keyed by model id.
    #[schema(value_type = Object)]
    pub models: serde_json::Value,
}

/// 200 body of `provider.list` (`GET /provider`): `{ all, default, connected }` — all known providers,
/// a `{ [providerID]: defaultModelID }` map, and the ids of providers with resolved credentials.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ProviderListV1Response {
    /// Every known provider.
    pub all: Vec<Provider>,
    /// Default model per provider (`{ [providerID]: modelID }`).
    pub default: std::collections::HashMap<String, String>,
    /// Provider ids that have a resolved credential (env or stored).
    pub connected: Vec<String>,
}

/// A `when` guard on an auth prompt (`{ key, op, value }`) — show the prompt only when an earlier
/// answer `key` is `eq`/`neq` `value`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct AuthPromptWhen {
    /// The earlier prompt key this depends on.
    pub key: String,
    /// `eq` | `neq`.
    pub op: String,
    /// The value to compare against.
    pub value: String,
}

/// A free-text auth prompt (`{ type: "text", key, message, placeholder?, when? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct AuthPromptText {
    /// Always `"text"`.
    pub r#type: String,
    /// The answer's field key.
    pub key: String,
    /// Prompt message.
    pub message: String,
    /// Input placeholder, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
    /// Conditional display guard, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<AuthPromptWhen>,
}

/// One option of a [`AuthPromptSelect`] (`{ label, value, hint? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct AuthSelectOption {
    /// Display label.
    pub label: String,
    /// Option value.
    pub value: String,
    /// Optional hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// A select auth prompt (`{ type: "select", key, message, options, when? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct AuthPromptSelect {
    /// Always `"select"`.
    pub r#type: String,
    /// The answer's field key.
    pub key: String,
    /// Prompt message.
    pub message: String,
    /// The choices.
    pub options: Vec<AuthSelectOption>,
    /// Conditional display guard, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub when: Option<AuthPromptWhen>,
}

/// One prompt of an auth method — a `text` or `select` input.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum AuthPrompt {
    /// Free-text input.
    Text(AuthPromptText),
    /// Single-choice select.
    Select(AuthPromptSelect),
}

/// The reduced project info embedded in a [`GlobalSession`] (`{ id, name?, worktree }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct GlobalSessionProject {
    /// Project id.
    pub id: String,
    /// Display name, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Absolute worktree path.
    pub worktree: String,
}

/// A global session (`experimental.session.list`) — the V1 [`Session`] fields plus the reduced owning
/// `project` (`{ id, name?, worktree }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct GlobalSession {
    /// Session id (`ses_…`).
    pub id: String,
    /// URL-safe slug.
    pub slug: String,
    /// Owning project id.
    #[serde(rename = "projectID")]
    pub project_id: String,
    /// Owning workspace id, if any.
    #[serde(rename = "workspaceID", skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    /// Working directory.
    pub directory: String,
    /// Session path, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Parent session id, if a child.
    #[serde(rename = "parentID", skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// Change summary, if computed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<SessionSummary>,
    /// Accumulated cost (USD).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// Accumulated token usage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<SessionTokens>,
    /// Share link, if shared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<SessionShare>,
    /// Title.
    pub title: String,
    /// Active agent, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Active model, if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// Schema/app version that wrote the session.
    pub version: String,
    /// Free-form metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub metadata: Option<serde_json::Value>,
    /// Timestamps.
    pub time: SessionV1Time,
    /// Permission ruleset (`[PermissionRule]`), if set.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission: Option<Vec<PermissionRule>>,
    /// Revert pointer, if the session is reverted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revert: Option<SessionRevert>,
    /// The owning project (reduced).
    pub project: GlobalSessionProject,
}

/// A PTY session (`pty.{list,get,create,update}`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Pty {
    /// PTY id (`pty_…`).
    pub id: String,
    /// Display title.
    pub title: String,
    /// The spawned command.
    pub command: String,
    /// Command arguments.
    pub args: Vec<String>,
    /// Working directory.
    pub cwd: String,
    /// `running` | `exited`.
    pub status: String,
    /// Process id.
    pub pid: i64,
}

/// One entry of `pty.shells` (`{ path, name, acceptable }`) — an available login shell.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PtyShell {
    /// Absolute path to the shell binary.
    pub path: String,
    /// Shell name (the binary's basename).
    pub name: String,
    /// Whether the shell binary exists / is usable.
    pub acceptable: bool,
}

/// 404 body of the PTY routes (`{ _tag: "PtyNotFoundError", ptyID, message }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PtyNotFoundError {
    /// Always `"PtyNotFoundError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// The PTY id that wasn't found.
    #[serde(rename = "ptyID")]
    pub pty_id: String,
    /// Human-readable message.
    pub message: String,
}

/// `effect_HttpApiError_Forbidden` — Effect's generic HttpApi forbidden error (`{ _tag: "Forbidden" }`).
/// The 403 body of `pty.connect`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[schema(as = effect_HttpApiError_Forbidden)]
pub struct EffectHttpApiForbidden {
    /// Always `"Forbidden"`.
    #[serde(rename = "_tag")]
    pub tag: String,
}

/// 403 body of `pty.connectToken` (`{ _tag: "PtyForbiddenError", message }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PtyForbiddenError {
    /// Always `"PtyForbiddenError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
    /// Human-readable message.
    pub message: String,
}

/// 200 body of `pty.connectToken` — a short-lived WebSocket connect ticket (`{ ticket, expires_in }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PtyConnectToken {
    /// Opaque single-use ticket presented on the WebSocket upgrade.
    pub ticket: String,
    /// Seconds until the ticket expires.
    pub expires_in: i64,
}

/// Request body of `pty.create` (`{ command?, args?, cwd?, title?, env? }`). All optional — an absent
/// `command` spawns the user's login shell.
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PtyCreateRequest {
    /// The command to spawn (defaults to the login shell).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Command arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// Working directory (defaults to the server cwd).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// Display title (defaults to the command).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Extra environment variables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<std::collections::HashMap<String, String>>,
}

/// A PTY's window size in character cells (`{ rows, cols }`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PtySize {
    /// Number of rows.
    pub rows: u16,
    /// Number of columns.
    pub cols: u16,
}

/// Request body of `pty.update` (`{ title?, size? }`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PtyUpdateRequest {
    /// New display title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// New window size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<PtySize>,
}

/// A workspace's `timeUsed` — a number, or one of the JSON-special strings (`NaN`/`Infinity`/…).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(untagged)]
pub enum WorkspaceTimeUsed {
    /// A finite timestamp.
    Number(f64),
    /// A non-finite marker (`NaN` / `Infinity` / `-Infinity`).
    Special(String),
}

/// A workspace (`experimental.workspace.{list,create,remove}`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct Workspace {
    /// Workspace id (`wrk_…`).
    pub id: String,
    /// Adapter type.
    pub r#type: String,
    /// Display name.
    pub name: String,
    /// Git branch, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Working directory, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub directory: Option<String>,
    /// Adapter-specific extra data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<serde_json::Value>,
    /// Owning project id.
    #[serde(rename = "projectID")]
    pub project_id: String,
    /// Last-used timestamp.
    #[serde(rename = "timeUsed")]
    pub time_used: WorkspaceTimeUsed,
}

/// One entry of `experimental.workspace.status` (`{ workspaceID, status }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct WorkspaceStatus {
    /// Workspace id (`wrk_…`).
    #[serde(rename = "workspaceID")]
    pub workspace_id: String,
    /// `connected` | `connecting` | `disconnected` | `error`.
    pub status: String,
}

/// One entry of `experimental.workspace.adapter.list` (`{ type, name, description }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct WorkspaceAdapter {
    /// Adapter type id.
    pub r#type: String,
    /// Display name.
    pub name: String,
    /// Description.
    pub description: String,
}

/// One entry of `sync.history.list` (`{ id, aggregate_id, seq, type, data }`) — a stored event.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SyncEvent {
    /// Event id (`evt_…`).
    pub id: String,
    /// Aggregate id.
    pub aggregate_id: String,
    /// Per-aggregate sequence.
    pub seq: i64,
    /// Event type.
    pub r#type: String,
    /// Event payload (free-form).
    #[schema(value_type = Object)]
    pub data: serde_json::Value,
}

/// 200 body of `experimental.projectCopy.generateName` (`{ name }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct GenerateNameResponse {
    /// The generated copy name.
    pub name: String,
}

/// 200 body of `sync.replay` / `sync.steal` (`{ sessionID }`) — the session the op resolved to.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SyncSessionResult {
    /// The session id (`ses_…`).
    #[serde(rename = "sessionID")]
    pub session_id: String,
}

/// 200 body of `provider.oauth.authorize` (`{ url, method, instructions }`) — the OAuth kickoff.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProviderOauthAuthorization {
    /// The provider authorization URL the user opens.
    pub url: String,
    /// Flow kind: `auto` | `code`.
    pub method: String,
    /// Human-readable next-step instructions.
    pub instructions: String,
}

/// Payload of [`ProviderOauthError`] (`{ providerID?, field?, message?, kind? }`). (Named with an
/// `Oauth` prefix to avoid colliding with the message-part `ProviderAuthErrorData` component.)
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProviderOauthErrorData {
    /// The provider id, if applicable.
    #[serde(rename = "providerID", skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// The offending field, if applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    /// Human-readable message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Error kind discriminator.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// The named provider-OAuth error (golden `ProviderAuthError1`: `{ name, data }`); `name` is one of
/// `BadRequest`/`ProviderAuthOauthMissing`/`…CodeMissing`/`…CallbackFailed`/`ProviderAuthValidationFailed`.
/// (The diff matches by structure, so the Rust name need not equal the golden's.)
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProviderOauthError {
    /// The error name (discriminator).
    pub name: String,
    /// Error payload.
    pub data: ProviderOauthErrorData,
}

/// The 400 union of `provider.oauth.authorize` / `callback`
/// (`anyOf[ProviderAuthError1, InvalidRequestError]`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum ProviderOauthRequestError {
    /// Named provider-auth error.
    Typed(ProviderOauthError),
    /// Schema validation error.
    Invalid(InvalidRequestError),
}

// --- Integrations (`v2.integration.*`) -------------------------------------

/// An OAuth integration method (`{ id, type:"oauth", label, prompts? }`). Reuses [`AuthPrompt`] for
/// `prompts` (structurally identical to the golden `IntegrationTextPrompt`/`IntegrationSelectPrompt`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct IntegrationOauthMethod {
    /// Method id.
    pub id: String,
    /// Always `"oauth"`.
    pub r#type: String,
    /// Display label.
    pub label: String,
    /// Input prompts for the flow.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<AuthPrompt>>,
}

/// An API-key integration method (`{ type:"key", label? }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct IntegrationKeyMethod {
    /// Always `"key"`.
    pub r#type: String,
    /// Display label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// An environment-variable integration method (`{ type:"env", names }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct IntegrationEnvMethod {
    /// Always `"env"`.
    pub r#type: String,
    /// The env var names that activate this integration.
    pub names: Vec<String>,
}

/// One integration auth method (`anyOf[OAuth, Key, Env]`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum IntegrationMethod {
    /// OAuth flow.
    Oauth(IntegrationOauthMethod),
    /// API key.
    Key(IntegrationKeyMethod),
    /// Environment variables.
    Env(IntegrationEnvMethod),
}

/// A credential-backed connection (`{ type:"credential", id, label }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ConnectionCredentialInfo {
    /// Always `"credential"`.
    pub r#type: String,
    /// Credential id.
    pub id: String,
    /// Display label.
    pub label: String,
}

/// An env-backed connection (`{ type:"env", name }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ConnectionEnvInfo {
    /// Always `"env"`.
    pub r#type: String,
    /// The env var name.
    pub name: String,
}

/// One active connection (`anyOf[Credential, Env]`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum ConnectionInfo {
    /// Credential-backed.
    Credential(ConnectionCredentialInfo),
    /// Env-backed.
    Env(ConnectionEnvInfo),
}

/// An integration (`{ id, name, methods, connections }`) — the `data` of `v2.integration.get`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct IntegrationInfo {
    /// Integration id (e.g. `github`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Available auth methods.
    pub methods: Vec<IntegrationMethod>,
    /// Active connections.
    pub connections: Vec<ConnectionInfo>,
}

/// 200 body of `v2.integration.get` (`{ location, data }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct IntegrationGetResponse {
    /// Resolved location context.
    pub location: LocationInfo,
    /// The integration.
    pub data: IntegrationInfo,
}

/// `time` of an [`IntegrationAttempt`] (`{ created, expires }`); each is a number or a JSON-special
/// string (reuses [`WorkspaceTimeUsed`]).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct IntegrationAttemptTime {
    /// Creation time.
    pub created: WorkspaceTimeUsed,
    /// Expiry time.
    pub expires: WorkspaceTimeUsed,
}

/// An in-flight OAuth attempt (`{ attemptID, url, instructions, mode, time }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct IntegrationAttempt {
    /// Attempt id.
    #[serde(rename = "attemptID")]
    pub attempt_id: String,
    /// The authorization URL.
    pub url: String,
    /// Human-readable instructions.
    pub instructions: String,
    /// Flow mode: `auto` | `code`.
    pub mode: String,
    /// Timestamps.
    pub time: IntegrationAttemptTime,
}

/// 200 body of `v2.integration.connect.oauth` (`{ location, data }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct IntegrationOauthResponse {
    /// Resolved location context.
    pub location: LocationInfo,
    /// The started attempt.
    pub data: IntegrationAttempt,
}

// --- global.upgrade --------------------------------------------------------

/// The succeeded arm of `global.upgrade` (`{ success: true, version }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct GlobalUpgradeSuccess {
    /// Always `true`.
    pub success: bool,
    /// The version upgraded to.
    pub version: String,
}

/// The failed arm of `global.upgrade` (`{ success: false, error }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct GlobalUpgradeFailure {
    /// Always `false`.
    pub success: bool,
    /// Failure detail.
    pub error: String,
}

/// 200 body of `global.upgrade` (`anyOf[{success:true,version}, {success:false,error}]`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum GlobalUpgradeResult {
    /// Upgrade succeeded.
    Succeeded(GlobalUpgradeSuccess),
    /// Upgrade failed / not performed.
    Failed(GlobalUpgradeFailure),
}

// --- final create/read ops -------------------------------------------------

/// 200 `data` of `v2.projectCopy.create` (`{ directory }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProjectCopyCopy {
    /// The copy's directory.
    pub directory: String,
}

/// A git worktree (`{ name, branch?, directory }`) — 200 of `worktree.create`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct Worktree {
    /// Worktree name.
    pub name: String,
    /// Checked-out branch, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Worktree directory.
    pub directory: String,
}

/// The 400 union of `experimental.workspace.create`
/// (`anyOf[WorkspaceCreateError, effect_HttpApiError_BadRequest, InvalidRequestError]`;
/// `WorkspaceCreateError` is structurally a [`NamedMessageError`]).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(untagged)]
pub enum WorkspaceCreateRequestError {
    /// Named create error (`{ name, data:{ message } }`).
    Named(NamedMessageError),
    /// Generic bad request.
    BadRequest(EffectHttpApiBadRequest),
    /// Schema validation error.
    Invalid(InvalidRequestError),
}

/// A base attempt-status (`{ status, time }`) — `v2.integration.attempt.status` 200 `data` arm.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct IntegrationAttemptStatusBase {
    /// Status discriminator.
    pub status: String,
    /// Timestamps.
    pub time: IntegrationAttemptTime,
}

/// An attempt-status carrying a message (`{ status, message, time }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct IntegrationAttemptStatusMessage {
    /// Status discriminator.
    pub status: String,
    /// Status detail.
    pub message: String,
    /// Timestamps.
    pub time: IntegrationAttemptTime,
}

/// `data` of `v2.integration.attempt.status` (`anyOf[{status,time}, {status,message,time}]`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(untagged)]
pub enum IntegrationAttemptStatusData {
    /// With a message.
    Message(IntegrationAttemptStatusMessage),
    /// Without a message.
    Base(IntegrationAttemptStatusBase),
}

/// 200 body of `v2.integration.attempt.status` (`{ location, data }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct IntegrationAttemptStatusResponse {
    /// Resolved location context.
    pub location: LocationInfo,
    /// The attempt status.
    pub data: IntegrationAttemptStatusData,
}

/// The Effect HttpApi 500 body (`{ _tag: "InternalServerError" }`). Structurally a tagged marker.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct EffectHttpApiInternalServerError {
    /// Always `"InternalServerError"`.
    #[serde(rename = "_tag")]
    pub tag: String,
}

/// 200 body of `experimental.console.get` (`{ consoleManagedProviders, activeOrgName?, switchableOrgCount }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ConsoleState {
    /// Provider ids managed by the console.
    #[serde(rename = "consoleManagedProviders")]
    pub console_managed_providers: Vec<String>,
    /// The active org name, if any.
    #[serde(rename = "activeOrgName", skip_serializing_if = "Option::is_none")]
    pub active_org_name: Option<String>,
    /// How many orgs the account can switch to.
    #[serde(rename = "switchableOrgCount")]
    pub switchable_org_count: i64,
}

/// One switchable Console org (`experimental.console.listOrgs`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ConsoleOrg {
    /// Account id.
    #[serde(rename = "accountID")]
    pub account_id: String,
    /// Account email.
    #[serde(rename = "accountEmail")]
    pub account_email: String,
    /// Account URL.
    #[serde(rename = "accountUrl")]
    pub account_url: String,
    /// Org id.
    #[serde(rename = "orgID")]
    pub org_id: String,
    /// Org name.
    #[serde(rename = "orgName")]
    pub org_name: String,
    /// Whether this org is active.
    pub active: bool,
}

/// 200 body of `experimental.console.listOrgs` (`{ orgs }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ConsoleOrgsResponse {
    /// The switchable orgs.
    pub orgs: Vec<ConsoleOrg>,
}

/// 200 body of `tui.control.next` (`{ path, body }`) — the next control request the server forwards to
/// a connected TUI. `body` is a free-form value (golden empty/any schema).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct TuiControlNext {
    /// The control request path/kind.
    pub path: String,
    /// The request body (any JSON value).
    pub body: serde_json::Value,
}

/// One way to authenticate a provider (`provider.auth`): `{ type, label, prompts? }` where `type` is
/// `oauth` | `api`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ProviderAuthMethod {
    /// `oauth` | `api`.
    pub r#type: String,
    /// Display label.
    pub label: String,
    /// The inputs to collect, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Vec<AuthPrompt>>,
}

/// 200 body of `config.providers` (GET /config/providers): `{ providers, default }` — the resolved
/// provider list plus a `{ [providerID]: defaultModelID }` map.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ConfigProvidersResponse {
    /// The resolved providers.
    pub providers: Vec<Provider>,
    /// Default model per provider.
    #[serde(rename = "default")]
    pub defaults: BTreeMap<String, String>,
}

/// One level of the config precedence cascade (`config.sources` item): its `code`/`label`, a short
/// `source` description (where it comes from), whether it's read-only, and the raw JSON it contributes
/// (`{}` when it defines nothing). Levels are returned base→top (lowest→highest precedence); a higher
/// level overrides a lower one field-by-field. Mirrors the design's 7-level cascade (REMOTE → GLOBAL →
/// CUSTOM → PROJECT → .OPENCODE → INLINE → MANAGED).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ConfigSourceLevel {
    /// Level code (e.g. `GLOBAL`, `PROJECT`, `MANAGED`).
    pub code: String,
    /// Human label (e.g. `Global`, `Project`, `Managed`).
    pub label: String,
    /// Short description of where the level reads from (e.g. `~/.config/opencode`, `opencode.json`).
    pub source: String,
    /// Whether this level is read-only (a client must not offer to edit it).
    #[serde(rename = "readOnly")]
    pub read_only: bool,
    /// The raw JSON this level contributes (`{}` if it defines nothing).
    #[schema(value_type = Object)]
    pub config: serde_json::Value,
}

/// 200 body of `config.sources` (GET /config/sources): the seven precedence levels (base→top), each
/// with the raw JSON it contributes, so a client can render per-field provenance (which level a value
/// came from) and the precedence cascade. The merged effective config is `config.get`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ConfigSourcesResponse {
    /// The precedence levels, base→top.
    pub levels: Vec<ConfigSourceLevel>,
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

/// 200 body of `v2.provider.test` (POST /api/provider/{providerID}/test): the result of a live
/// connectivity check against the provider's resolved endpoint using its stored credentials. `ok` is the
/// only required field — a failed check is `{ ok: false, error, status? }`, not an HTTP error. `status`
/// is the provider's HTTP status (absent on a transport/timeout error); `models` is the count of models
/// the provider listed, when it returns an OpenAI-style `{ data: [...] }` (best-effort).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ProviderTestResult {
    /// Whether the provider answered successfully (a 2xx from its models endpoint).
    pub ok: bool,
    /// The provider's HTTP status code, if a response was received.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<i64>,
    /// A short error description when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Number of models the provider listed (best-effort, OpenAI-style `{ data: [...] }`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub models: Option<i64>,
}

/// A skill entry (`v2.skill.list` item). Mirrors the golden `SkillV2Info`: `{ name, description?,
/// slash?, location, content }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SkillV2Info {
    /// Skill name.
    pub name: String,
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the skill is invokable as a slash command.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slash: Option<bool>,
    /// Source location (path or origin id).
    pub location: String,
    /// Skill body/content.
    pub content: String,
}

/// One entry of `app.skills` (`GET /skill`, the V1 instance route): `{ name, description?, location,
/// content }`. Like [`SkillV2Info`] but without the `slash` flag (the V1 shape doesn't carry it).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SkillInfo {
    /// Skill name.
    pub name: String,
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Source location (path or origin id).
    pub location: String,
    /// Skill body/content.
    pub content: String,
}

/// One entry of `formatter.status` (`GET /formatter`): a configured code formatter and whether it's
/// enabled (`{ name, extensions, enabled }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct FormatterStatus {
    /// Formatter name.
    pub name: String,
    /// File extensions it handles.
    pub extensions: Vec<String>,
    /// Whether it is enabled (its command resolved).
    pub enabled: bool,
}

/// One entry of `tool.list` (`GET /experimental/tool`): a tool's id, description, and JSON-Schema
/// parameters. `parameters` is a free-form JSON value (the golden schema is the empty/any schema `{}`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ToolListItem {
    /// The tool id (its name).
    pub id: String,
    /// Human/model-facing description.
    pub description: String,
    /// JSON Schema for the tool's arguments.
    pub parameters: serde_json::Value,
}
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SkillListResponse {
    /// The resolved request location.
    pub location: LocationInfo,
    /// The skills.
    pub data: Vec<SkillV2Info>,
}

/// A command entry (`v2.command.list` item). Mirrors the golden `CommandV2Info`: `{ name, template,
/// description?, agent?, model?, subtask? }`. `model` is a typed `{ id, providerID, variant? }` ref
/// (reusing [`ModelRef`]), not a free-form object.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct CommandV2Info {
    /// Command name.
    pub name: String,
    /// Prompt template.
    pub template: String,
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional agent the command runs as.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Optional model override (`{ id, providerID, variant? }`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// Whether the command runs as a subtask.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtask: Option<bool>,
}

/// 200 body of `v2.command.list` (GET /api/command): the `Location.response` wrapper
/// `{ location, data }` around the command list.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct CommandListResponse {
    /// The resolved request location.
    pub location: LocationInfo,
    /// The commands.
    pub data: Vec<CommandV2Info>,
}

/// Request body for `v2.command.set` (PUT /api/command/{commandID}): the writable command fields,
/// persisted to the project's `.opencode/command/{commandID}.md` (frontmatter + body). `template` is
/// the markdown body (the prompt template); `model` reuses [`ModelRef`] (rendered back to the
/// `provider/model` string form in frontmatter).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct CommandWriteRequest {
    /// Prompt template (the markdown body).
    pub template: String,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional agent the command runs as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// Optional model override (`{ id, providerID, variant? }`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// Whether the command runs as a subtask.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtask: Option<bool>,
}

/// 200 body of `v2.command.set` (PUT /api/command/{commandID}): the `Location.response` wrapper
/// `{ location, data }` around the written command (re-read from disk after the write).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct CommandGetResponse {
    /// The resolved request location.
    pub location: LocationInfo,
    /// The written command.
    pub data: CommandV2Info,
}

/// Where a reference comes from (`ReferenceInfo.source`) — the golden union of `ReferenceLocalSource`
/// (`{ type: "local", path, description?, hidden? }`) and `ReferenceGitSource` (`{ type: "git",
/// repository, branch?, description?, hidden? }`), internally tagged on `type`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ReferenceSource {
    /// A local-path reference.
    Local {
        /// Filesystem path.
        path: String,
        /// Optional description.
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Whether the reference is hidden.
        #[serde(skip_serializing_if = "Option::is_none")]
        hidden: Option<bool>,
    },
    /// A git-repository reference.
    Git {
        /// Repository URL.
        repository: String,
        /// Optional branch.
        #[serde(skip_serializing_if = "Option::is_none")]
        branch: Option<String>,
        /// Optional description.
        #[serde(skip_serializing_if = "Option::is_none")]
        description: Option<String>,
        /// Whether the reference is hidden.
        #[serde(skip_serializing_if = "Option::is_none")]
        hidden: Option<bool>,
    },
}

/// A reference entry (`v2.reference.list` item). Mirrors the golden `ReferenceInfo`: `{ name, path,
/// description?, hidden?, source }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct ReferenceInfo {
    /// Reference name.
    pub name: String,
    /// Reference path/identifier.
    pub path: String,
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Whether the reference is hidden.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// Where the reference comes from.
    pub source: ReferenceSource,
}

/// 200 body of `v2.reference.list` (GET /api/reference): the `Location.response` wrapper
/// `{ location, data }` around the reference list.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct ReferenceListResponse {
    /// The resolved request location.
    pub location: LocationInfo,
    /// The references.
    pub data: Vec<ReferenceInfo>,
}

/// The effect of a permission rule (`PermissionV2Effect`): `allow` | `deny` | `ask`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PermissionV2Effect {
    /// Allow the action.
    Allow,
    /// Deny the action.
    Deny,
    /// Ask the user.
    Ask,
}

/// A single permission rule (`PermissionV2Rule`): `{ action, resource, effect }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PermissionV2Rule {
    /// The action the rule governs.
    pub action: String,
    /// The resource the rule governs.
    pub resource: String,
    /// What to do when the rule matches.
    pub effect: PermissionV2Effect,
}

/// Agent operating mode (`AgentV2Info.mode`): `subagent` | `primary` | `all`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentMode {
    /// Usable only as a subagent.
    Subagent,
    /// Usable as a primary agent.
    Primary,
    /// Usable in any mode.
    All,
}

/// The model-request shape an agent carries (`AgentV2Info.request`): `{ headers, body }`, both
/// required. `headers` is a string map; `body` is a free-form request body.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct AgentV2Request {
    /// HTTP headers to attach to the model request.
    pub headers: BTreeMap<String, String>,
    /// Request body overlay.
    #[schema(value_type = Object)]
    pub body: serde_json::Value,
}

/// An agent entry (`v2.agent.list` item). Mirrors the golden `AgentV2Info`: `{ id, model?, request,
/// system?, description?, mode, hidden, color?, steps?, permissions }`. `model` reuses the typed
/// [`ModelRef`]; `color` is a string (the golden `anyOf` of a hex pattern and a named-color enum both
/// collapse to a plain string under the contract normalizer).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct AgentV2Info {
    /// Agent id.
    pub id: String,
    /// Optional model override (`{ id, providerID, variant? }`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// The model-request shape.
    pub request: AgentV2Request,
    /// Optional system prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Optional description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Operating mode.
    pub mode: AgentMode,
    /// Whether the agent is hidden from pickers.
    pub hidden: bool,
    /// Optional display color (hex or named).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Optional step limit (> 0).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<i64>,
    /// The agent's permission ruleset.
    pub permissions: Vec<PermissionV2Rule>,
}

/// 200 body of `v2.agent.list` (GET /api/agent): the `Location.response` wrapper `{ location, data }`
/// around the agent list.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct AgentListResponse {
    /// The resolved request location.
    pub location: LocationInfo,
    /// The agents.
    pub data: Vec<AgentV2Info>,
}

/// Request body for `v2.agent.set` (PUT /api/agent/{agentID}): the writable agent fields, persisted to
/// the project's `.opencode/agent/{agentID}.md` (frontmatter + body). `model` reuses [`ModelRef`];
/// `system` is the markdown body (the system prompt). All fields are optional — a bare `{}` writes an
/// empty agent. The computed `request`/`permissions` and the path-param `id` aren't writable here.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct AgentWriteRequest {
    /// Optional model override (`{ id, providerID, variant? }`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<ModelRef>,
    /// Optional system prompt (the markdown body).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// Optional description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional operating mode (`subagent` | `primary` | `all`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<AgentMode>,
    /// Whether the agent is hidden from pickers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    /// Optional display color (hex or named).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// Optional step limit (> 0).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steps: Option<i64>,
}

/// 200 body of `v2.agent.set` (PUT /api/agent/{agentID}): the `Location.response` wrapper
/// `{ location, data }` around the written agent (re-read from disk after the write).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct AgentGetResponse {
    /// The resolved request location.
    pub location: LocationInfo,
    /// The written agent.
    pub data: AgentV2Info,
}

/// What triggered a permission request (`PermissionV2Request.source`): `{ type, messageID, callID }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PermissionV2Source {
    /// Source kind.
    #[serde(rename = "type")]
    pub kind: String,
    /// The message the request originated from.
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// The tool-call the request originated from.
    #[serde(rename = "callID")]
    pub call_id: String,
}

/// A pending permission request (`v2.permission.request.list` / `v2.session.permission.list` item).
/// Mirrors the golden `PermissionV2Request`: `{ id, sessionID, action, resources, save?, metadata?,
/// source? }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct PermissionV2Request {
    /// Request id.
    pub id: String,
    /// The session the request belongs to.
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// The action being requested.
    pub action: String,
    /// The resources the action targets.
    pub resources: Vec<String>,
    /// Rule patterns offered for "save" (remember this decision).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub save: Option<Vec<String>>,
    /// Free-form request metadata.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Object)]
    pub metadata: Option<serde_json::Value>,
    /// What triggered the request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<PermissionV2Source>,
}

/// A saved permission decision (`v2.permission.saved.list` item). Mirrors the golden
/// `PermissionSavedInfo`: `{ id, projectID, action, resource }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PermissionSavedInfo {
    /// Saved-rule id.
    pub id: String,
    /// The project the rule belongs to.
    #[serde(rename = "projectID")]
    pub project_id: String,
    /// The action the rule governs.
    pub action: String,
    /// The resource the rule governs.
    pub resource: String,
}

/// 200 body of `v2.permission.request.list` (GET /api/permission/request): the `Location.response`
/// wrapper `{ location, data }` around the pending permission requests.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct PermissionRequestListResponse {
    /// The resolved request location.
    pub location: LocationInfo,
    /// The pending requests.
    pub data: Vec<PermissionV2Request>,
}

/// 200 body of `v2.permission.saved.list` (GET /api/permission/saved): `{ data }` (no location).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PermissionSavedListResponse {
    /// The saved rules.
    pub data: Vec<PermissionSavedInfo>,
}

/// Request body for `v2.permission.saved.create` (POST /api/permission/saved): the writable fields of a
/// saved permission rule; the `id` is generated server-side.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct PermissionSavedCreate {
    /// The project the rule belongs to.
    #[serde(rename = "projectID")]
    pub project_id: String,
    /// The action the rule governs (e.g. `bash`, `edit`).
    pub action: String,
    /// The resource the rule governs (e.g. a command pattern or path glob).
    pub resource: String,
}

/// 200 body of `v2.session.permission.list` (GET /api/session/{sessionID}/permission): `{ data }` (no
/// location) around the session's pending permission requests.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
pub struct SessionPermissionListResponse {
    /// The pending requests.
    pub data: Vec<PermissionV2Request>,
}

/// The tool-call a question is attached to (`QuestionV2Request.tool`): `{ messageID, callID }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct QuestionV2Tool {
    /// The message the question originated from.
    #[serde(rename = "messageID")]
    pub message_id: String,
    /// The tool-call the question originated from.
    #[serde(rename = "callID")]
    pub call_id: String,
}

/// A single choice for a question (`QuestionV2Option`): `{ label, description }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct QuestionV2Option {
    /// The option label.
    pub label: String,
    /// The option description.
    pub description: String,
}

/// A single question (`QuestionV2Info`): `{ question, header, options, multiple?, custom? }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct QuestionV2Info {
    /// The question text.
    pub question: String,
    /// A short header/label.
    pub header: String,
    /// The available choices.
    pub options: Vec<QuestionV2Option>,
    /// Whether multiple options may be selected.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub multiple: Option<bool>,
    /// Whether a custom (free-text) answer is allowed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub custom: Option<bool>,
}

/// A pending question request (`v2.question.request.list` / `v2.session.question.list` item). Mirrors
/// the golden `QuestionV2Request`: `{ id, sessionID, questions, tool? }`.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct QuestionV2Request {
    /// Request id.
    pub id: String,
    /// The session the request belongs to.
    #[serde(rename = "sessionID")]
    pub session_id: String,
    /// The questions to answer.
    pub questions: Vec<QuestionV2Info>,
    /// The tool-call the request is attached to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<QuestionV2Tool>,
}

/// 200 body of `v2.question.request.list` (GET /api/question/request): the `Location.response` wrapper
/// `{ location, data }` around the pending question requests.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct QuestionRequestListResponse {
    /// The resolved request location.
    pub location: LocationInfo,
    /// The pending requests.
    pub data: Vec<QuestionV2Request>,
}

/// 200 body of `v2.session.question.list` (GET /api/session/{sessionID}/question): `{ data }` (no
/// location) around the session's pending question requests.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct SessionQuestionListResponse {
    /// The pending requests.
    pub data: Vec<QuestionV2Request>,
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

    #[test]
    fn v1_session_round_trips_with_camelcase_and_ruleset_array() {
        let session = Session {
            id: "ses_1".into(),
            slug: "my-chat".into(),
            project_id: "prj_1".into(),
            workspace_id: None,
            directory: "/repo".into(),
            path: None,
            parent_id: None,
            summary: None,
            cost: Some(0.02),
            tokens: Some(SessionTokens {
                input: 10.0,
                output: 20.0,
                reasoning: 0.0,
                cache: TokenCache {
                    read: 1.0,
                    write: 2.0,
                },
            }),
            share: Some(SessionShare {
                url: "https://opencode.ai/s/x".into(),
            }),
            title: "My chat".into(),
            agent: Some("build".into()),
            model: Some(ModelRef {
                id: "claude".into(),
                provider_id: "anthropic".into(),
                variant: None,
            }),
            version: "1.0".into(),
            metadata: None,
            time: SessionV1Time {
                created: 100,
                updated: 200,
                compacting: None,
                archived: None,
            },
            permission: Some(vec![PermissionRule {
                permission: "bash".into(),
                pattern: "*".into(),
                action: PermissionAction::Ask,
            }]),
            revert: Some(SessionRevert {
                message_id: "msg_9".into(),
                part_id: None,
                snapshot: None,
                diff: None,
            }),
        };
        let json = serde_json::to_value(&session).unwrap();
        assert_eq!(json["projectID"], "prj_1");
        assert_eq!(json["model"]["providerID"], "anthropic");
        assert_eq!(json["share"]["url"], "https://opencode.ai/s/x");
        // PermissionRuleset is a bare array; action serializes lowercase.
        assert_eq!(json["permission"][0]["action"], "ask");
        assert_eq!(json["revert"]["messageID"], "msg_9");
        // Absent optionals are omitted.
        assert!(json.get("workspaceID").is_none());
        let back: Session = serde_json::from_value(json).unwrap();
        assert_eq!(back, session);
    }
}
