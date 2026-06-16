//! axum HTTP API + the **strangler-fig seam**.
//!
//! Native contract routes are added to the router as they are cut over, gated by a [`RouteTable`]
//! parsed from `OPENCODE_RUST_ROUTES`. Everything not handled natively falls through to
//! [`proxy::proxy_handler`], which forwards the request to the existing TypeScript server. With an
//! empty route table the server proxies 100% of contract traffic — proving the seam end-to-end with
//! zero native handlers. An always-native `/_rust/health` liveness route supports the Phase 0 smoke.

pub mod proxy;

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::Json,
    routing::{get, post},
    Router,
};
use opencode_core::native_tools::{self, NativeToolBox};
use opencode_core::provider::{
    split_model, DefaultRegistry, EngineError, EngineSettings, EnvCredentials, ProviderRegistry,
};
use opencode_core::runner::SessionOutcome;
use opencode_core::session::{
    run_gated, AllowAll, BusSink, EventStoreSink, FanOutSink, LlmEngine, PermissionGate, Session,
    ToolBox,
};
use opencode_effect::AppContext;
use opencode_llm::{ContentPart, Generation, Message, Role};
use opencode_proto::Health;

/// Server version, taken from this crate's Cargo version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Which contract route groups are served natively by Rust; the rest are proxied to TypeScript.
/// Parsed from `OPENCODE_RUST_ROUTES` (comma-separated group names, e.g. `health,fs,location`).
#[derive(Clone, Debug, Default)]
pub struct RouteTable {
    groups: BTreeSet<String>,
}

impl RouteTable {
    /// Build from the `OPENCODE_RUST_ROUTES` environment variable.
    pub fn from_env() -> Self {
        Self::parse(&std::env::var("OPENCODE_RUST_ROUTES").unwrap_or_default())
    }

    /// Parse a comma-separated list of group names.
    pub fn parse(raw: &str) -> Self {
        let groups = raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
        Self { groups }
    }

    /// Whether the named route group is served natively.
    pub fn handles(&self, group: &str) -> bool {
        self.groups.contains(group)
    }

    /// Number of natively-served groups.
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Whether no group is served natively (empty table → proxy everything).
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

/// Shared axum state.
#[derive(Clone)]
pub struct ServerState {
    /// Application/DI context (event store, projections, bus).
    pub ctx: AppContext,
    /// Native-vs-proxy routing decisions.
    pub routes: RouteTable,
    /// Upstream TypeScript server used for proxied routes.
    pub proxy: Arc<proxy::Upstream>,
    /// Session-runner collaborators (engine factory, permission gate, tools root).
    pub runner: RunnerServices,
}

/// Builds the per-request [`LlmEngine`] from a `provider/model` id — the execute route's injection
/// seam. Production resolves environment credentials through the provider registry; tests return a
/// fixed engine pointed at a local server.
pub(crate) trait EngineFactory: Send + Sync {
    fn build(&self, model: &str) -> Result<Arc<dyn LlmEngine>, EngineError>;
}

/// Production [`EngineFactory`]: resolve settings from environment credentials, then build via the
/// provider registry.
pub(crate) struct EnvEngineFactory {
    registry: Arc<dyn ProviderRegistry>,
    endpoint: Option<String>,
}

impl EngineFactory for EnvEngineFactory {
    fn build(&self, model: &str) -> Result<Arc<dyn LlmEngine>, EngineError> {
        let settings = EngineSettings::resolve(model, self.endpoint.clone(), &EnvCredentials)?;
        self.registry.engine(&settings)
    }
}

/// The session runner's collaborators, constructed at the composition root and threaded through
/// [`ServerState`]. These live here (not in `opencode-effect`'s `AppServices`) because the runner's
/// traits are defined in `opencode-core`, which already depends on `opencode-effect` — housing them in
/// `AppServices` would form a dependency cycle.
#[derive(Clone)]
pub struct RunnerServices {
    /// Builds the per-turn engine from the request's `provider/model`.
    pub(crate) engines: Arc<dyn EngineFactory>,
    /// Permission gate consulted before each tool call.
    pub(crate) gate: Arc<dyn PermissionGate>,
    /// Working directory the native tools resolve relative paths against.
    pub(crate) root: std::path::PathBuf,
}

impl RunnerServices {
    /// Production wiring: a real HTTPS provider registry with environment credentials, an allow-all
    /// gate (a DB-backed gate is a later increment), and `root` as the native tools' working directory.
    pub fn from_env(root: impl Into<std::path::PathBuf>) -> Result<Self, EngineError> {
        Ok(Self {
            engines: Arc::new(EnvEngineFactory {
                registry: Arc::new(DefaultRegistry::new()?),
                endpoint: None,
            }),
            gate: Arc::new(AllowAll),
            root: root.into(),
        })
    }
}

impl Default for RunnerServices {
    fn default() -> Self {
        Self {
            engines: Arc::new(EnvEngineFactory {
                registry: Arc::new(DefaultRegistry::with_client(reqwest::Client::new())),
                endpoint: None,
            }),
            gate: Arc::new(AllowAll),
            root: std::path::PathBuf::from("."),
        }
    }
}

/// Always-native internal liveness/readiness route (not part of the public OpenAPI contract).
async fn rust_health() -> Json<Health> {
    Json(Health {
        ok: true,
        backend: "rust".to_string(),
        version: VERSION.to_string(),
    })
}

/// Always-native internal SSE stream of the in-process event bus (`/_rust/event`). This is **not**
/// the contract `/event` (which stays proxied to TS): during coexistence the Rust bus has no
/// producers, so this is infrastructure proving the SSE + channel plumbing end-to-end (and the Phase-4
/// runner's outlet). Each [`opencode_effect::BusEvent`] becomes one SSE message (`event:` = its type).
async fn rust_event(
    State(state): State<ServerState>,
) -> axum::response::Sse<
    impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use futures::StreamExt;
    let stream = state.ctx.event_bus().subscribe().map(|event| {
        let data = serde_json::to_string(&event).unwrap_or_else(|_| "{}".to_string());
        Ok(axum::response::sse::Event::default()
            .event(event.kind)
            .data(data))
    });
    axum::response::Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
}

/// Request body for the internal session-execute proving route.
#[derive(serde::Deserialize)]
struct ExecutePayload {
    /// `provider/model` id (e.g. `anthropic/claude-...`).
    model: String,
    /// The user's prompt for this single turn.
    prompt: String,
    /// Optional system-prompt parts.
    #[serde(default)]
    system: Vec<String>,
    /// Max turns before stopping (the continuation step limit).
    #[serde(default = "default_step_limit")]
    step_limit: usize,
}

fn default_step_limit() -> usize {
    16
}

