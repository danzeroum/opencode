//! Wire types for the **external HTTP contract** — these mirror `packages/sdk/openapi.json`
//! and are the strangler-fig migration contract. Every type here derives `Serialize`,
//! `Deserialize` and `ToSchema` so the generated OpenAPI can be diffed against the golden spec.
//!
//! Keep this crate free of server/runtime dependencies: it is the single target of the
//! contract tests (`xtask openapi-diff`).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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
}
