//! Real, config/credential-driven [`LlmEngine`](crate::session::LlmEngine)s (Phase 4) — the runner's
//! first real *provider* collaborator.
//!
//! [`NativeToolBox`](crate::native_tools::NativeToolBox) gave the runner real tools; this gives it a
//! real model. Where the runner's tests script the engine, production must pick a provider from config
//! (the `provider/model` string), resolve its credentials from the environment, and send each turn
//! over the real [`transport`](opencode_llm::transport). The pieces:
//!
//! - [`Provider`] — the provider families. Only [`Anthropic`](Provider::Anthropic) has an engine
//!   today; the other ported protocols (openai-chat, gemini, bedrock-converse, openai-responses)
//!   slot in as new variants + [`ProviderRegistry`] arms **without touching the runner** (which only
//!   ever sees `&dyn LlmEngine`).
//! - [`EngineSettings`] — a resolved, validated `(provider, model id, api key, endpoint)`, produced
//!   by [`EngineSettings::resolve`] from the config string + a [`Credentials`] source. All failures
//!   surface *before* any network call (no model / not `provider/model` / unknown provider / missing
//!   key).
//! - [`ProviderRegistry`] — the extension seam: `engine(&settings) -> Arc<dyn LlmEngine>`. The
//!   [`DefaultRegistry`] reuses one HTTP client across the engines it builds.
//! - [`AnthropicEngine`] — the concrete `anthropic-messages` engine, wrapping
//!   [`opencode_llm::transport::complete`] with the resolved key/endpoint.
//!
//! Not wired into the server yet: the `AppContext`-constructed runner on a gated session-execution
//! path is the next increment. Here the engine is proven end-to-end (registry → engine → runner) over
//! an in-test transport.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use opencode_llm::anthropic::AnthropicMessages;
use opencode_llm::transport;
use opencode_llm::{LlmError, LlmEvent, LlmRequest};

use crate::session::LlmEngine;

/// The dated `anthropic-version` header every Messages request carries.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Request timeout for the real HTTPS client. The transport collects the whole SSE response before
/// decoding, so this bounds an entire generation (not just connect) — generous for long completions.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Why building an engine from configuration failed — every variant is raised **before** any provider
/// call is attempted, so a misconfiguration never reaches the network.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// No model was configured (the `model` key is absent or empty).
    #[error("no model configured")]
    NoModel,
    /// The model id is not in `provider/model` form (e.g. it is missing the `/`).
    #[error("invalid model {0:?}: expected `provider/model`")]
    InvalidModel(String),
    /// The provider prefix names a provider that has no engine here yet.
    #[error("unknown provider {0:?}")]
    UnknownProvider(String),
    /// The selected provider has no API key (its environment variable is unset or empty).
    #[error("missing API key for provider {provider}: set {env}")]
    MissingApiKey {
        /// The selected provider's id.
        provider: &'static str,
        /// The environment variable that must hold the key.
        env: &'static str,
    },
    /// The underlying HTTP client could not be built.
    #[error(transparent)]
    Client(#[from] LlmError),
}

/// A provider family. Only [`Anthropic`](Provider::Anthropic) is wired to an engine today; the other
/// ported protocols become new variants (and one [`ProviderRegistry`] arm each) as follow-ups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// Anthropic Messages (`anthropic-messages`).
    Anthropic,
}

impl Provider {
    /// The provider's id — the segment before `/` in a `provider/model` string.
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Anthropic => "anthropic",
        }
    }

    /// The environment variable that holds this provider's API key.
    pub fn api_key_env(self) -> &'static str {
        match self {
            Provider::Anthropic => "ANTHROPIC_API_KEY",
        }
    }

    /// The provider's default API endpoint (overridable in [`EngineSettings::resolve`]).
    pub fn default_endpoint(self) -> &'static str {
        match self {
            Provider::Anthropic => "https://api.anthropic.com/v1/messages",
        }
    }

    /// Parse a provider id (the segment before `/`).
    pub fn parse(name: &str) -> Result<Self, EngineError> {
        match name {
            "anthropic" => Ok(Provider::Anthropic),
            other => Err(EngineError::UnknownProvider(other.to_string())),
        }
    }
}

