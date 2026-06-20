//! MCP (Model Context Protocol) wire types — `mcp.status` (`GET /mcp`).
//!
//! The contract's `mcp.status` returns a map `{ [server]: MCPStatus }`. `MCPStatus` is an `anyOf`
//! of five named variants (`Connected`/`Disabled`/`Failed`/`NeedsAuth`/`NeedsClientRegistration`),
//! each discriminated by a `status` string — i.e. a serde **internally-tagged** enum on `status`.
//! Modeled here so the value is usable once the MCP host (Phase 3b, `rmcp`) lands; until then the
//! route returns an empty map (no servers connected).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Status of a single MCP server. Mirrors the golden `MCPStatus` (`anyOf` of `MCPStatusConnected`/
/// `MCPStatusDisabled`/`MCPStatusFailed`/`MCPStatusNeedsAuth`/`MCPStatusNeedsClientRegistration`),
/// internally tagged on `status` (snake_case).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum McpStatus {
    /// Server connected and ready.
    Connected,
    /// Server disabled by configuration.
    Disabled,
    /// Connection attempt failed.
    Failed {
        /// Failure detail.
        error: String,
    },
    /// Server requires OAuth authentication before use.
    NeedsAuth,
    /// Server requires dynamic client registration.
    NeedsClientRegistration {
        /// Registration detail.
        error: String,
    },
}
