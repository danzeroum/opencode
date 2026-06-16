//! The **models.dev catalog** — the `providerID → Provider` registry opencode resolves models against
//! (capability flags, pricing, token limits). Port target: `packages/core/src/models-dev.ts`.
//!
//! models.dev serves the catalog as `…/api.json`; opencode caches it on disk (`<cache>/models.json`)
//! and refreshes it periodically. This module models that shape ([`Catalog`]) and parses it — from the
//! on-disk cache the TypeScript side maintains during coexistence, or any path. **Deferred to a
//! follow-up slice** (kept out so this module stays pure and dependency-free): the live HTTP fetch of
//! `api.json`, the periodic refresh, and resolving the platform cache directory.
//!
//! Parsing is intentionally **lenient** — unknown fields are ignored and the schema's required scalars
//! are defaulted — so catalog drift (new providers/models/fields on models.dev) never fails the load.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::ConfigError;

/// The full catalog: `providerID → Provider` (the top-level shape of models.dev `api.json`).
pub type Catalog = BTreeMap<String, Provider>;

/// A provider entry. Mirrors `ModelsDev.Provider`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Provider {
    /// Stable provider id (e.g. `anthropic`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Environment variables that can supply this provider's API key.
    #[serde(default)]
    pub env: Vec<String>,
    /// Base API URL, when the provider advertises one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
    /// npm package implementing the provider SDK, when applicable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    /// `modelID → Model`.
    #[serde(default)]
    pub models: BTreeMap<String, Model>,
}

/// A model entry. Mirrors `ModelsDev.Model`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Model {
    /// Stable model id (e.g. `claude-sonnet-4-6`).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Model family, when grouped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// ISO release date.
    #[serde(default)]
    pub release_date: String,
    /// Accepts file attachments.
    #[serde(default)]
    pub attachment: bool,
    /// Supports reasoning/thinking.
    #[serde(default)]
    pub reasoning: bool,
    /// Honors a `temperature` sampling knob.
    #[serde(default)]
    pub temperature: bool,
    /// Supports tool calls.
    #[serde(default)]
    pub tool_call: bool,
    /// Interleaved-thinking support (`true`, or which reasoning field carries it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interleaved: Option<Interleaved>,
    /// Pricing, when published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<Cost>,
    /// Token limits (context / input / output).
    #[serde(default)]
    pub limit: Limit,
    /// Input/output modalities, when published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modalities: Option<Modalities>,
    /// Free-form experimental knobs (per-mode cost/provider overrides); preserved verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub experimental: Option<serde_json::Value>,
    /// `alpha` / `beta` / `deprecated` (kept as a string so a new status never fails parsing).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Per-model provider hints (`npm` / `api`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ModelProvider>,
}

/// `interleaved`: either `true`, or `{ field: … }` selecting the reasoning field.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Interleaved {
    /// `true`.
    Flag(bool),
    /// `{ field: "reasoning" | "reasoning_content" | "reasoning_details" }` (string for resilience).
    Field {
        /// Which reasoning field carries interleaved thinking.
        field: String,
    },
}

/// Pricing (USD per million tokens, per the models.dev `cost` block).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cost {
    /// Input price.
    pub input: f64,
    /// Output price.
    pub output: f64,
    /// Cache-read price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Cache-write price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    /// Context-size pricing tiers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<CostTier>>,
    /// Legacy over-200k-context pricing override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_over_200k: Option<TierCost>,
}

/// A context-size pricing tier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CostTier {
    /// Input price in this tier.
    pub input: f64,
    /// Output price in this tier.
    pub output: f64,
    /// Cache-read price in this tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Cache-write price in this tier.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
    /// The `{ type: "context", size }` selector for this tier.
    pub tier: Tier,
}

/// The `{ type: "context", size }` selector of a [`CostTier`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tier {
    /// Tier kind (currently always `"context"`; kept as a string for resilience).
    #[serde(rename = "type")]
    pub kind: String,
    /// Context size threshold for this tier.
    pub size: f64,
}

/// A bare input/output price block (the `context_over_200k` override shape).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TierCost {
    /// Input price.
    pub input: f64,
    /// Output price.
    pub output: f64,
    /// Cache-read price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Cache-write price.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// Token limits.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Limit {
    /// Maximum context window (tokens).
    #[serde(default)]
    pub context: f64,
    /// Maximum input tokens, when distinct from `context`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<f64>,
    /// Maximum output tokens.
    #[serde(default)]
    pub output: f64,
}