/// Split a `provider/model` id into its [`Provider`] and the bare, provider-native model id (what the
/// request carries). Only the first `/` is the provider boundary, so model ids may themselves contain
/// `/`.
pub fn split_model(model: &str) -> Result<(Provider, &str), EngineError> {
    if model.is_empty() {
        return Err(EngineError::NoModel);
    }
    let (provider, id) = model
        .split_once('/')
        .ok_or_else(|| EngineError::InvalidModel(model.to_string()))?;
    if id.is_empty() {
        return Err(EngineError::InvalidModel(model.to_string()));
    }
    Provider::parse(provider).map(|provider| (provider, id))
}

/// A source of provider credentials — the seam between [`EngineSettings::resolve`] and the process
/// environment, so tests inject keys without mutating global env state.
pub trait Credentials {
    /// The value of credential `name` (an env-var name like `ANTHROPIC_API_KEY`), if set.
    fn get(&self, name: &str) -> Option<String>;

    /// The API key for a provider, looked up by provider id (e.g. `"anthropic"`) and/or its env-var
    /// name. The default reads only the env var; richer sources (e.g. [`OpencodeCredentials`], which
    /// reads `opencode auth login`'s `auth.json` keyed by provider id) override this.
    fn api_key(&self, _provider_id: &str, env: &str) -> Option<String> {
        self.get(env)
    }
}

/// Reads credentials from the process environment ([`std::env::var`]).
pub struct EnvCredentials;

impl Credentials for EnvCredentials {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }
}

/// The opencode data directory (`$XDG_DATA_HOME/opencode`, else `~/.local/share/opencode`), mirroring
/// `packages/core/src/global.ts`.
fn opencode_data_dir() -> std::path::PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return std::path::PathBuf::from(xdg).join("opencode");
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    std::path::PathBuf::from(home).join(".local/share/opencode")
}

/// Reads opencode's `auth.json` — what `opencode auth login` writes — so the backend finds API keys
/// stored by the CLI/GUI, falling back to the process environment. The file is a
/// `{ [providerID]: { type, key?, … } }` map (mirroring the TS `Auth` service); `OPENCODE_AUTH_CONTENT`
/// (inline JSON) takes precedence over the file. Only `key`-bearing entries (`type: "api"`/`"wellknown"`)
/// yield a static key; `oauth` entries (which need token refresh) are skipped, leaving the env fallback.
pub struct OpencodeCredentials {
    /// providerID → API key, extracted from `auth.json` / `OPENCODE_AUTH_CONTENT`.
    keys: std::collections::HashMap<String, String>,
}

impl OpencodeCredentials {
    /// Load the stored auth map. A missing or unparseable source yields an empty map (the env-var
    /// fallback still applies), so this never fails.
    pub fn load() -> Self {
        let raw = std::env::var("OPENCODE_AUTH_CONTENT")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(|| std::fs::read_to_string(opencode_data_dir().join("auth.json")).ok());
        Self {
            keys: raw.as_deref().map(parse_auth_keys).unwrap_or_default(),
        }
    }
}

