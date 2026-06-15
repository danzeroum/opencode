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
