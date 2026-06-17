//! Projects the parsed **models.dev catalog** ([`opencode_config::catalog::Catalog`]) into the V2 wire
//! contract ([`opencode_proto::ProviderV2Info`] / [`opencode_proto::ModelV2Info`]).
//!
//! Port target: the **pure transform half** of `packages/core/src/plugin/models-dev.ts` — the part
//! that fills a `ProviderV2.Info` / `ModelV2.Info` from a raw `ModelsDev` provider/model (the I/O half
//! — fetch + cache — lives in [`crate::catalog`], and the in-memory catalog state machine, plugin
//! transforms and credential projection are not part of this slice).
//!
//! Field mapping mirrors the TS `refresh()` exactly, starting from the schema's `empty()` defaults:
//! - `provider.enabled` stays `false` here — the credential/integration layer is what flips it on, so
//!   the raw projection always reports a provider as disabled.
//! - `provider.api` / `model.api` pick `aisdk` when an npm package is advertised, else `native`; the
//!   model API uses the model's *own* `provider` hint (not the provider-level npm) and carries the
//!   model id.
//! - `request` keeps the `empty()` shape (`{ headers: {}, body: {}, generation: {}, options: {} }`).
//! - `time.released` parses the ISO `release_date` to epoch-millis (unparseable → `0`, matching the TS
//!   `Number.isFinite(Date.parse(date)) ? … : 0`).
//! - `cost` is the base price plus, when published, the legacy over-200k context tier.
//! - `status` defaults to `active`; `limit` integers are coerced from the catalog's numbers.
//!
//! **Deferred to a follow-up slice** (kept out to bound this one): the experimental-mode `variants`
//! (TS `variants()` / `ModelRequest.normalizeAiSdkOptions`, with its per-package option profiles) —
//! `variants` is projected as empty here.

use std::collections::BTreeMap;

use opencode_config::catalog::{Catalog, Cost, Model, Provider};
use opencode_proto::{
    EffectNumber, ModelApi, ModelCapabilities, ModelCost, ModelCostCache, ModelCostTier,
    ModelGeneration, ModelLimit, ModelRequest, ModelStatus, ModelTime, ModelV2Info, ProviderApi,
    ProviderEnabled, ProviderRequest, ProviderV2Info,
};

/// Project a single catalog [`Provider`] into its V2 wire form. `enabled` is always `false` (the raw
/// projection; the credential layer enables providers downstream).
pub fn provider_info(provider: &Provider) -> ProviderV2Info {
    ProviderV2Info {
        id: provider.id.clone(),
        name: provider.name.clone(),
        enabled: ProviderEnabled::Disabled(false),
        env: provider.env.clone(),
        api: match provider.npm.as_ref() {
            Some(npm) => ProviderApi::Aisdk {
                package: npm.clone(),
                url: provider.api.clone(),
                settings: None,
            },
            None => ProviderApi::Native {
                url: provider.api.clone(),
                settings: serde_json::json!({}),
            },
        },
        request: ProviderRequest {
            headers: BTreeMap::new(),
            body: serde_json::json!({}),
        },
    }
}

/// Project a single catalog [`Model`] (under `provider_id`) into its V2 wire form.
pub fn model_info(provider_id: &str, model: &Model) -> ModelV2Info {
    let model_npm = model.provider.as_ref().and_then(|p| p.npm.as_ref());
    let model_api = model.provider.as_ref().and_then(|p| p.api.clone());
    ModelV2Info {
        id: model.id.clone(),
        provider_id: provider_id.to_string(),
        family: model.family.clone(),
        name: model.name.clone(),
        api: match model_npm {
            Some(npm) => ModelApi::Aisdk {
                id: model.id.clone(),
                package: npm.clone(),
                url: model_api,
                settings: None,
            },
            None => ModelApi::Native {
                id: model.id.clone(),
                url: model_api,
                settings: serde_json::json!({}),
            },
        },
        capabilities: ModelCapabilities {
            tools: model.tool_call,
            input: model
                .modalities
                .as_ref()
                .map(|m| m.input.clone())
                .unwrap_or_default(),
            output: model
                .modalities
                .as_ref()
                .map(|m| m.output.clone())
                .unwrap_or_default(),
        },
        request: ModelRequest {
            headers: BTreeMap::new(),
            body: serde_json::json!({}),
            generation: Some(ModelGeneration::default()),
            options: Some(serde_json::json!({})),
            variant: None,
        },
        // Experimental-mode variants are deferred to a follow-up slice (see module docs).
        variants: Vec::new(),
        time: ModelTime {
            released: EffectNumber::Finite(released_millis(&model.release_date)),
        },
        cost: cost(model.cost.as_ref()),
        status: status(model.status.as_deref()),
        enabled: true,
        limit: ModelLimit {
            context: model.limit.context as i64,
            input: model.limit.input.map(|v| v as i64),
            output: model.limit.output as i64,
        },
    }
}