/// `POST /_rust/session/{sessionID}/execute` — internal, gated (`session-exec`) proving route for the
/// Phase-4 runner. Builds the `AppContext`-wired runner (engine from the provider registry, native
/// tools, permission gate) and drives **one** user turn to completion, persisting each turn's events to
/// the store and announcing them on the bus (observable live on `/_rust/event`). This is **not** the
/// contract `/api/session/{id}/prompt` cutover (which durably admits the input + schedules a background
/// agent loop); it proves the runner end-to-end inside the server.
async fn rust_session_execute(
    State(state): State<ServerState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    Json(payload): Json<ExecutePayload>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;

    // The engine is built from the full `provider/model`; the turn request carries the bare model id.
    let (_, model_id) =
        split_model(&payload.model).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let model_id = model_id.to_string();

    let engine = state
        .runner
        .engines
        .build(&payload.model)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;
    let tools: Arc<dyn ToolBox> = Arc::new(NativeToolBox::new(state.runner.root.clone()));

    let session = Session {
        id: session_id.clone(),
        model: model_id,
        system: payload.system,
        tools: native_tools::tool_definitions(),
        generation: Generation::default(),
        step_limit: payload.step_limit,
    };

    // Persist (authoritative) then announce (best-effort) — the runner's fan-out sink.
    let primary = EventStoreSink::new(state.ctx.event_store().clone(), session_id.as_str())
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    let secondary = BusSink::new(state.ctx.event_bus().clone(), session_id.as_str());
    let sink = FanOutSink::new(primary, secondary);

    let run = run_gated(
        engine.as_ref(),
        tools,
        &sink,
        state.runner.gate.as_ref(),
        &session,
        vec![Message::user_text(payload.prompt)],
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    let (outcome, steps) = match &run.outcome {
        SessionOutcome::Completed { steps } => ("completed", *steps),
        SessionOutcome::StepLimitReached { steps } => ("step_limit_reached", *steps),
        SessionOutcome::AwaitingPermission { steps } => ("awaiting_permission", *steps),
    };
    let text = run
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::Assistant)
        .and_then(|m| {
            m.content.iter().find_map(|part| match part {
                ContentPart::Text(t) => Some(t.clone()),
                _ => None,
            })
        });

    Ok(Json(serde_json::json!({
        "session": session_id,
        "outcome": outcome,
        "steps": steps,
        "text": text,
        "usage": { "input": run.usage.input, "output": run.usage.output },
    })))
}

/// `GET /health` — first contract route cut over natively (gated by the route table).
#[utoipa::path(
    get,
    path = "/health",
    responses((status = 200, description = "Service health", body = Health)),
    tag = "control"
)]
async fn health(State(state): State<ServerState>) -> Json<Health> {
    let _ = &state.ctx;
    Json(Health {
        ok: true,
        backend: "rust".to_string(),
        version: VERSION.to_string(),
    })
}

/// `GET /global/health` — the first real contract route served natively (gated by the `global`
/// group). Matches the golden `global.health` operation: inline 200 health body + 400 BadRequestError.
#[utoipa::path(
    get,
    path = "/global/health",
    operation_id = "global.health",
    responses(
        (status = 200, description = "Health information", body = inline(opencode_proto::GlobalHealth)),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "global"
)]
async fn global_health() -> Json<opencode_proto::GlobalHealth> {
    Json(opencode_proto::GlobalHealth {
        healthy: true,
        version: VERSION.to_string(),
    })
}

/// `GET /path` — resolve opencode paths for a directory (group `instance`). Matches the golden
/// `path.get`: `$ref Path` 200 + `BadRequestError` 400. Path values are computed from the
/// environment (XDG dirs) + the git worktree of `directory`.
#[utoipa::path(
    get,
    path = "/path",
    operation_id = "path.get",
    params(
        ("directory" = Option<String>, Query, description = "Working directory to resolve (defaults to the server cwd)"),
        ("workspace" = Option<String>, Query, description = "Workspace id")
    ),
    responses(
        (status = 200, description = "Resolved paths", body = opencode_proto::Path),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "instance"
)]
async fn path_get(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<opencode_proto::Path> {
    let directory = params.get("directory").cloned().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    });
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_default();
    let config = format!(
        "{}/opencode",
        std::env::var("XDG_CONFIG_HOME").unwrap_or_else(|_| format!("{home}/.config"))
    );
    let state = format!(
        "{}/opencode",
        std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| format!("{home}/.local/state"))
    );
    let worktree = opencode_tools::git::root(std::path::Path::new(&directory))
        .await
        .ok()
        .flatten()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| directory.clone());
    Json(opencode_proto::Path {
        home,
        state,
        config,
        worktree,
        directory,
    })
}

/// 400 responder returning the Effect HttpApi `BadRequestError` body (`{name, data}`).
#[derive(Debug)]
pub struct ApiBadRequest(pub opencode_proto::BadRequestError);

impl axum::response::IntoResponse for ApiBadRequest {
    fn into_response(self) -> axum::response::Response {
        (axum::http::StatusCode::BAD_REQUEST, Json(self.0)).into_response()
    }
}

fn bad_request(message: impl Into<String>, kind: &str) -> ApiBadRequest {
    ApiBadRequest(opencode_proto::BadRequestError {
        name: "BadRequest".to_string(),
        data: opencode_proto::BadRequestData {
            message: message.into(),
            kind: Some(kind.to_string()),
        },
    })
}

/// `GET /find/file` — fuzzy file search (group `file`). Matches the golden `find.files`:
/// 200 `array<string>` + 400 `BadRequestError`. Reuses `opencode_tools::find_files`.
#[utoipa::path(
    get,
    path = "/find/file",
    operation_id = "find.files",
    params(
        ("directory" = Option<String>, Query, description = "Directory to search (defaults to cwd)"),
        ("workspace" = Option<String>, Query, description = "Workspace id"),
        ("query" = String, Query, description = "Search query"),
        ("dirs" = Option<bool>, Query, description = "Include directories"),
        ("type" = Option<String>, Query, description = "Filter by type"),
        ("limit" = Option<i64>, Query, description = "Max results")
    ),
    responses(
        (status = 200, description = "File paths", body = Vec<String>),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "file"
)]
async fn find_files(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Vec<String>>, ApiBadRequest> {
    let query = params
        .get("query")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad_request("missing required query parameter: query", "Query"))?;
    let directory = params.get("directory").cloned().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    });
    let limit = params
        .get("limit")
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100);
    let files = opencode_tools::find_files(std::path::Path::new(&directory), query, limit);
    Ok(Json(files))
}

