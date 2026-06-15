//! Tools and system helpers (Phase 1): the ~18 tools (bash, read, edit, glob, grep, git, skill, …)
//! plus filesystem / git / search / PTY / process helpers.
//!
//! Notable crate choices (see plan): the ripgrep libraries (`grep`, `ignore`, `globset`) for
//! search parity, `gix` for git, `portable-pty` for PTY, `tokio::process` for spawning. NB:
//! **tree-sitter is out of scope** — the backend only has a `// TODO` in `bash.ts`; real use is the
//! TUI. This is a Phase 0 placeholder.

#![allow(dead_code)]

/// Errors produced while executing a tool.
#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    /// The tool ran but failed (non-zero exit, validation failure, etc.).
    #[error("tool failure: {0}")]
    Failure(String),
    /// The tool was denied by the permission system.
    #[error("permission denied: {0}")]
    Denied(String),
}
