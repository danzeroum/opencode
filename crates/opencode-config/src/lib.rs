//! Configuration: JSONC parsing + the typed config model (Phase 1).
//!
//! Port target: `packages/core/src/config/*` and `packages/core/src/v1/config/*`. JSONC parsing
//! uses the `jsonc-parser` crate (same name/semantics as the TS loader) so comments and trailing
//! commas behave identically. [`Config`] currently models the confirmed top-level fields; any other
//! keys round-trip through [`Config::extra`] so nothing is lost while the schema port continues.

pub mod catalog;

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Errors produced while loading or validating configuration.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Failed to read the config file from disk.
    #[error("config read error: {0}")]
    Read(#[from] std::io::Error),
    /// Failed to parse the JSONC document.
    #[error("config parse error: {0}")]
    Parse(String),
    /// The document parsed but failed to deserialize into the schema.
    #[error("config validation error: {0}")]
    Validation(String),
}

/// Log verbosity (`logLevel`). Mirrors `LogLevel` in `v1/config/config.ts`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// Session sharing behavior (`share`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Share {
    Manual,
    Auto,
    Disabled,
}

/// Auto-update behavior (`autoupdate`): a boolean, or the literal `"notify"`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Autoupdate {
    /// `true`/`false`.
    Flag(bool),
    /// `"notify"`.
    Mode(AutoupdateMode),
}

/// The non-boolean `autoupdate` variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AutoupdateMode {
    Notify,
}

/// Top-level opencode configuration (a faithful subset of the V1 schema; the remainder is preserved
/// in [`extra`](Config::extra)). Field names match the JSON keys exactly.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// JSON schema reference (`$schema`).
    #[serde(rename = "$schema", skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    /// Default shell for the terminal and bash tool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Log level.
    #[serde(rename = "logLevel", skip_serializing_if = "Option::is_none")]
    pub log_level: Option<LogLevel>,
    /// Model in `provider/model` form (e.g. `anthropic/claude-...`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Small model for tasks like title generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub small_model: Option<String>,
    /// Default agent when none is specified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    /// Custom username shown in conversations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// Sharing behavior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share: Option<Share>,
    /// Auto-update behavior.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autoupdate: Option<Autoupdate>,
    /// Enable/disable filesystem snapshot tracking (defaults to true when absent).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<bool>,
    /// Providers to disable from autoloading.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_providers: Option<Vec<String>>,
    /// When set, ONLY these providers are enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled_providers: Option<Vec<String>>,
    /// Any config keys not yet modeled, preserved verbatim so nothing is lost on round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}

/// Parse a JSONC document (comments + trailing commas allowed) into a [`serde_json::Value`].
/// Returns [`serde_json::Value::Null`] for an empty / comment-only document.
pub fn parse_jsonc(text: &str) -> Result<serde_json::Value, ConfigError> {
    jsonc_parser::parse_to_serde_value(text, &Default::default())
        .map_err(|e| ConfigError::Parse(e.to_string()))
        .map(|v| v.unwrap_or(serde_json::Value::Null))
}

/// Parse a JSONC config document into a typed [`Config`].
pub fn load_str(text: &str) -> Result<Config, ConfigError> {
    let value = parse_jsonc(text)?;
    if value.is_null() {
        return Ok(Config::default());
    }
    serde_json::from_value(value).map_err(|e| ConfigError::Validation(e.to_string()))
}

/// Read and parse a JSONC config file into a typed [`Config`].
pub fn load_file(path: impl AsRef<Path>) -> Result<Config, ConfigError> {
    let text = std::fs::read_to_string(path)?;
    load_str(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
        // line comment
        "$schema": "https://opencode.ai/config.json",
        "model": "anthropic/claude-sonnet-4-6",
        "logLevel": "DEBUG",
        "share": "disabled",
        "autoupdate": "notify",
        "disabled_providers": ["openai", "google",], /* trailing comma + block comment */
        "experimental": { "hooks": true }
    }"#;

    #[test]
    fn parses_jsonc_with_comments_and_trailing_commas() {
        let cfg = load_str(SAMPLE).expect("should parse");
        assert_eq!(
            cfg.schema.as_deref(),
            Some("https://opencode.ai/config.json")
        );
        assert_eq!(cfg.model.as_deref(), Some("anthropic/claude-sonnet-4-6"));
        assert_eq!(cfg.log_level, Some(LogLevel::Debug));
        assert_eq!(cfg.share, Some(Share::Disabled));
        assert_eq!(
            cfg.autoupdate,
            Some(Autoupdate::Mode(AutoupdateMode::Notify))
        );
        assert_eq!(
            cfg.disabled_providers,
            Some(vec!["openai".to_string(), "google".to_string()])
        );
    }

    #[test]
    fn preserves_unmodeled_keys_in_extra() {
        let cfg = load_str(SAMPLE).unwrap();
        assert!(
            cfg.extra.contains_key("experimental"),
            "unknown keys must round-trip"
        );
        assert_eq!(cfg.extra["experimental"]["hooks"], serde_json::json!(true));
    }

    #[test]
    fn autoupdate_accepts_boolean() {
        let cfg = load_str(r#"{ "autoupdate": false }"#).unwrap();
        assert_eq!(cfg.autoupdate, Some(Autoupdate::Flag(false)));
    }

    #[test]
    fn empty_or_comment_only_is_default() {
        assert_eq!(load_str("").unwrap(), Config::default());
        assert_eq!(load_str("// just a comment").unwrap(), Config::default());
    }

    #[test]
    fn roundtrip_serializes_without_nulls() {
        let cfg = load_str(r#"{ "model": "anthropic/x" }"#).unwrap();
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(json, serde_json::json!({ "model": "anthropic/x" }));
    }
}