/// `GET /find` — regex text search (group `file`). Matches the golden `find.text`: 200 array of
/// ripgrep-style match objects + 400 `BadRequestError`. Reuses `opencode_tools::grep_detailed`.
#[utoipa::path(
    get,
    path = "/find",
    operation_id = "find.text",
    params(
        ("directory" = Option<String>, Query, description = "Directory to search (defaults to cwd)"),
        ("workspace" = Option<String>, Query, description = "Workspace id"),
        ("pattern" = String, Query, description = "Regex pattern")
    ),
    responses(
        (status = 200, description = "Matches", body = Vec<opencode_proto::TextSearchMatch>),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "file"
)]
async fn find_text(
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Vec<opencode_proto::TextSearchMatch>>, ApiBadRequest> {
    let pattern = params
        .get("pattern")
        .filter(|s| !s.is_empty())
        .ok_or_else(|| bad_request("missing required query parameter: pattern", "Query"))?;
    let directory = params.get("directory").cloned().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    });
    let matches = opencode_tools::grep_detailed(pattern, std::path::Path::new(&directory))
        .map_err(|e| bad_request(e.to_string(), "Query"))?;
    let items = matches
        .into_iter()
        .map(|m| opencode_proto::TextSearchMatch {
            path: opencode_proto::TextWrap { text: m.path },
            lines: opencode_proto::TextWrap { text: m.line_text },
            line_number: m.line_number,
            absolute_offset: m.absolute_offset,
            submatches: m
                .submatches
                .into_iter()
                .map(|s| opencode_proto::TextSubmatch {
                    r#match: opencode_proto::TextWrap { text: s.text },
                    start: s.start as u64,
                    end: s.end as u64,
                })
                .collect(),
        })
        .collect();
    Ok(Json(items))
}

/// `POST /log` — write a client log entry (group `control`). Matches the golden `app.log`: 200
/// `boolean` + 400 `BadRequestError`. The body is parsed into `LogEntry` and emitted via `tracing`;
/// any parse error returns a contract-shaped `BadRequestError`. (openapi-diff gates responses only,
/// so the request body schema is not yet enforced.)
#[utoipa::path(
    post,
    path = "/log",
    operation_id = "app.log",
    responses(
        (status = 200, description = "Log entry written", body = bool, content_type = "application/json"),
        (status = 400, description = "Bad request", body = opencode_proto::RequestError)
    ),
    tag = "control"
)]
async fn app_log(Json(entry): Json<opencode_proto::LogEntry>) -> Json<bool> {
    let opencode_proto::LogEntry {
        service,
        level,
        message,
    } = entry;
    match level.as_str() {
        "error" => tracing::error!(target: "opencode.client", service, "{message}"),
        "warn" => tracing::warn!(target: "opencode.client", service, "{message}"),
        "debug" => tracing::debug!(target: "opencode.client", service, "{message}"),
        _ => tracing::info!(target: "opencode.client", service, "{message}"),
    }
    Json(true)
}

/// Map a `session` projection row to the `SessionV2Info` wire shape (mirrors TS `session/info.ts`
/// `fromRow`): integer DB columns widen to the contract's `number`, and the `model` JSON column is
/// projected to `ModelRef` (variant defaults to `"default"`, as in TS).
fn session_record_to_info(r: opencode_db::SessionRecord) -> opencode_proto::SessionV2Info {
    let model = r.model.as_ref().map(|m| opencode_proto::ModelRef {
        id: m
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        provider_id: m
            .get("providerID")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        variant: Some(
            m.get("variant")
                .and_then(|v| v.as_str())
                .unwrap_or("default")
                .to_string(),
        ),
    });
    opencode_proto::SessionV2Info {
        id: r.id,
        parent_id: r.parent_id,
        project_id: r.project_id,
        agent: r.agent,
        model,
        cost: r.cost,
        tokens: opencode_proto::SessionTokens {
            input: r.tokens_input as f64,
            output: r.tokens_output as f64,
            reasoning: r.tokens_reasoning as f64,
            cache: opencode_proto::TokenCache {
                read: r.tokens_cache_read as f64,
                write: r.tokens_cache_write as f64,
            },
        },
        time: opencode_proto::SessionTime {
            created: r.time_created as f64,
            updated: r.time_updated as f64,
            archived: r.time_archived.map(|t| t as f64),
        },
        title: r.title,
        location: opencode_proto::LocationRef {
            directory: r.directory,
            workspace_id: r.workspace_id,
        },
        subpath: r.path,
    }
}

/// Error responder for `v2.session.get`: a contract-shaped 404 `SessionNotFoundError`, or a generic
/// 500 envelope if the store read fails.
pub enum SessionGetError {
    /// No session with that id (404).
    NotFound(String),
    /// Store read failed (500).
    Internal(String),
}

impl axum::response::IntoResponse for SessionGetError {
    fn into_response(self) -> axum::response::Response {
        match self {
            SessionGetError::NotFound(id) => (
                axum::http::StatusCode::NOT_FOUND,
                Json(opencode_proto::SessionNotFoundError {
                    tag: "SessionNotFoundError".to_string(),
                    message: format!("Session {id} not found"),
                    session_id: id,
                }),
            )
                .into_response(),
            SessionGetError::Internal(message) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(opencode_proto::ErrorEnvelope {
                    tag: "InternalError".to_string(),
                    message,
                }),
            )
                .into_response(),
        }
    }
}

/// `GET /api/session/{sessionID}` — fetch a session (group `session`). Matches the golden
/// `v2.session.get`: 200 `{ data: SessionV2Info }`, 400/401 typed errors, 404 `SessionNotFoundError`.
/// Reads the shared `session` projection table (TS maintains it via the projector on the write path).
#[utoipa::path(
    get,
    path = "/api/session/{sessionID}",
    operation_id = "v2.session.get",
    params(("sessionID" = String, Path, description = "Session id")),
    responses(
        (status = 200, description = "Success", body = inline(opencode_proto::SessionGetResponse)),
        (status = 400, description = "Bad request", body = opencode_proto::InvalidRequestError),
        (status = 401, description = "Unauthorized", body = opencode_proto::UnauthorizedError),
        (status = 404, description = "Session not found", body = opencode_proto::SessionNotFoundError)
    ),
    tag = "sessions"
)]
async fn v2_session_get(
    State(state): State<ServerState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Result<Json<opencode_proto::SessionGetResponse>, SessionGetError> {
    let record = state
        .ctx
        .sessions()
        .get(&session_id)
        .await
        .map_err(|e| SessionGetError::Internal(e.to_string()))?;
    match record {
        Some(record) => Ok(Json(opencode_proto::SessionGetResponse {
            data: session_record_to_info(record),
        })),
        None => Err(SessionGetError::NotFound(session_id)),
    }
}

/// Default page size when `limit` is omitted (mirrors TS `DefaultSessionsLimit`).
const DEFAULT_SESSIONS_LIMIT: i64 = 50;

/// Error responder for `v2.session.list`, producing the golden 400 union arms (`InvalidRequestError`
/// for a bad param, `InvalidCursorError` for an undecodable cursor) or a generic 500 envelope.
pub enum SessionListFailure {
    /// Invalid query parameter (400 `InvalidRequestError`).
    BadRequest(String),
    /// Undecodable pagination cursor (400 `InvalidCursorError`).
    InvalidCursor,
    /// Store read failed (500).
    Internal(String),
}

impl axum::response::IntoResponse for SessionListFailure {
    fn into_response(self) -> axum::response::Response {
        match self {
            SessionListFailure::BadRequest(message) => (
                axum::http::StatusCode::BAD_REQUEST,
                Json(opencode_proto::InvalidRequestError {
                    tag: "InvalidRequestError".to_string(),
                    message,
                    kind: None,
                    field: None,
                }),
            )
                .into_response(),
            SessionListFailure::InvalidCursor => (
                axum::http::StatusCode::BAD_REQUEST,
                Json(opencode_proto::InvalidCursorError {
                    tag: "InvalidCursorError".to_string(),
                    message: "Invalid cursor".to_string(),
                }),
            )
                .into_response(),
            SessionListFailure::Internal(message) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(opencode_proto::ErrorEnvelope {
                    tag: "InternalError".to_string(),
                    message,
                }),
            )
                .into_response(),
        }
    }
}