/// Input/output modalities (kept as strings so a new modality never fails parsing).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Modalities {
    /// Accepted input modalities (`text`/`audio`/`image`/`video`/`pdf`).
    #[serde(default)]
    pub input: Vec<String>,
    /// Produced output modalities.
    #[serde(default)]
    pub output: Vec<String>,
}

/// Per-model provider hints.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelProvider {
    /// npm package for this model's provider SDK.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<String>,
    /// Base API URL for this model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<String>,
}

/// Parse the catalog from the models.dev `api.json` text.
pub fn parse_catalog(text: &str) -> Result<Catalog, ConfigError> {
    serde_json::from_str(text).map_err(|e| ConfigError::Validation(e.to_string()))
}

/// Read + parse the catalog from a file (the on-disk `models.json` cache, or `OPENCODE_MODELS_PATH`).
pub fn load_catalog(path: impl AsRef<Path>) -> Result<Catalog, ConfigError> {
    let text = std::fs::read_to_string(path)?;
    parse_catalog(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "anthropic": {
        "id": "anthropic",
        "name": "Anthropic",
        "env": ["ANTHROPIC_API_KEY"],
        "api": "https://api.anthropic.com",
        "models": {
          "claude-sonnet-4-6": {
            "id": "claude-sonnet-4-6",
            "name": "Claude Sonnet 4.6",
            "release_date": "2026-01-01",
            "attachment": true,
            "reasoning": true,
            "temperature": true,
            "tool_call": true,
            "cost": { "input": 3.0, "output": 15.0, "cache_read": 0.3 },
            "limit": { "context": 200000, "output": 64000 },
            "modalities": { "input": ["text", "image"], "output": ["text"] },
            "status": "beta",
            "futureField": "ignored-for-forward-compat"
          }
        }
      }
    }"#;

    #[test]
    fn parses_provider_and_model() {
        let cat = parse_catalog(SAMPLE).unwrap();
        let anthropic = cat.get("anthropic").expect("provider present");
        assert_eq!(anthropic.name, "Anthropic");
        assert_eq!(anthropic.env, vec!["ANTHROPIC_API_KEY".to_string()]);
        assert_eq!(anthropic.api.as_deref(), Some("https://api.anthropic.com"));

        let model = anthropic
            .models
            .get("claude-sonnet-4-6")
            .expect("model present");
        assert!(model.tool_call);
        assert!(model.reasoning);
        let cost = model.cost.as_ref().unwrap();
        assert_eq!(cost.input, 3.0);
        assert_eq!(cost.cache_read, Some(0.3));
        assert_eq!(model.limit.context, 200_000.0);
        assert_eq!(model.limit.output, 64_000.0);
        assert_eq!(
            model.modalities.as_ref().unwrap().input,
            vec!["text".to_string(), "image".to_string()]
        );
        assert_eq!(model.status.as_deref(), Some("beta"));
    }

    #[test]
    fn ignores_unknown_fields_for_forward_compat() {
        // `futureField` is not modeled; parsing must not fail (catalog-drift resilience).
        let cat = parse_catalog(SAMPLE).unwrap();
        assert!(cat.contains_key("anthropic"));
    }

    #[test]
    fn tolerates_missing_optional_capability_flags() {
        // A minimal model omitting the capability booleans / cost / modalities still parses.
        let json = r#"{ "x": { "id": "x", "name": "X", "models": {
          "m": { "id": "m", "name": "M", "limit": { "context": 1000, "output": 100 } }
        } } }"#;
        let cat = parse_catalog(json).unwrap();
        let m = &cat["x"].models["m"];
        assert!(!m.tool_call);
        assert!(!m.reasoning);
        assert_eq!(m.release_date, "");
        assert!(m.cost.is_none());
        assert_eq!(m.limit.output, 100.0);
    }

    #[test]
    fn invalid_json_is_a_validation_error() {
        assert!(matches!(
            parse_catalog("not json"),
            Err(ConfigError::Validation(_))
        ));
    }

    #[test]
    fn load_from_path_reads_and_parses() {
        let mut p = std::env::temp_dir();
        p.push(format!("oc_catalog_test_{}.json", std::process::id()));
        std::fs::write(&p, SAMPLE).unwrap();
        let cat = load_catalog(&p).expect("loads from disk");
        assert!(cat.contains_key("anthropic"));
        let _ = std::fs::remove_file(&p);
    }
}