/// Project every provider in a [`Catalog`] (ordered by id via the catalog's `BTreeMap`).
pub fn providers(catalog: &Catalog) -> Vec<ProviderV2Info> {
    catalog.values().map(provider_info).collect()
}

/// Project every model across every provider in a [`Catalog`].
pub fn models(catalog: &Catalog) -> Vec<ModelV2Info> {
    catalog
        .values()
        .flat_map(|provider| {
            provider
                .models
                .values()
                .map(move |model| model_info(&provider.id, model))
        })
        .collect()
}

/// Build the V2 `cost` array from the catalog's optional cost block: the base price, plus the legacy
/// over-200k context tier when published. Mirrors the TS `cost()` (which consults only
/// `context_over_200k`, not the generic `tiers` list).
fn cost(input: Option<&Cost>) -> Vec<ModelCost> {
    let base = ModelCost {
        tier: None,
        input: input.map(|c| c.input).unwrap_or(0.0),
        output: input.map(|c| c.output).unwrap_or(0.0),
        cache: ModelCostCache {
            read: input.and_then(|c| c.cache_read).unwrap_or(0.0),
            write: input.and_then(|c| c.cache_write).unwrap_or(0.0),
        },
    };
    match input.and_then(|c| c.context_over_200k.as_ref()) {
        None => vec![base],
        Some(over) => vec![
            base,
            ModelCost {
                tier: Some(ModelCostTier {
                    kind: "context".to_string(),
                    size: 200_000,
                }),
                input: over.input,
                output: over.output,
                cache: ModelCostCache {
                    read: over.cache_read.unwrap_or(0.0),
                    write: over.cache_write.unwrap_or(0.0),
                },
            },
        ],
    }
}

/// Map the catalog's free-form status string to the closed V2 enum, defaulting to `active` (matching
/// the TS `model.status ?? "active"`; an unknown/forward-compat status also falls back to `active`).
fn status(status: Option<&str>) -> ModelStatus {
    match status {
        Some("alpha") => ModelStatus::Alpha,
        Some("beta") => ModelStatus::Beta,
        Some("deprecated") => ModelStatus::Deprecated,
        _ => ModelStatus::Active,
    }
}

/// Parse an ISO `release_date` to epoch-milliseconds, returning `0` when it can't be parsed (mirrors
/// the TS `Number.isFinite(Date.parse(date)) ? Date.parse(date) : 0`). models.dev publishes a
/// date-only `YYYY-MM-DD` (interpreted as UTC midnight); any other/empty form yields `0`.
fn released_millis(date: &str) -> f64 {
    let date = date.split('T').next().unwrap_or(date);
    let mut parts = date.split('-');
    let parsed = (|| {
        let year: i64 = parts.next()?.parse().ok()?;
        let month: i64 = parts.next()?.parse().ok()?;
        let day: i64 = parts.next()?.parse().ok()?;
        if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        Some(days_from_civil(year, month, day) * 86_400_000)
    })();
    parsed.map(|ms| ms as f64).unwrap_or(0.0)
}

