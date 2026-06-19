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
//! Experimental-mode `variants` are projected from `model.experimental.modes` ([`variants`]), mirroring
//! the TS `variants()` / `ModelRequest.normalizeAiSdkOptions` (per-package option profiles). The
//! stored-credential enabling path (`enabled = { via: "credential" }`) still needs the credential store
//! and is a documented follow-up.

use std::collections::BTreeMap;

use opencode_config::catalog::{Catalog, Cost, Model, Provider};
use opencode_proto::{
    EffectNumber, ModelApi, ModelCapabilities, ModelCost, ModelCostCache, ModelCostTier,
    ModelGeneration, ModelLimit, ModelRequest, ModelStatus, ModelTime, ModelV2Info, ModelVariant,
    ProviderApi, ProviderEnabled, ProviderRequest, ProviderV2Info,
};

/// A provider's env-derived `enabled`: the first of its `env` vars that `env` reports set
/// (`{ via: "env", name }`), else disabled. Mirrors `packages/core/src/plugin/env.ts`. The
/// stored-credential path (`{ via: "credential" }`) needs the credential store — a documented
/// follow-up — so a provider with no matching env var reports disabled here.
fn enabled_from_env(provider: &Provider, env: &impl Fn(&str) -> bool) -> ProviderEnabled {
    provider
        .env
        .iter()
        .find(|name| env(name))
        .map_or(ProviderEnabled::Disabled(false), |name| {
            ProviderEnabled::Env {
                via: "env".to_string(),
                name: name.clone(),
            }
        })
}

/// Whether an `enabled` value counts as available (anything other than the literal `false`; mirrors
/// the TS `provider.enabled !== false`).
fn is_available(enabled: &ProviderEnabled) -> bool {
    !matches!(enabled, ProviderEnabled::Disabled(false))
}

/// A model's release time as epoch-millis (`0` for the non-finite arm), for release-date ordering.
fn released_ms(model: &ModelV2Info) -> f64 {
    match model.time.released {
        EffectNumber::Finite(ms) => ms,
        EffectNumber::NonFinite(_) => 0.0,
    }
}

