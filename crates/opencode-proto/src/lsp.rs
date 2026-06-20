//! LSP (Language Server Protocol) wire types — `lsp.status` (`GET /lsp`).
//!
//! `lsp.status` returns a list of `LSPStatus` entries describing each language server the instance
//! has spun up. The set of running servers is live runtime state owned by the LSP host; until that
//! host exists in Rust the route returns an empty list (no servers running) — the same wired-empty
//! pattern as `mcp.status`/`permission.list`/`question.list`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Connection state of a language server (`LSPStatus.status`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LspServerStatus {
    /// Server connected and serving requests.
    Connected,
    /// Server failed to start or crashed.
    Error,
}

/// Status of a single language server. Mirrors the golden `LSPStatus`:
/// `{ id, name, root, status }`, all required.
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq, Eq)]
pub struct LspStatus {
    /// Stable server id (e.g. the language id).
    pub id: String,
    /// Human-readable server name.
    pub name: String,
    /// Workspace root the server was started in.
    pub root: String,
    /// Connection state.
    pub status: LspServerStatus,
}
