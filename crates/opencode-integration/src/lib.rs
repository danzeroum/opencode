//! Integrations (Phase 5): in-core GitHub / GitLab / Slack adapters.
//!
//! Note: the standalone `packages/slack` (a bot that consumes the SDK over HTTP) stays TypeScript;
//! only the integration logic *inside* the core is ported here. This is a Phase 0 placeholder.

#![allow(dead_code)]

/// Errors from an external integration.
#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    /// The upstream service returned an error.
    #[error("integration error: {0}")]
    Upstream(String),
}
