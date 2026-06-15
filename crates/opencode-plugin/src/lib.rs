//! Plugin host + MCP client (Phase 5).
//!
//! Built-in plugins/providers are reimplemented natively in Rust (the dynamic `@ai-sdk/*` imports
//! disappear once providers are native). **Third-party** plugins keep their JS hooks, run in an
//! out-of-process Node/Bun host, and talk to the Rust core over local RPC/JSON. MCP uses the `rmcp`
//! crate; the local `@modelcontextprotocol/sdk` patch (reconnect/`onsessionexpired`) must be
//! reproduced — audit it in Phase 0. This is a Phase 0 placeholder.

#![allow(dead_code)]

/// Errors from the plugin host or MCP client.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// The out-of-process JS host could not be reached.
    #[error("plugin host unavailable: {0}")]
    HostUnavailable(String),
    /// An MCP protocol error.
    #[error("mcp error: {0}")]
    Mcp(String),
}