/// Project a single catalog [`Provider`] into its V2 wire form, deriving `enabled` from `env`
/// (an "is this variable set?" predicate — kept injected so the projection stays pure + testable).
pub fn provider_info(provider: &Provider, env: &impl Fn(&str) -> bool) -> ProviderV2Info {
    ProviderV2Info {
        id: provider.id.clone(),
        name: provider.name.clone(),
        enabled: enabled_from_env(provider, env),
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

/// Project a single catalog [`Model`] (under `provider_id`, whose provider-level npm — if any — is
/// `provider_npm`) into its V2 wire form.
pub fn model_info(provider_id: &str, provider_npm: Option<&str>, model: &Model) -> ModelV2Info {
    let model_npm = model.provider.as_ref().and_then(|p| p.npm.as_ref());
    let model_api = model.provider.as_ref().and_then(|p| p.api.clone());
    // Variant option-partitioning uses the model's own npm, falling back to the provider's.
    let package_name = model_npm.map(String::as_str).or(provider_npm);
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
        variants: variants(model, package_name),
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

/// Project a model's experimental `modes` into request [`ModelVariant`]s. Mirrors the TS `variants()` in
/// `plugin/models-dev.ts`: each `experimental.modes[id]` carries an optional `provider.{ body, headers }`;
/// the body is partitioned by [`normalize_aisdk_options`] (driven by `package_name`) into generation
/// knobs / provider options / passthrough body. Absent `experimental.modes` yields no variants.
fn variants(model: &Model, package_name: Option<&str>) -> Vec<ModelVariant> {
    let Some(modes) = model
        .experimental
        .as_ref()
        .and_then(|e| e.get("modes"))
        .and_then(|m| m.as_object())
    else {
        return Vec::new();
    };
    modes
        .iter()
        .map(|(id, item)| {
            let provider = item.get("provider");
            let body_input = provider
                .and_then(|p| p.get("body"))
                .and_then(|b| b.as_object())
                .cloned()
                .unwrap_or_default();
            let headers = provider
                .and_then(|p| p.get("headers"))
                .and_then(|h| h.as_object())
                .map(|h| {
                    h.iter()
                        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                        .collect()
                })
                .unwrap_or_default();
            let (generation, options, body) = normalize_aisdk_options(package_name, &body_input);
            ModelVariant {
                id: id.clone(),
                headers,
                body,
                generation: Some(generation),
                options: Some(options),
            }
        })
        .collect()
}

/// Partition AI-SDK-shaped request options into generation knobs, provider options, and passthrough
/// body. Mirrors `ModelRequest.normalizeAiSdkOptions`: known generation keys (numbers, or a string
/// array for `stop`) become [`ModelGeneration`] fields; keys the package's profile recognizes become
/// `options`; everything else stays in `body`.
fn normalize_aisdk_options(
    package_name: Option<&str>,
    input: &serde_json::Map<String, serde_json::Value>,
) -> (ModelGeneration, serde_json::Value, serde_json::Value) {
    let mut generation = ModelGeneration::default();
    let mut options = serde_json::Map::new();
    let mut body = serde_json::Map::new();
    for (key, value) in input {
        match generation_key(key) {
            Some("stop") => {
                if let Some(stop) = as_string_array(value) {
                    generation.stop = Some(stop);
                    continue;
                }
            }
            Some(field) => {
                if let Some(n) = value.as_f64() {
                    set_generation_number(&mut generation, field, n);
                    continue;
                }
            }
            None => {}
        }
        match semantic_option(package_name, key) {
            Some(canonical) => {
                options.insert(canonical.to_string(), value.clone());
            }
            None => {
                body.insert(key.clone(), value.clone());
            }
        }
    }
    (
        generation,
        serde_json::Value::Object(options),
        serde_json::Value::Object(body),
    )
}

/// Map an AI-SDK request key to its canonical [`ModelGeneration`] field name (or `None` if it isn't a
/// generation knob). Mirrors the TS `generationKeys` map.
fn generation_key(key: &str) -> Option<&'static str> {
    match key {
        "maxOutputTokens" | "maxTokens" => Some("maxTokens"),
        "temperature" => Some("temperature"),
        "topP" => Some("topP"),
        "topK" => Some("topK"),
        "frequencyPenalty" => Some("frequencyPenalty"),
        "presencePenalty" => Some("presencePenalty"),
        "seed" => Some("seed"),
        "stopSequences" | "stop" => Some("stop"),
        _ => None,
    }
}

/// Set the [`ModelGeneration`] numeric field named by `field` (a canonical key from [`generation_key`]).
fn set_generation_number(generation: &mut ModelGeneration, field: &str, value: f64) {
    let value = Some(EffectNumber::Finite(value));
    match field {
        "maxTokens" => generation.max_tokens = value,
        "temperature" => generation.temperature = value,
        "topP" => generation.top_p = value,
        "topK" => generation.top_k = value,
        "frequencyPenalty" => generation.frequency_penalty = value,
        "presencePenalty" => generation.presence_penalty = value,
        "seed" => generation.seed = value,
        _ => {}
    }
}

/// The canonical provider-option name for `key` under `package_name`'s profile, or `None` if the key
/// isn't a recognized option (so it falls through to the passthrough body). Mirrors the TS `profiles`.
fn semantic_option(package_name: Option<&str>, key: &str) -> Option<&'static str> {
    let table: &[(&str, &str)] = match package_name {
        Some("@ai-sdk/openai") => &[
            ("store", "store"),
            ("promptCacheKey", "promptCacheKey"),
            ("reasoningEffort", "reasoningEffort"),
            ("reasoningSummary", "reasoningSummary"),
            ("include", "include"),
            ("textVerbosity", "textVerbosity"),
            ("serviceTier", "serviceTier"),
            ("service_tier", "serviceTier"),
        ],
        Some("@ai-sdk/openai-compatible") => &[
            ("store", "store"),
            ("promptCacheKey", "promptCacheKey"),
            ("reasoningEffort", "reasoningEffort"),
            ("reasoning_effort", "reasoningEffort"),
        ],
        Some("@ai-sdk/anthropic") => &[("thinking", "thinking")],
        _ => &[],
    };
    table
        .iter()
        .find(|(k, _)| *k == key)
        .map(|(_, canonical)| *canonical)
}

/// A JSON value as a `Vec<String>` when it's an array of strings (else `None`) — mirrors the TS
/// `Array.isArray(value) && value.every(item => typeof item === "string")` check for `stop`.
fn as_string_array(value: &serde_json::Value) -> Option<Vec<String>> {
    let array = value.as_array()?;
    array
        .iter()
        .map(|item| item.as_str().map(String::from))
        .collect()
}

/// Project every provider in a [`Catalog`] (ordered by id via the catalog's `BTreeMap`), deriving
/// each `enabled` from `env`.
pub fn providers(catalog: &Catalog, env: &impl Fn(&str) -> bool) -> Vec<ProviderV2Info> {
    catalog.values().map(|p| provider_info(p, env)).collect()
}

/// Project every model across every provider in a [`Catalog`].
pub fn models(catalog: &Catalog) -> Vec<ModelV2Info> {
    catalog
        .values()
        .flat_map(|provider| {
            let npm = provider.npm.as_deref();
            provider
                .models
                .values()
                .map(move |model| model_info(&provider.id, npm, model))
        })
        .collect()
}

/// The `available()` provider list (TS `catalog.provider.available()`): every provider whose
/// env-derived `enabled` isn't `false`.
pub fn available_providers(catalog: &Catalog, env: &impl Fn(&str) -> bool) -> Vec<ProviderV2Info> {
    providers(catalog, env)
        .into_iter()
        .filter(|p| is_available(&p.enabled))
        .collect()
}

/// The `available()` model list (TS `catalog.model.available()`): every enabled model of an
/// env-enabled provider, ordered by release date (newest first).
pub fn available_models(catalog: &Catalog, env: &impl Fn(&str) -> bool) -> Vec<ModelV2Info> {
    let mut out: Vec<ModelV2Info> = catalog
        .values()
        .filter(|provider| is_available(&enabled_from_env(provider, env)))
        .flat_map(|provider| {
            let npm = provider.npm.as_deref();
            provider
                .models
                .values()
                .map(move |model| model_info(&provider.id, npm, model))
        })
        .filter(|model| model.enabled)
        .collect();
    out.sort_by(|a, b| released_ms(b).total_cmp(&released_ms(a)));
    out
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
        let p = provider_info(&cat["anthropic"], &|_| false);
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
        let p = provider_info(&cat["local"], &|_| false);
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
        let m = model_info(
            "anthropic",
            cat["anthropic"].npm.as_deref(),
            &cat["anthropic"].models["claude-sonnet-4-6"],
        );
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
        let m = model_info(
            "local",
            cat["local"].npm.as_deref(),
            &cat["local"].models["tiny"],
        );
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
        let m = model_info("x", cat["x"].npm.as_deref(), &cat["x"].models["m"]);
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
        let m = model_info("x", cat["x"].npm.as_deref(), &cat["x"].models["m"]);
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
        let providers = providers(&cat, &|_| false);
        let models = models(&cat);
        assert_eq!(providers.len(), 2);
        assert_eq!(models.len(), 2);
        // BTreeMap order: "anthropic" before "local".
        assert_eq!(providers[0].id, "anthropic");
        assert_eq!(providers[1].id, "local");
    }

    #[test]
    fn enabled_from_env_picks_first_present_var() {
        let cat = parse_catalog(
            r#"{ "p": { "id": "p", "name": "P", "env": ["FIRST_KEY", "SECOND_KEY"], "models": {} } }"#,
        )
        .unwrap();
        let provider = &cat["p"];
        // None set → disabled.
        assert_eq!(
            enabled_from_env(provider, &|_| false),
            ProviderEnabled::Disabled(false)
        );
        // Only the second set → that one wins.
        assert_eq!(
            enabled_from_env(provider, &|n| n == "SECOND_KEY"),
            ProviderEnabled::Env {
                via: "env".to_string(),
                name: "SECOND_KEY".to_string(),
            }
        );
        // Both set → the first in the list wins.
        assert_eq!(
            enabled_from_env(provider, &|_| true),
            ProviderEnabled::Env {
                via: "env".to_string(),
                name: "FIRST_KEY".to_string(),
            }
        );
    }

    #[test]
    fn available_filters_to_env_enabled_providers_and_models() {
        let cat = parse_catalog(SAMPLE).unwrap(); // anthropic (env ANTHROPIC_API_KEY) + local (env [])
        let has_anthropic_key = |n: &str| n == "ANTHROPIC_API_KEY";

        // Provider list: only anthropic is env-enabled; local (no env) is filtered out.
        let providers = available_providers(&cat, &has_anthropic_key);
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].id, "anthropic");
        assert_eq!(
            providers[0].enabled,
            ProviderEnabled::Env {
                via: "env".to_string(),
                name: "ANTHROPIC_API_KEY".to_string(),
            }
        );

        // Model list: only anthropic's model (local's "tiny" excluded — provider disabled).
        let models = available_models(&cat, &has_anthropic_key);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "claude-sonnet-4-6");

        // Nothing enabled → both lists empty.
        assert!(available_providers(&cat, &|_| false).is_empty());
        assert!(available_models(&cat, &|_| false).is_empty());
    }

    #[test]
    fn available_models_sorted_newest_release_first() {
        let cat = parse_catalog(
            r#"{ "p": { "id": "p", "name": "P", "env": ["KEY"], "models": {
              "old": { "id": "old", "name": "Old", "release_date": "2020-01-01",
                       "limit": { "context": 1, "output": 1 } },
              "new": { "id": "new", "name": "New", "release_date": "2026-01-01",
                       "limit": { "context": 1, "output": 1 } }
            } } }"#,
        )
        .unwrap();
        let models = available_models(&cat, &|_| true);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "new"); // newest first
        assert_eq!(models[1].id, "old");
    }

    #[test]
    fn variants_empty_without_experimental_modes() {
        let cat = parse_catalog(
            r#"{ "p": { "id":"p","name":"P","models": {
              "m": { "id":"m","name":"M","limit":{"context":1,"output":1} } } } }"#,
        )
        .unwrap();
        let m = model_info("p", None, &cat["p"].models["m"]);
        assert!(m.variants.is_empty());
    }

    #[test]
    fn variants_partition_aisdk_options() {
        let cat = parse_catalog(
            r#"{ "openai": { "id":"openai","name":"OpenAI","npm":"@ai-sdk/openai","models": {
              "gpt": { "id":"gpt","name":"GPT","limit":{"context":1,"output":1},
                "experimental": { "modes": { "reasoning": { "provider": {
                  "body": { "maxOutputTokens": 1000, "temperature": 0.5, "stop": ["END"],
                            "reasoningEffort": "high", "service_tier": "flex", "customKey": "v" },
                  "headers": { "X-Foo": "bar" } } } } } }
            } } }"#,
        )
        .unwrap();
        let m = model_info(
            "openai",
            cat["openai"].npm.as_deref(),
            &cat["openai"].models["gpt"],
        );
        assert_eq!(m.variants.len(), 1);
        let v = &m.variants[0];
        assert_eq!(v.id, "reasoning");
        assert_eq!(v.headers.get("X-Foo").map(String::as_str), Some("bar"));
        // generation knobs: maxOutputTokens → maxTokens, temperature, stop (string array).
        let gen = v.generation.as_ref().unwrap();
        assert_eq!(gen.max_tokens, Some(EffectNumber::Finite(1000.0)));
        assert_eq!(gen.temperature, Some(EffectNumber::Finite(0.5)));
        assert_eq!(gen.stop, Some(vec!["END".to_string()]));
        // openai profile → options (service_tier canonicalizes to serviceTier); unknown → body.
        let options = v.options.as_ref().unwrap();
        assert_eq!(options["reasoningEffort"], "high");
        assert_eq!(options["serviceTier"], "flex");
        assert!(options.get("customKey").is_none());
        assert_eq!(v.body["customKey"], "v");
    }

    #[test]
    fn variants_use_provider_npm_fallback_for_semantics() {
        // The model has no own npm; the provider's npm (@ai-sdk/anthropic) drives option semantics.
        let cat = parse_catalog(
            r#"{ "anthropic": { "id":"anthropic","name":"Anthropic","npm":"@ai-sdk/anthropic","models": {
              "claude": { "id":"claude","name":"Claude","limit":{"context":1,"output":1},
                "experimental": { "modes": { "thinking": { "provider": {
                  "body": { "thinking": { "type": "enabled" }, "other": 1 } } } } } }
            } } }"#,
        )
        .unwrap();
        let m = model_info(
            "anthropic",
            cat["anthropic"].npm.as_deref(),
            &cat["anthropic"].models["claude"],
        );
        let v = &m.variants[0];
        let options = v.options.as_ref().unwrap();
        assert_eq!(options["thinking"]["type"], "enabled"); // anthropic profile → options
        assert_eq!(v.body["other"], 1); // unrecognized key → passthrough body
    }
}