/// Decoded payload of the opaque `v2.session.list` cursor: the filters/order to preserve across
/// pages, plus the keyset anchor. base64url(JSON) of this is the opaque token. This is a Rust-native
/// format (not byte-interchangeable with TS cursors mid-pagination — a documented rollback caveat);
/// the pagination *behavior* matches TS (`session.ts` keyset).
#[derive(serde::Serialize, serde::Deserialize)]
struct CursorPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    order: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    search: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    directory: Option<String>,
    anchor: opencode_db::ListAnchor,
}

fn encode_cursor(payload: &CursorPayload) -> String {
    use base64::Engine;
    let json = serde_json::to_vec(payload).unwrap_or_default();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

fn decode_cursor(raw: &str) -> Result<CursorPayload, SessionListFailure> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| SessionListFailure::InvalidCursor)?;
    serde_json::from_slice(&bytes).map_err(|_| SessionListFailure::InvalidCursor)
}

/// `GET /api/session` — list sessions (group `session`). Matches the golden `v2.session.list`:
/// 200 `SessionsResponse`, 400 union (`InvalidCursorError`/`InvalidRequestError`), 401. Reads the
/// shared `session` projection table with `limit`/`order`/`search`/`project`/`workspace`/`directory`
/// filters and keyset cursor pagination (`previous`/`next`), defaulting to 50 rows.
#[utoipa::path(
    get,
    path = "/api/session",
    operation_id = "v2.session.list",
    params(
        ("workspace" = Option<String>, Query, description = "Filter by workspace id"),
        ("project" = Option<String>, Query, description = "Filter by project id"),
        ("directory" = Option<String>, Query, description = "Filter by directory"),
        ("search" = Option<String>, Query, description = "Title substring (case-insensitive)"),
        ("order" = Option<String>, Query, description = "asc | desc (default desc)"),
        ("limit" = Option<i64>, Query, description = "Max results (default 50)"),
        ("cursor" = Option<String>, Query, description = "Opaque pagination cursor")
    ),
    responses(
        (status = 200, description = "Sessions", body = opencode_proto::SessionsResponse),
        (status = 400, description = "Bad request", body = opencode_proto::SessionListError),
        (status = 401, description = "Unauthorized", body = opencode_proto::UnauthorizedError)
    ),
    tag = "sessions"
)]
async fn v2_session_list(
    State(state): State<ServerState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<opencode_proto::SessionsResponse>, SessionListFailure> {
    let pick = |key: &str| params.get(key).filter(|s| !s.is_empty()).cloned();
    // `limit` always comes from the request (the cursor omits it), defaulting to 50.
    let limit = match params.get("limit") {
        None => DEFAULT_SESSIONS_LIMIT,
        Some(s) => s.parse::<i64>().map_err(|_| {
            SessionListFailure::BadRequest(format!("limit must be an integer: {s}"))
        })?,
    };

    // A cursor supplies the filters/order + anchor; otherwise read them from the query params.
    let (order, search, project, workspace, directory, anchor) = match pick("cursor") {
        Some(raw) => {
            let c = decode_cursor(&raw)?;
            (
                c.order,
                c.search,
                c.project,
                c.workspace,
                c.directory,
                Some(c.anchor),
            )
        }
        None => (
            pick("order"),
            pick("search"),
            pick("project"),
            pick("workspace"),
            pick("directory"),
            None,
        ),
    };
    let descending = match order.as_deref() {
        None | Some("desc") => true,
        Some("asc") => false,
        Some(other) => {
            return Err(SessionListFailure::BadRequest(format!(
                "invalid order: {other} (expected asc|desc)"
            )))
        }
    };

    let query = opencode_db::SessionListQuery {
        limit: Some(limit),
        descending,
        search: search.clone(),
        project: project.clone(),
        workspace: workspace.clone(),
        directory: directory.clone(),
        anchor,
    };
    let records = state
        .ctx
        .sessions()
        .list(&query)
        .await
        .map_err(|e| SessionListFailure::Internal(e.to_string()))?;

    // Build adjacent-page cursors from the first/last rows (same filters/order, new anchor).
    let order_field = if descending { "desc" } else { "asc" };
    let make_cursor = |record: &opencode_db::SessionRecord,
                       direction: opencode_db::ListDirection| {
        encode_cursor(&CursorPayload {
            order: Some(order_field.to_string()),
            search: search.clone(),
            project: project.clone(),
            workspace: workspace.clone(),
            directory: directory.clone(),
            anchor: opencode_db::ListAnchor {
                id: record.id.clone(),
                time: record.time_created,
                direction,
            },
        })
    };
    let cursor = opencode_proto::SessionCursor {
        previous: records
            .first()
            .map(|r| make_cursor(r, opencode_db::ListDirection::Previous)),
        next: records
            .last()
            .map(|r| make_cursor(r, opencode_db::ListDirection::Next)),
    };

    let data = records.into_iter().map(session_record_to_info).collect();
    Ok(Json(opencode_proto::SessionsResponse { data, cursor }))
}

/// Map a `project` projection row to the `Project` wire shape (the `icon_*` columns fold into one
/// `icon` object; `commands`/`sandboxes` come from JSON columns).
fn project_record_to_info(r: opencode_db::ProjectRecord) -> opencode_proto::Project {
    let icon = if r.icon_url.is_some() || r.icon_url_override.is_some() || r.icon_color.is_some() {
        Some(opencode_proto::ProjectIcon {
            url: r.icon_url,
            override_: r.icon_url_override,
            color: r.icon_color,
        })
    } else {
        None
    };
    let commands = r
        .commands
        .as_ref()
        .map(|c| opencode_proto::ProjectCommands {
            start: c.get("start").and_then(|v| v.as_str()).map(String::from),
        });
    opencode_proto::Project {
        id: r.id,
        worktree: r.worktree,
        vcs: r.vcs,
        name: r.name,
        icon,
        commands,
        time: opencode_proto::ProjectTime {
            created: r.time_created,
            updated: r.time_updated,
            initialized: r.time_initialized,
        },
        sandboxes: r.sandboxes,
    }
}

/// `GET /project` — list projects (group `project`). Matches the golden `project.list`:
/// 200 `array<Project>` + 400 `BadRequestError`. Reads the shared `project` projection table — the
/// second entity proving the `AppContext → Store → projection` pattern generalizes. Returns all
/// projects; `directory`/`workspace` scoping is accepted but not yet applied (a documented follow-up).
#[utoipa::path(
    get,
    path = "/project",
    operation_id = "project.list",
    params(
        ("directory" = Option<String>, Query, description = "Directory context (not yet applied)"),
        ("workspace" = Option<String>, Query, description = "Workspace id (not yet applied)")
    ),
    responses(
        (status = 200, description = "Projects", body = Vec<opencode_proto::Project>),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "project"
)]
async fn project_list(
    State(state): State<ServerState>,
) -> Result<Json<Vec<opencode_proto::Project>>, ApiError> {
    let records = state.ctx.projects().list().await.map_err(|e| {
        ApiError(opencode_effect::AppError::Other(anyhow::anyhow!(
            e.to_string()
        )))
    })?;
    let data = records.into_iter().map(project_record_to_info).collect();
    Ok(Json(data))
}

