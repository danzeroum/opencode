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
