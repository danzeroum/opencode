//! Configuration: JSONC parsing, the config schema, and catalog (`models.dev`) merge.
//!
//! Port target (Phase 1): `packages/core/src/config/*` and `packages/core/src/v1/config/*`
//! (~30 files). JSONC parsing will use the `jsonc-parser` crate for byte-for-byte parity with the
//! current loader. This is a Phase 0 placeholder.

#![allow(dead_code)]

/// Errors produced while loading or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Failed to parse the JSONC document.
    #[error("config parse error: {0}")]
    Parse(String),
    /// The document parsed but failed schema validation.
    #[error("config validation error: {0}")]
    Validation(String),
}