/// `GET /project/current` — the project for the current directory (group `project`). Matches the
/// golden `project.current`: 200 `Project` + 400 `BadRequestError`. Resolves `directory` (param or
/// cwd) to its git worktree (via `opencode_tools::git::root`), then looks the project up by that
/// `worktree` — a non-PK lookup. The TS remote-id derivation and non-repo "global" fallback are
/// documented follow-ups; if no project matches the worktree, returns 400.
#[utoipa::path(
    get,
    path = "/project/current",
    operation_id = "project.current",
    params(
        ("directory" = Option<String>, Query, description = "Directory to resolve (defaults to cwd)"),
        ("workspace" = Option<String>, Query, description = "Workspace id (not yet applied)")
    ),
    responses(
        (status = 200, description = "Current project", body = opencode_proto::Project),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "project"
)]
async fn project_current(
    State(state): State<ServerState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<opencode_proto::Project>, ApiBadRequest> {
    let directory = params.get("directory").cloned().unwrap_or_else(|| {
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    });
    let worktree = opencode_tools::git::root(std::path::Path::new(&directory))
        .await
        .ok()
        .flatten()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| directory.clone());
    let record = state
        .ctx
        .projects()
        .get_by_worktree(&worktree)
        .await
        .map_err(|e| bad_request(e.to_string(), "Unknown"))?;
    match record {
        Some(record) => Ok(Json(project_record_to_info(record))),
        None => Err(bad_request(
            format!("no project for worktree: {worktree}"),
            "Unknown",
        )),
    }
}

/// Code-first OpenAPI document. `xtask openapi` emits it; `xtask openapi-diff` checks it against
/// `packages/sdk/openapi.json` per route group.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(
        health,
        global_health,
        path_get,
        find_files,
        find_text,
        app_log,
        v2_session_get,
        v2_session_list,
        project_list,
        project_current
    ),
    components(schemas(
        opencode_proto::Health,
        opencode_proto::ErrorEnvelope,
        opencode_proto::BadRequestError,
        opencode_proto::BadRequestData,
        opencode_proto::Path,
        opencode_proto::TextSearchMatch,
        opencode_proto::TextWrap,
        opencode_proto::TextSubmatch,
        opencode_proto::LogEntry,
        opencode_proto::EffectHttpApiBadRequest,
        opencode_proto::InvalidRequestError,
        opencode_proto::RequestError,
        opencode_proto::SessionV2Info,
        opencode_proto::ModelRef,
        opencode_proto::SessionTokens,
        opencode_proto::TokenCache,
        opencode_proto::SessionTime,
        opencode_proto::LocationRef,
        opencode_proto::SessionNotFoundError,
        opencode_proto::UnauthorizedError,
        opencode_proto::SessionsResponse,
        opencode_proto::SessionCursor,
        opencode_proto::InvalidCursorError,
        opencode_proto::SessionListError,
        opencode_proto::Project,
        opencode_proto::ProjectIcon,
        opencode_proto::ProjectCommands,
        opencode_proto::ProjectTime
    )),
    tags(
        (name = "control", description = "Control-plane routes"),
        (name = "global", description = "Global control-plane routes"),
        (name = "instance", description = "Instance-scoped routes"),
        (name = "file", description = "File routes"),
        (name = "sessions", description = "Session routes"),
        (name = "project", description = "Project routes")
    ),
    info(title = "opencode", version = VERSION)
)]
pub struct ApiDoc;

/// Return the generated OpenAPI document.
pub fn openapi_document() -> utoipa::openapi::OpenApi {
    use utoipa::OpenApi;
    ApiDoc::openapi()
}

/// HTTP wrapper for [`opencode_effect::AppError`]. Maps domain errors to a status code plus the
/// `_tag` error envelope so error responses match the TypeScript server. Handlers return
/// `Result<T, ApiError>`; `?` converts an `AppError` automatically.
pub struct ApiError(pub opencode_effect::AppError);

impl From<opencode_effect::AppError> for ApiError {
    fn from(err: opencode_effect::AppError) -> Self {
        Self(err)
    }
}

impl ApiError {
    /// The HTTP status code and serialized error envelope for this error.
    fn parts(&self) -> (u16, opencode_proto::ErrorEnvelope) {
        (
            self.0.status_code(),
            opencode_proto::ErrorEnvelope {
                tag: self.0.tag().to_string(),
                message: self.0.to_string(),
            },
        )
    }
}

impl axum::response::IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let (status, body) = self.parts();
        let status = axum::http::StatusCode::from_u16(status)
            .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(body)).into_response()
    }
}

/// Build the axum router: always-native liveness + cut-over contract routes + proxy fallback.
pub fn build_router(state: ServerState) -> Router {
    let mut router = Router::new()
        .route("/_rust/health", get(rust_health))
        .route("/_rust/event", get(rust_event));

    // Native contract routes are enabled here as they are cut over, gated by the route table.
    if state.routes.handles("health") {
        router = router.route("/health", get(health));
    }
    if state.routes.handles("global") {
        router = router.route("/global/health", get(global_health));
    }
    if state.routes.handles("instance") {
        router = router.route("/path", get(path_get));
    }
    if state.routes.handles("file") {
        router = router.route("/find/file", get(find_files));
        router = router.route("/find", get(find_text));
    }
    if state.routes.handles("control") {
        router = router.route("/log", post(app_log));
    }
    if state.routes.handles("session") {
        router = router.route("/api/session", get(v2_session_list));
        router = router.route("/api/session/{sessionID}", get(v2_session_get));
    }
    if state.routes.handles("project") {
        router = router.route("/project", get(project_list));
        router = router.route("/project/current", get(project_current));
    }
    // Internal Phase-4 proving route (not a contract path): build the runner and drive one turn.
    if state.routes.handles("session-exec") {
        router = router.route(
            "/_rust/session/{sessionID}/execute",
            post(rust_session_execute),
        );
    }

    router.fallback(proxy::proxy_handler).with_state(state)
}