/// Days since the Unix epoch (1970-01-01) for a proleptic-Gregorian `y-m-d`, via Howard Hinnant's
/// `days_from_civil` algorithm. `m` is 1..=12, `d` is 1..=31.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode_config::catalog::parse_catalog;

    const SAMPLE: &str = r#"{
      "anthropic": {
        "id": "anthropic",
        "name": "Anthropic",
        "env": ["ANTHROPIC_API_KEY"],
        "api": "https://api.anthropic.com",
        "npm": "@ai-sdk/anthropic",
        "models": {
          "claude-sonnet-4-6": {
            "id": "claude-sonnet-4-6",
            "name": "Claude Sonnet 4.6",
            "family": "claude-sonnet",
            "release_date": "2026-01-01",
            "tool_call": true,
            "reasoning": true,
            "cost": { "input": 3.0, "output": 15.0, "cache_read": 0.3, "cache_write": 3.75 },
            "limit": { "context": 200000, "input": 180000, "output": 64000 },
            "modalities": { "input": ["text", "image"], "output": ["text"] },
            "status": "beta"
          }
        }
      },
      "local": {
        "id": "local",
        "name": "Local",
        "env": [],
        "models": {
          "tiny": {
            "id": "tiny",
            "name": "Tiny",
            "release_date": "",
            "limit": { "context": 1000, "output": 100 }
          }
        }
      }
    }"#;

    #[test]
    fn provider_aisdk_when_npm_present() {
        let cat = parse_catalog(SAMPLE).unwrap();
        let p = provider_info(&cat["anthropic"]);
        assert_eq!(p.id, "anthropic");
        assert_eq!(p.name, "Anthropic");
        assert_eq!(p.env, vec!["ANTHROPIC_API_KEY".to_string()]);
        assert_eq!(p.enabled, ProviderEnabled::Disabled(false));
        assert_eq!(
            p.api,
            ProviderApi::Aisdk {
                package: "@ai-sdk/anthropic".to_string(),
                url: Some("https://api.anthropic.com".to_string()),
                settings: None,
            }
        );
        // request keeps the empty() shape.
        assert!(p.request.headers.is_empty());
        assert_eq!(p.request.body, serde_json::json!({}));
    }

    #[test]
    fn provider_native_when_npm_absent() {
        let cat = parse_catalog(SAMPLE).unwrap();
        let p = provider_info(&cat["local"]);
        assert_eq!(
            p.api,
            ProviderApi::Native {
                url: None,
                settings: serde_json::json!({}),
            }
        );
        assert!(p.env.is_empty());
    }

    #[test]
    fn model_maps_core_fields() {
        let cat = parse_catalog(SAMPLE).unwrap();
        let m = model_info("anthropic", &cat["anthropic"].models["claude-sonnet-4-6"]);
        assert_eq!(m.id, "claude-sonnet-4-6");
        assert_eq!(m.provider_id, "anthropic");
        assert_eq!(m.family.as_deref(), Some("claude-sonnet"));
        assert!(m.enabled);
        assert_eq!(m.status, ModelStatus::Beta);
        // No per-model provider hint → native, carrying the model id.
        assert_eq!(
            m.api,
            ModelApi::Native {
                id: "claude-sonnet-4-6".to_string(),
                url: None,
                settings: serde_json::json!({}),
            }
        );
        assert!(m.capabilities.tools);
        assert_eq!(m.capabilities.input, vec!["text", "image"]);
        assert_eq!(m.capabilities.output, vec!["text"]);
        assert_eq!(m.limit.context, 200_000);
        assert_eq!(m.limit.input, Some(180_000));
        assert_eq!(m.limit.output, 64_000);
        assert!(m.variants.is_empty());
        // request keeps the empty() shape (generation/options present but empty).
        assert_eq!(m.request.generation, Some(ModelGeneration::default()));
        assert_eq!(m.request.options, Some(serde_json::json!({})));
        assert_eq!(m.time.released, EffectNumber::Finite(1_767_225_600_000.0));
        // base-only cost (no over-200k tier).
        assert_eq!(m.cost.len(), 1);
        assert_eq!(m.cost[0].tier, None);
        assert_eq!(m.cost[0].input, 3.0);
        assert_eq!(m.cost[0].cache.read, 0.3);
        assert_eq!(m.cost[0].cache.write, 3.75);
    }

    #[test]
    fn model_defaults_when_minimal() {
        let cat = parse_catalog(SAMPLE).unwrap();
        let m = model_info("local", &cat["local"].models["tiny"]);
        assert_eq!(m.status, ModelStatus::Active); // status absent → active
        assert!(!m.capabilities.tools); // tool_call absent → false
        assert!(m.capabilities.input.is_empty()); // no modalities → []
        assert_eq!(m.time.released, EffectNumber::Finite(0.0)); // empty release_date → 0
        assert_eq!(m.cost.len(), 1); // no cost → single zeroed base
        assert_eq!(m.cost[0].input, 0.0);
        assert_eq!(m.cost[0].cache.read, 0.0);
        assert_eq!(m.limit.input, None);
    }

    #[test]
    fn model_aisdk_when_model_provider_npm_present() {
        let json = r#"{ "x": { "id": "x", "name": "X", "models": {
          "m": { "id": "m", "name": "M", "limit": { "context": 1, "output": 1 },
                 "provider": { "npm": "@ai-sdk/openai", "api": "https://api.openai.com" } }
        } } }"#;
        let cat = parse_catalog(json).unwrap();
        let m = model_info("x", &cat["x"].models["m"]);
        assert_eq!(
            m.api,
            ModelApi::Aisdk {
                id: "m".to_string(),
                package: "@ai-sdk/openai".to_string(),
                url: Some("https://api.openai.com".to_string()),
                settings: None,
            }
        );
    }

    #[test]
    fn cost_includes_over_200k_tier() {
        let json = r#"{ "x": { "id": "x", "name": "X", "models": {
          "m": { "id": "m", "name": "M", "limit": { "context": 1, "output": 1 },
                 "cost": { "input": 3.0, "output": 15.0,
                           "context_over_200k": { "input": 6.0, "output": 22.5, "cache_read": 0.6 } } }
        } } }"#;
        let cat = parse_catalog(json).unwrap();
        let m = model_info("x", &cat["x"].models["m"]);
        assert_eq!(m.cost.len(), 2);
        assert_eq!(m.cost[0].tier, None);
        assert_eq!(m.cost[0].input, 3.0);
        assert_eq!(
            m.cost[1].tier,
            Some(ModelCostTier {
                kind: "context".to_string(),
                size: 200_000,
            })
        );
        assert_eq!(m.cost[1].input, 6.0);
        assert_eq!(m.cost[1].output, 22.5);
        assert_eq!(m.cost[1].cache.read, 0.6);
        assert_eq!(m.cost[1].cache.write, 0.0); // cache_write absent → 0
    }

    #[test]
    fn status_maps_and_defaults() {
        assert_eq!(status(Some("alpha")), ModelStatus::Alpha);
        assert_eq!(status(Some("beta")), ModelStatus::Beta);
        assert_eq!(status(Some("deprecated")), ModelStatus::Deprecated);
        assert_eq!(status(Some("active")), ModelStatus::Active);
        assert_eq!(status(None), ModelStatus::Active);
        assert_eq!(status(Some("preview")), ModelStatus::Active); // unknown → active
    }

    #[test]
    fn released_millis_parses_date_only_and_falls_back() {
        assert_eq!(released_millis("2026-01-01"), 1_767_225_600_000.0);
        assert_eq!(released_millis("1970-01-01"), 0.0);
        assert_eq!(released_millis("2026-01-01T12:00:00Z"), 1_767_225_600_000.0);
        assert_eq!(released_millis(""), 0.0);
        assert_eq!(released_millis("not-a-date"), 0.0);
        assert_eq!(released_millis("2026-13-01"), 0.0); // invalid month
    }

    #[test]
    fn whole_catalog_projection_counts() {
        let cat = parse_catalog(SAMPLE).unwrap();
        let providers = providers(&cat);
        let models = models(&cat);
        assert_eq!(providers.len(), 2);
        assert_eq!(models.len(), 2);
        // BTreeMap order: "anthropic" before "local".
        assert_eq!(providers[0].id, "anthropic");
        assert_eq!(providers[1].id, "local");
    }
}