/// Extract the `{ providerID: key }` map from an `auth.json` body: each `key`-bearing entry
/// (`type: "api"`/`"wellknown"`) yields a static key; `oauth` entries (no static key) are skipped.
/// Unparseable input yields an empty map.
fn parse_auth_keys(raw: &str) -> std::collections::HashMap<String, String> {
    serde_json::from_str::<serde_json::Value>(raw)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .map(|obj| {
            obj.into_iter()
                .filter_map(|(provider, info)| {
                    info.get("key")
                        .and_then(|k| k.as_str())
                        .filter(|k| !k.is_empty())
                        .map(|k| (provider, k.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Credentials for OpencodeCredentials {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn api_key(&self, provider_id: &str, env: &str) -> Option<String> {
        self.keys
            .get(provider_id)
            .cloned()
            .or_else(|| std::env::var(env).ok())
    }
}

/// A resolved, validated engine configuration: the [`Provider`], the bare model id, the API key, and
/// the endpoint. Produced by [`resolve`](EngineSettings::resolve); the [`provider`](Self::provider) /
/// [`api_key`](Self::api_key) / [`endpoint`](Self::endpoint) drive a [`ProviderRegistry`], and
/// [`model`](Self::model) is what the caller puts in its [`Session`](crate::session::Session).
#[derive(Debug, Clone)]
pub struct EngineSettings {
    /// The selected provider.
    pub provider: Provider,
    /// The bare, provider-native model id (the part after `provider/`) — sent on every turn.
    pub model: String,
    /// The provider API key.
    pub api_key: String,
    /// The provider endpoint (a resolved override, else [`Provider::default_endpoint`]).
    pub endpoint: String,
}

impl EngineSettings {
    /// Resolve settings from the config `model` string (`provider/model`) and a [`Credentials`]
    /// source, validating everything **before** any network call: the model must be present and in
    /// `provider/model` form, the provider must be known, and its API key must be set and non-empty.
    /// `endpoint` overrides the provider default when `Some` (e.g. a config `baseURL`); `None` uses
    /// [`Provider::default_endpoint`].
    pub fn resolve(
        model: &str,
        endpoint: Option<String>,
        creds: &dyn Credentials,
    ) -> Result<Self, EngineError> {
        let (provider, model_id) = split_model(model)?;
        let env = provider.api_key_env();
        let api_key = creds
            .api_key(provider.as_str(), env)
            .filter(|key| !key.is_empty())
            .ok_or(EngineError::MissingApiKey {
                provider: provider.as_str(),
                env,
            })?;
        Ok(Self {
            provider,
            model: model_id.to_string(),
            api_key,
            endpoint: endpoint.unwrap_or_else(|| provider.default_endpoint().to_string()),
        })
    }
}

/// Builds an [`LlmEngine`] for resolved [`EngineSettings`] — the seam where new providers are added
/// without touching the runner (which only ever holds a `&dyn LlmEngine`).
pub trait ProviderRegistry: Send + Sync {
    /// Build the engine for `settings`.
    fn engine(&self, settings: &EngineSettings) -> Result<Arc<dyn LlmEngine>, EngineError>;
}

/// The default registry — one shared [`reqwest::Client`] reused across the engines it builds (so the
/// connection pool is shared). Only [`Provider::Anthropic`] is implemented; each other provider is a
/// follow-up match arm.
pub struct DefaultRegistry {
    client: reqwest::Client,
}

impl DefaultRegistry {
    /// A registry whose engines call real providers over an HTTPS-only client with the default
    /// timeout.
    pub fn new() -> Result<Self, EngineError> {
        Ok(Self {
            client: transport::https_client(DEFAULT_TIMEOUT)?,
        })
    }

    /// A registry over a caller-supplied client — tests point this at a local HTTP server (the real
    /// HTTPS-only client rejects `http://`).
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }
}

impl ProviderRegistry for DefaultRegistry {
    fn engine(&self, settings: &EngineSettings) -> Result<Arc<dyn LlmEngine>, EngineError> {
        match settings.provider {
            Provider::Anthropic => Ok(Arc::new(AnthropicEngine::new(
                self.client.clone(),
                settings.endpoint.clone(),
                settings.api_key.clone(),
            ))),
        }
    }
}

/// The concrete `anthropic-messages` [`LlmEngine`]: each turn lowers the request and POSTs it via
/// [`transport::complete`] with the resolved key (`x-api-key`) and the [`ANTHROPIC_VERSION`] header.
pub struct AnthropicEngine {
    client: reqwest::Client,
    endpoint: String,
    api_key: String,
}

impl AnthropicEngine {
    /// An engine over `client`, posting to `endpoint` authenticated with `api_key`.
    pub fn new(client: reqwest::Client, endpoint: String, api_key: String) -> Self {
        Self {
            client,
            endpoint,
            api_key,
        }
    }
}

#[async_trait]
impl LlmEngine for AnthropicEngine {
    async fn complete(&self, request: &LlmRequest) -> Result<Vec<LlmEvent>, LlmError> {
        transport::complete(
            &self.client,
            &self.endpoint,
            &[
                ("x-api-key", self.api_key.as_str()),
                ("anthropic-version", ANTHROPIC_VERSION),
            ],
            &AnthropicMessages,
            request,
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::SessionOutcome;
    use crate::session::{run, Session, ToolBox};
    use opencode_llm::{ContentPart, Message, ToolDefinition};
    use serde_json::{json, Value};
    use std::collections::HashMap;

    /// An in-memory [`Credentials`] source for tests (no process env mutation).
    struct MapCreds(HashMap<&'static str, &'static str>);

    impl Credentials for MapCreds {
        fn get(&self, name: &str) -> Option<String> {
            self.0.get(name).map(|value| value.to_string())
        }
    }

    fn creds(pairs: &[(&'static str, &'static str)]) -> MapCreds {
        MapCreds(pairs.iter().copied().collect())
    }

    fn with_key() -> MapCreds {
        creds(&[("ANTHROPIC_API_KEY", "sk-ant-test")])
    }

    // ---- auth.json credential source ----

    #[test]
    fn parse_auth_keys_extracts_api_keys_and_skips_oauth() {
        let raw = json!({
            "anthropic": { "type": "api", "key": "sk-ant-stored" },
            "openai": { "type": "wellknown", "key": "sk-wk", "token": "t" },
            "google": { "type": "oauth", "refresh": "r", "access": "a", "expires": 1 },
            "empty": { "type": "api", "key": "" }
        })
        .to_string();
        let keys = parse_auth_keys(&raw);
        assert_eq!(
            keys.get("anthropic").map(String::as_str),
            Some("sk-ant-stored")
        );
        assert_eq!(keys.get("openai").map(String::as_str), Some("sk-wk"));
        // OAuth (no static key) and empty keys are skipped.
        assert!(!keys.contains_key("google"));
        assert!(!keys.contains_key("empty"));
    }

    #[test]
    fn parse_auth_keys_tolerates_garbage() {
        assert!(parse_auth_keys("not json").is_empty());
        assert!(parse_auth_keys("[]").is_empty());
    }

    #[test]
    fn opencode_credentials_prefers_stored_key_then_env_fallback() {
        let creds = OpencodeCredentials {
            keys: [("anthropic".to_string(), "sk-ant-stored".to_string())]
                .into_iter()
                .collect(),
        };
        // Stored key wins (independent of the env var).
        assert_eq!(
            creds.api_key("anthropic", "ANTHROPIC_API_KEY").as_deref(),
            Some("sk-ant-stored")
        );
        // A provider with no stored key falls back to env — use an env var that is certainly unset, so
        // the assertion doesn't depend on the test host's environment.
        assert_eq!(
            creds.api_key("nope", "OPENCODE_UNSET_KEY_FOR_TEST_XZ"),
            None
        );
    }

    // ---- Settings resolution (no network) ----

    #[test]
    fn resolve_parses_provider_model_and_defaults_the_endpoint() {
        let settings =
            EngineSettings::resolve("anthropic/claude-haiku-4-5-20251001", None, &with_key())
                .unwrap();
        assert_eq!(settings.provider, Provider::Anthropic);
        assert_eq!(settings.model, "claude-haiku-4-5-20251001");
        assert_eq!(settings.api_key, "sk-ant-test");
        assert_eq!(settings.endpoint, "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn resolve_honors_an_endpoint_override() {
        let settings = EngineSettings::resolve(
            "anthropic/claude-x",
            Some("http://localhost:1234/v1/messages".to_string()),
            &with_key(),
        )
        .unwrap();
        assert_eq!(settings.endpoint, "http://localhost:1234/v1/messages");
    }

    #[test]
    fn resolve_rejects_an_empty_model() {
        assert!(matches!(
            EngineSettings::resolve("", None, &with_key()).unwrap_err(),
            EngineError::NoModel
        ));
    }

    #[test]
    fn resolve_requires_provider_model_form() {
        // No `/` separator, and a trailing-slash (empty model id) are both invalid.
        assert!(matches!(
            EngineSettings::resolve("claude-haiku", None, &with_key()).unwrap_err(),
            EngineError::InvalidModel(_)
        ));
        assert!(matches!(
            EngineSettings::resolve("anthropic/", None, &with_key()).unwrap_err(),
            EngineError::InvalidModel(_)
        ));
    }

    #[test]
    fn resolve_rejects_an_unknown_provider() {
        match EngineSettings::resolve("bogus/model", None, &with_key()).unwrap_err() {
            EngineError::UnknownProvider(name) => assert_eq!(name, "bogus"),
            other => panic!("expected UnknownProvider, got {other:?}"),
        }
    }

    #[test]
    fn resolve_reports_a_missing_key_without_calling_out() {
        match EngineSettings::resolve("anthropic/claude-x", None, &creds(&[])).unwrap_err() {
            EngineError::MissingApiKey { provider, env } => {
                assert_eq!(provider, "anthropic");
                assert_eq!(env, "ANTHROPIC_API_KEY");
            }
            other => panic!("expected MissingApiKey, got {other:?}"),
        }
    }

    #[test]
    fn resolve_treats_an_empty_key_as_missing() {
        assert!(matches!(
            EngineSettings::resolve(
                "anthropic/claude-x",
                None,
                &creds(&[("ANTHROPIC_API_KEY", "")])
            )
            .unwrap_err(),
            EngineError::MissingApiKey { .. }
        ));
    }

    #[test]
    fn registry_builds_an_engine_for_resolved_settings() {
        let registry = DefaultRegistry::with_client(reqwest::Client::new());
        let settings = EngineSettings::resolve("anthropic/claude-x", None, &with_key()).unwrap();
        assert!(registry.engine(&settings).is_ok());
    }

    // ---- End-to-end over the real HTTP transport (in-test axum server, no network/TLS) ----
    //
    // The SSE fixtures mirror the runner's transport test: a `get_weather({"city":"Paris"})` tool call,
    // then a text answer.

    const TOOL_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"Paris\\\"}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    const TEXT_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"It is sunny in Paris.\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":20,\"output_tokens\":7}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    /// A toolbox with a single `get_weather` tool; any other name is a tool-level error.
    struct WeatherTools;

    #[async_trait]
    impl ToolBox for WeatherTools {
        async fn invoke(&self, name: &str, input: Value) -> Result<String, String> {
            match name {
                "get_weather" => {
                    let city = input.get("city").and_then(Value::as_str).unwrap_or("?");
                    Ok(
                        json!({ "city": city, "temperature": 22, "condition": "sunny" })
                            .to_string(),
                    )
                }
                other => Err(format!("unknown tool: {other}")),
            }
        }
    }

    /// Spawn a local axum server returning the given SSE bodies in order (last body repeats).
    async fn spawn_sse(bodies: &'static [&'static str]) -> String {
        use axum::{routing::post, Router};
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let app = Router::new().route(
            "/v1/messages",
            post(move || {
                let calls = calls.clone();
                async move {
                    let i = calls.fetch_add(1, Ordering::SeqCst).min(bodies.len() - 1);
                    ([("content-type", "text/event-stream")], bodies[i])
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/v1/messages")
    }

    #[tokio::test]
    async fn anthropic_engine_completes_a_turn_over_transport() {
        let url = spawn_sse(&[TEXT_SSE]).await;
        let registry = DefaultRegistry::with_client(reqwest::Client::new());
        let settings = EngineSettings::resolve(
            "anthropic/claude-haiku-4-5-20251001",
            Some(url),
            &with_key(),
        )
        .unwrap();
        let engine = registry.engine(&settings).unwrap();

        let request = LlmRequest {
            model: settings.model.clone(),
            messages: vec![Message::user_text("weather in Paris?")],
            ..Default::default()
        };
        let events = engine.complete(&request).await.unwrap();

        let text: String = events
            .iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(text, "It is sunny in Paris.");
        assert!(events.iter().any(|e| matches!(e, LlmEvent::Finish { .. })));
    }

    #[tokio::test]
    async fn registry_engine_drives_a_session_tool_loop() {
        // Turn 1 streams a tool call; turn 2 answers in text.
        let url = spawn_sse(&[TOOL_SSE, TEXT_SSE]).await;
        let registry = DefaultRegistry::with_client(reqwest::Client::new());
        let settings = EngineSettings::resolve(
            "anthropic/claude-haiku-4-5-20251001",
            Some(url),
            &with_key(),
        )
        .unwrap();
        let engine = registry.engine(&settings).unwrap();

        let mut session = Session::new(settings.model.clone(), 8);
        session.tools = vec![ToolDefinition {
            name: "get_weather".into(),
            description: Some("Get current weather.".into()),
            input_schema: json!({ "type": "object" }),
        }];

        let run = run(
            engine.as_ref(),
            Arc::new(WeatherTools),
            &session,
            vec![Message::user_text("What's the weather in Paris?")],
        )
        .await
        .unwrap();

        assert_eq!(run.outcome, SessionOutcome::Completed { steps: 2 });
        // The decoded tool call drove a real tool execution, fed back into the second turn.
        match &run.messages[2].content[0] {
            ContentPart::ToolResult { id, result, .. } => {
                assert_eq!(id, "toolu_1");
                assert!(result.as_str().unwrap().contains("Paris"));
            }
            other => panic!("expected tool result, got {other:?}"),
        }
        let answer: String = run
            .transcript
            .iter()
            .filter_map(|e| match e {
                LlmEvent::TextDelta { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert!(answer.contains("sunny"));
        // Usage summed across both real turns (10/5 + 20/7).
        assert_eq!(run.usage.input, 30);
        assert_eq!(run.usage.output, 12);
    }
}