/// Bind `bind` (e.g. `127.0.0.1:4096`) and serve the router until shutdown.
pub async fn serve(state: ServerState, bind: &str) -> anyhow::Result<()> {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("listening on http://{bind}");
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use opencode_effect::AppServices;

    #[test]
    fn route_table_parses_and_matches() {
        let rt = RouteTable::parse(" health , fs ,, location ");
        assert!(rt.handles("health"));
        assert!(rt.handles("fs"));
        assert!(rt.handles("location"));
        assert!(!rt.handles("session"));
        assert_eq!(rt.len(), 3);
    }

    #[test]
    fn openapi_document_contains_health() {
        let doc = openapi_document();
        let json = serde_json::to_value(&doc).unwrap();
        assert!(
            json["paths"]["/health"].is_object(),
            "health path must be present"
        );
    }

    #[test]
    fn api_error_maps_status_and_tag() {
        let (status, env) = ApiError(opencode_effect::AppError::NotFound("ses_1".into())).parts();
        assert_eq!(status, 404);
        assert_eq!(env.tag, "NotFoundError");
        assert!(env.message.contains("ses_1"));

        let (status, env) = ApiError(opencode_effect::AppError::Conflict("dup".into())).parts();
        assert_eq!(status, 409);
        assert_eq!(env.tag, "ConflictError");
    }

    #[test]
    fn openapi_has_global_health_contract_operation() {
        let json = serde_json::to_value(openapi_document()).unwrap();
        let op = &json["paths"]["/global/health"]["get"];
        assert_eq!(op["operationId"], "global.health");
        assert!(op["responses"]["200"].is_object());
        assert!(op["responses"]["400"].is_object());
    }

    #[tokio::test]
    async fn global_health_handler_returns_healthy() {
        let Json(body) = global_health().await;
        assert!(body.healthy);
        assert_eq!(body.version, VERSION);
    }

    #[test]
    fn openapi_has_path_get_contract_operation() {
        let json = serde_json::to_value(openapi_document()).unwrap();
        let op = &json["paths"]["/path"]["get"];
        assert_eq!(op["operationId"], "path.get");
        assert_eq!(
            op["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Path"
        );
        assert!(op["responses"]["400"].is_object());
    }

    #[tokio::test]
    async fn path_get_handler_resolves_directory() {
        let Json(body) = path_get(Query(std::collections::HashMap::new())).await;
        assert!(!body.directory.is_empty());
        assert!(body.config.ends_with("/opencode"));
    }

    #[test]
    fn openapi_has_find_files_contract_operation() {
        let json = serde_json::to_value(openapi_document()).unwrap();
        let op = &json["paths"]["/find/file"]["get"];
        assert_eq!(op["operationId"], "find.files");
        assert_eq!(
            op["responses"]["200"]["content"]["application/json"]["schema"]["type"],
            "array"
        );
        assert!(op["responses"]["400"].is_object());
    }

    #[tokio::test]
    async fn find_files_missing_query_is_bad_request() {
        let res = find_files(Query(std::collections::HashMap::new())).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn find_files_with_query_returns_ok() {
        let mut q = std::collections::HashMap::new();
        q.insert("query".to_string(), "Cargo".to_string());
        q.insert(
            "directory".to_string(),
            env!("CARGO_MANIFEST_DIR").to_string(),
        );
        let res = find_files(Query(q)).await;
        assert!(res.is_ok());
    }

    #[test]
    fn openapi_has_find_text_contract_operation() {
        let json = serde_json::to_value(openapi_document()).unwrap();
        let op = &json["paths"]["/find"]["get"];
        assert_eq!(op["operationId"], "find.text");
        assert_eq!(
            op["responses"]["200"]["content"]["application/json"]["schema"]["type"],
            "array"
        );
        assert!(op["responses"]["400"].is_object());
    }

    #[tokio::test]
    async fn find_text_missing_pattern_is_bad_request() {
        let res = find_text(Query(std::collections::HashMap::new())).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn find_text_with_pattern_returns_matches() {
        let mut q = std::collections::HashMap::new();
        q.insert("pattern".to_string(), "find_text".to_string());
        q.insert(
            "directory".to_string(),
            env!("CARGO_MANIFEST_DIR").to_string(),
        );
        let Json(matches) = find_text(Query(q)).await.unwrap();
        // This source file contains "find_text", so there is at least one match with a submatch.
        assert!(!matches.is_empty());
        assert!(matches.iter().all(|m| !m.submatches.is_empty()));
    }

    #[test]
    fn openapi_has_app_log_contract_operation() {
        let json = serde_json::to_value(openapi_document()).unwrap();
        let op = &json["paths"]["/log"]["post"];
        assert_eq!(op["operationId"], "app.log");
        assert_eq!(
            op["responses"]["200"]["content"]["application/json"]["schema"]["type"],
            "boolean"
        );
        assert!(op["responses"]["400"].is_object());
    }

    #[tokio::test]
    async fn app_log_logs_and_returns_true() {
        let Json(ok) = app_log(Json(opencode_proto::LogEntry {
            service: "tui".into(),
            level: "info".into(),
            message: "hi".into(),
        }))
        .await;
        assert!(ok);
    }

    #[tokio::test]
    async fn router_serves_native_route_and_proxies_others() {
        use tower::ServiceExt;
        let state = ServerState {
            ctx: AppContext::in_memory(),
            // Only the `global` group is cut over natively here.
            routes: RouteTable::parse("global"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        let app = build_router(state);

        // `/global/health` is enabled → served natively (200, healthy: true).
        let resp = app
            .clone()
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/global/health")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["healthy"], serde_json::json!(true));

        // `/path` is NOT enabled → falls through to the proxy → unreachable upstream → 502.
        let resp = app
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/path")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }

    fn test_session_record(id: &str) -> opencode_db::SessionRecord {
        opencode_db::SessionRecord {
            id: id.to_string(),
            project_id: "prj_1".into(),
            parent_id: None,
            agent: Some("build".into()),
            model: Some(serde_json::json!({
                "id": "claude", "providerID": "anthropic", "variant": "default"
            })),
            cost: 1.5,
            tokens_input: 2,
            tokens_output: 3,
            tokens_reasoning: 4,
            tokens_cache_read: 5,
            tokens_cache_write: 6,
            title: "Hello".into(),
            directory: "/repo".into(),
            workspace_id: None,
            path: None,
            time_created: 100,
            time_updated: 200,
            time_archived: None,
        }
    }

    #[test]
    fn openapi_has_v2_session_get_contract_operation() {
        let json = serde_json::to_value(openapi_document()).unwrap();
        let op = &json["paths"]["/api/session/{sessionID}"]["get"];
        assert_eq!(op["operationId"], "v2.session.get");
        assert_eq!(
            op["responses"]["200"]["content"]["application/json"]["schema"]["properties"]["data"]
                ["$ref"],
            "#/components/schemas/SessionV2Info"
        );
        for code in ["400", "401", "404"] {
            assert!(op["responses"][code].is_object(), "missing {code} response");
        }
    }

    #[tokio::test]
    async fn v2_session_get_returns_session_when_present() {
        use tower::ServiceExt;
        let sessions = Arc::new(opencode_db::MemorySessionStore::new());
        sessions.insert(test_session_record("ses_1"));
        let state = ServerState {
            ctx: AppContext::new(AppServices {
                sessions,
                ..Default::default()
            }),
            routes: RouteTable::parse("session"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/session/ses_1")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["data"]["id"], "ses_1");
        assert_eq!(v["data"]["projectID"], "prj_1");
        assert_eq!(v["data"]["model"]["providerID"], "anthropic");
        assert_eq!(v["data"]["tokens"]["input"], 2.0);
        assert_eq!(v["data"]["location"]["directory"], "/repo");
        // Optional/absent fields are omitted (workspaceID, parentID, subpath).
        assert!(v["data"]["location"].get("workspaceID").is_none());
    }

    #[tokio::test]
    async fn v2_session_get_missing_is_not_found() {
        use tower::ServiceExt;
        let state = ServerState {
            ctx: AppContext::in_memory(), // empty session store
            routes: RouteTable::parse("session"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/session/ses_missing")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["_tag"], "SessionNotFoundError");
        assert_eq!(v["sessionID"], "ses_missing");
    }

    #[tokio::test]
    async fn v2_session_get_proxies_when_group_disabled() {
        use tower::ServiceExt;
        let state = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse(""), // `session` not enabled → proxy fallback
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/session/ses_1")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Unreachable upstream → 502, proving the route is proxied (not served natively).
        assert_eq!(resp.status(), 502);
    }

    #[test]
    fn openapi_has_v2_session_list_contract_operation() {
        let json = serde_json::to_value(openapi_document()).unwrap();
        let op = &json["paths"]["/api/session"]["get"];
        assert_eq!(op["operationId"], "v2.session.list");
        assert_eq!(
            op["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/SessionsResponse"
        );
        for code in ["400", "401"] {
            assert!(op["responses"][code].is_object(), "missing {code} response");
        }
    }

    #[tokio::test]
    async fn v2_session_list_returns_ordered_sessions() {
        use tower::ServiceExt;
        let sessions = Arc::new(opencode_db::MemorySessionStore::new());
        let mut older = test_session_record("ses_a");
        older.time_created = 100;
        let mut newer = test_session_record("ses_b");
        newer.time_created = 300;
        sessions.insert(older);
        sessions.insert(newer);
        let state = ServerState {
            ctx: AppContext::new(AppServices {
                sessions,
                ..Default::default()
            }),
            routes: RouteTable::parse("session"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/session")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Default order is descending by time_created → newest first.
        assert_eq!(v["data"].as_array().unwrap().len(), 2);
        assert_eq!(v["data"][0]["id"], "ses_b");
        assert_eq!(v["data"][1]["id"], "ses_a");
        // `cursor` is always present (empty here).
        assert!(v["cursor"].is_object());
    }

    #[tokio::test]
    async fn v2_session_list_invalid_order_is_bad_request() {
        use tower::ServiceExt;
        let state = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse("session"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/session?order=sideways")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["_tag"], "InvalidRequestError");
    }

    #[tokio::test]
    async fn v2_session_list_cursor_paginates() {
        use tower::ServiceExt;
        let sessions = Arc::new(opencode_db::MemorySessionStore::new());
        for (id, t) in [("ses_a", 100), ("ses_b", 200), ("ses_c", 300)] {
            let mut r = test_session_record(id);
            r.time_created = t;
            sessions.insert(r);
        }
        let state = ServerState {
            ctx: AppContext::new(AppServices {
                sessions,
                ..Default::default()
            }),
            routes: RouteTable::parse("session"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        let app = build_router(state);

        let read = |app: axum::Router, uri: String| async move {
            let resp = app
                .oneshot(
                    axum::extract::Request::builder()
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), 200);
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()
        };

        // Page 1: newest first, one per page.
        let p1 = read(app.clone(), "/api/session?limit=1".to_string()).await;
        assert_eq!(p1["data"].as_array().unwrap().len(), 1);
        assert_eq!(p1["data"][0]["id"], "ses_c");
        let next = p1["cursor"]["next"].as_str().unwrap().to_string();

        // Page 2: follow the `next` cursor → the second-newest session.
        let p2 = read(app.clone(), format!("/api/session?limit=1&cursor={next}")).await;
        assert_eq!(p2["data"][0]["id"], "ses_b");

        // The `previous` cursor from page 2 returns page 1.
        let prev = p2["cursor"]["previous"].as_str().unwrap().to_string();
        let back = read(app, format!("/api/session?limit=1&cursor={prev}")).await;
        assert_eq!(back["data"][0]["id"], "ses_c");
    }

    #[tokio::test]
    async fn v2_session_list_invalid_cursor_is_invalid_cursor_error() {
        use tower::ServiceExt;
        let state = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse("session"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        // Valid base64url ("aGVsbG8" = "hello") but not a JSON cursor payload.
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/session?cursor=aGVsbG8")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["_tag"], "InvalidCursorError");
    }

    fn test_project_record(id: &str) -> opencode_db::ProjectRecord {
        opencode_db::ProjectRecord {
            id: id.to_string(),
            worktree: "/repo".into(),
            vcs: Some("git".into()),
            name: Some("Repo".into()),
            icon_url: Some("http://icon".into()),
            icon_url_override: None,
            icon_color: None,
            time_created: 100,
            time_updated: 200,
            time_initialized: Some(150),
            sandboxes: vec!["/repo/sb".into()],
            commands: Some(serde_json::json!({ "start": "bun dev" })),
        }
    }

    #[test]
    fn openapi_has_project_list_contract_operation() {
        let json = serde_json::to_value(openapi_document()).unwrap();
        let op = &json["paths"]["/project"]["get"];
        assert_eq!(op["operationId"], "project.list");
        assert_eq!(
            op["responses"]["200"]["content"]["application/json"]["schema"]["type"],
            "array"
        );
        assert_eq!(
            op["responses"]["200"]["content"]["application/json"]["schema"]["items"]["$ref"],
            "#/components/schemas/Project"
        );
        assert!(op["responses"]["400"].is_object());
    }

    #[tokio::test]
    async fn project_list_returns_projects() {
        use tower::ServiceExt;
        let projects = Arc::new(opencode_db::MemoryProjectStore::new());
        projects.insert(test_project_record("prj_1"));
        let state = ServerState {
            ctx: AppContext::new(AppServices {
                projects,
                ..Default::default()
            }),
            routes: RouteTable::parse("project"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/project")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 1);
        let p = &v[0];
        assert_eq!(p["id"], "prj_1");
        assert_eq!(p["worktree"], "/repo");
        assert_eq!(p["vcs"], "git");
        // icon_* columns folded into one object.
        assert_eq!(p["icon"]["url"], "http://icon");
        assert!(p["icon"].get("override").is_none());
        assert_eq!(p["commands"]["start"], "bun dev");
        assert_eq!(p["time"]["created"], 100);
        assert_eq!(p["time"]["initialized"], 150);
        assert_eq!(p["sandboxes"][0], "/repo/sb");
    }

    #[tokio::test]
    async fn rust_event_serves_sse_stream() {
        use tower::ServiceExt;
        let state = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse(""),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/_rust/event")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        // Always-native SSE infra route → 200 text/event-stream. (Body is an open stream; not read.)
        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "text/event-stream"
        );
    }

    #[test]
    fn openapi_has_project_current_contract_operation() {
        let json = serde_json::to_value(openapi_document()).unwrap();
        let op = &json["paths"]["/project/current"]["get"];
        assert_eq!(op["operationId"], "project.current");
        assert_eq!(
            op["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/Project"
        );
        assert!(op["responses"]["400"].is_object());
    }

    #[tokio::test]
    async fn project_current_resolves_by_worktree() {
        use tower::ServiceExt;
        // A non-existent directory has no git root → the handler falls back to the directory string
        // as the worktree, making the lookup deterministic without a real repo.
        let worktree = "/nonexistent/opencode-current-test";
        let projects = Arc::new(opencode_db::MemoryProjectStore::new());
        let mut record = test_project_record("prj_cur");
        record.worktree = worktree.to_string();
        projects.insert(record);
        let state = ServerState {
            ctx: AppContext::new(AppServices {
                projects,
                ..Default::default()
            }),
            routes: RouteTable::parse("project"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri(format!("/project/current?directory={worktree}"))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["id"], "prj_cur");
        assert_eq!(v["worktree"], worktree);
    }

    #[tokio::test]
    async fn project_current_unknown_worktree_is_bad_request() {
        use tower::ServiceExt;
        let state = ServerState {
            ctx: AppContext::in_memory(), // empty project store
            routes: RouteTable::parse("project"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/project/current?directory=/nonexistent/none")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 400);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["name"], "BadRequest");
    }

    #[tokio::test]
    async fn project_list_proxies_when_group_disabled() {
        use tower::ServiceExt;
        let state = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse(""), // `project` not enabled → proxy fallback
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/project")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }

    // ---- Phase 4: the internal session-execute proving route (engine factory injected) ----

    /// An [`EngineFactory`] returning an `anthropic-messages` engine pointed at a local test server, so
    /// the runner drives a real transport round-trip with no network/TLS/credentials.
    struct TestEngines {
        url: String,
    }

    impl EngineFactory for TestEngines {
        fn build(&self, _model: &str) -> Result<Arc<dyn LlmEngine>, EngineError> {
            Ok(Arc::new(opencode_core::provider::AnthropicEngine::new(
                reqwest::Client::new(),
                self.url.clone(),
                "test".to_string(),
            )))
        }
    }

    const EXEC_TEXT_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":20,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"It is sunny in Paris.\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":20,\"output_tokens\":7}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    const EXEC_TOOL_SSE: &str = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":10,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"bash\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"echo marker-xyz\\\"}\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"input_tokens\":10,\"output_tokens\":5}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";

    /// Spawn a local anthropic-style SSE server returning `bodies` in order (last repeats).
    async fn spawn_anthropic(bodies: &'static [&'static str]) -> String {
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

    fn exec_state(url: String, root: std::path::PathBuf) -> ServerState {
        ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse("session-exec"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices {
                engines: Arc::new(TestEngines { url }),
                gate: Arc::new(AllowAll),
                root,
            },
        }
    }

    fn exec_request(session: &str, model: &str, prompt: &str) -> axum::extract::Request {
        axum::extract::Request::builder()
            .method("POST")
            .uri(format!("/_rust/session/{session}/execute"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({ "model": model, "prompt": prompt }).to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn execute_runs_a_turn_and_persists_and_publishes() {
        use tower::ServiceExt;
        let url = spawn_anthropic(&[EXEC_TEXT_SSE]).await;
        let dir = tempfile::tempdir().unwrap();
        let state = exec_state(url, dir.path().to_path_buf());
        // Subscribe before the run so we observe the announced events.
        let mut bus = state.ctx.event_bus().subscribe();

        let resp = build_router(state.clone())
            .oneshot(exec_request(
                "ses_exec",
                "anthropic/claude-haiku-4-5-20251001",
                "weather in Paris?",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["outcome"], "completed");
        assert_eq!(v["steps"], 1);
        assert!(v["text"].as_str().unwrap().contains("sunny"));

        // Persisted under the session aggregate.
        let stored = state.ctx.event_store().read("ses_exec", 0).await.unwrap();
        let kinds: Vec<&str> = stored.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["message.assistant.1", "session.finished.1"]);

        // Announced on the bus.
        let mut seen = Vec::new();
        while let Ok(ev) = bus.try_recv() {
            seen.push(ev.kind);
        }
        assert!(seen.iter().any(|k| k.as_str() == "message.assistant.1"));
        assert!(seen.iter().any(|k| k.as_str() == "session.finished.1"));
    }

    #[tokio::test]
    async fn execute_drives_a_native_tool_loop() {
        use tower::ServiceExt;
        let url = spawn_anthropic(&[EXEC_TOOL_SSE, EXEC_TEXT_SSE]).await;
        let dir = tempfile::tempdir().unwrap();
        let state = exec_state(url, dir.path().to_path_buf());

        let resp = build_router(state.clone())
            .oneshot(exec_request(
                "ses_tool",
                "anthropic/claude-haiku-4-5-20251001",
                "run echo",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["outcome"], "completed");
        assert_eq!(v["steps"], 2);

        let stored = state.ctx.event_store().read("ses_tool", 0).await.unwrap();
        let kinds: Vec<&str> = stored.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "message.assistant.1",
                "message.tool_results.1",
                "message.assistant.1",
                "session.finished.1",
            ]
        );
        // The bash tool actually ran in the toolbox root; its stdout is in the tool-results event.
        let result = stored[1].data["results"][0]["result"].as_str().unwrap();
        assert!(result.contains("marker-xyz"));
    }

    #[tokio::test]
    async fn execute_is_gated_behind_session_exec() {
        use tower::ServiceExt;
        // `session-exec` not enabled → route not registered → proxy fallback → 502 (upstream down).
        let state = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse(""),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
        };
        let resp = build_router(state)
            .oneshot(exec_request("ses_x", "anthropic/x", "hi"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }
}
