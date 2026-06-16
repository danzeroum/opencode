//! axum HTTP API + the **strangler-fig seam**.
//!
//! Native contract routes are added to the router as they are cut over, gated by a [`RouteTable`]
//! parsed from `OPENCODE_RUST_ROUTES`. Everything not handled natively falls through to
//! [`proxy::proxy_handler`], which forwards the request to the existing TypeScript server. With an
//! empty route table the server proxies 100% of contract traffic — proving the seam end-to-end with
//! zero native handlers. An always-native `/_rust/health` liveness route supports the Phase 0 smoke.

pub mod proxy;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
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
    SessionRun, ToolBox,
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
    /// Per-session background execution (admit + wake-coalesced drain).
    pub coordinator: SessionCoordinator,
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

/// One prompt admitted to a session's inbox (the internal proving-route shape; the public contract's
/// richer `Prompt` / `SessionInput` types land with the `/api/session/{id}/prompt` cutover).
#[derive(Clone)]
pub(crate) struct AdmittedPrompt {
    pub(crate) model: String,
    pub(crate) prompt: String,
    pub(crate) system: Vec<String>,
    pub(crate) step_limit: usize,
}

/// Runs one admitted prompt to completion — the injection seam between the [`SessionCoordinator`]'s
/// scheduling and the actual turn loop. Production drives the real runner; tests inject a controllable
/// one to exercise the coordinator's concurrency in isolation.
#[async_trait]
pub(crate) trait TurnRunner: Send + Sync {
    async fn run(&self, session_id: &str, prompt: AdmittedPrompt, cancel: Arc<AtomicBool>);
}

/// Production [`TurnRunner`]: build the engine + native tools + sink and drive one turn (the same
/// machinery the synchronous `/_rust/session/{id}/execute` route uses).
struct LiveTurnRunner {
    ctx: AppContext,
    runner: RunnerServices,
}

#[async_trait]
impl TurnRunner for LiveTurnRunner {
    async fn run(&self, session_id: &str, prompt: AdmittedPrompt, cancel: Arc<AtomicBool>) {
        tracing::info!(session = session_id, model = %prompt.model, "background turn started");
        match drive_one_turn(
            &self.ctx,
            &self.runner,
            session_id,
            &prompt.model,
            prompt.prompt,
            prompt.system,
            prompt.step_limit,
            Some(cancel),
        )
        .await
        {
            Ok(run) => {
                tracing::info!(session = session_id, outcome = ?run.outcome, "background turn finished")
            }
            Err(error) => tracing::warn!(session = session_id, %error, "background turn failed"),
        }
    }
}

/// A session's inbox slot: whether a drain task currently owns it, and the queued prompts a running
/// task will pick up (wake-coalescing).
#[derive(Default)]
struct Slot {
    running: bool,
    pending: VecDeque<AdmittedPrompt>,
    /// Cooperative-cancellation token, shared with the drain task + the runner; set by `cancel`.
    cancel: Arc<AtomicBool>,
}

/// Per-session background execution with **wake-coalescing**: an admit spawns a drain task when the
/// session is idle, otherwise queues the prompt for the task already draining it — at most one task per
/// session at a time, processing its inbox in order. In-memory for now (durable `session_input`
/// admission lands with the public `/prompt` cutover).
#[derive(Clone, Default)]
pub struct SessionCoordinator {
    slots: Arc<Mutex<HashMap<String, Slot>>>,
    spawns: Arc<AtomicU64>,
}

impl SessionCoordinator {
    /// Admit `prompt` for `session_id` and ensure a drain task (using `runner`) is processing it. If a
    /// task already owns the session, the prompt is queued for it (coalesced) and `runner` is unused.
    pub(crate) fn admit(
        &self,
        runner: Arc<dyn TurnRunner>,
        session_id: String,
        prompt: AdmittedPrompt,
    ) {
        let mut slots = self.slots.lock().expect("coordinator mutex poisoned");
        let slot = slots.entry(session_id.clone()).or_default();
        slot.pending.push_back(prompt);
        if slot.running {
            return;
        }
        slot.running = true;
        let cancel = slot.cancel.clone();
        drop(slots);
        self.spawns.fetch_add(1, Ordering::SeqCst);
        let coordinator = self.clone();
        tokio::spawn(async move { coordinator.drain(runner, session_id, cancel).await });
    }

    /// Drain a session's inbox to completion, then drop the slot and release ownership (so a later
    /// admit respawns).
    async fn drain(
        &self,
        runner: Arc<dyn TurnRunner>,
        session_id: String,
        cancel: Arc<AtomicBool>,
    ) {
        loop {
            let next = {
                let mut slots = self.slots.lock().expect("coordinator mutex poisoned");
                let slot = match slots.get_mut(&session_id) {
                    Some(slot) => slot,
                    None => return,
                };
                // A cancel request clears queued work so the session drains to a stop.
                if cancel.load(Ordering::SeqCst) {
                    slot.pending.clear();
                }
                match slot.pending.pop_front() {
                    Some(prompt) => prompt,
                    None => {
                        // Inbox empty: release ownership atomically under the lock.
                        slots.remove(&session_id);
                        return;
                    }
                }
            };
            runner.run(&session_id, next, cancel.clone()).await;
        }
    }

    /// Number of drain tasks spawned over this coordinator's lifetime (observability).
    pub fn spawn_count(&self) -> u64 {
        self.spawns.load(Ordering::SeqCst)
    }

    /// Request cooperative cancellation of `session_id`'s in-flight run: sets the token the runner
    /// checks at each step boundary (and clears the inbox). Returns whether a session slot was found.
    pub(crate) fn cancel(&self, session_id: &str) -> bool {
        let slots = self.slots.lock().expect("coordinator mutex poisoned");
        match slots.get(session_id) {
            Some(slot) => {
                slot.cancel.store(true, Ordering::SeqCst);
                true
            }
            None => false,
        }
    }
}

/// Build the engine + native tools + persist-then-announce sink and drive one user turn to completion.
/// Shared by the synchronous execute route and the background [`LiveTurnRunner`].
#[allow(clippy::too_many_arguments)]
async fn drive_one_turn(
    ctx: &AppContext,
    runner: &RunnerServices,
    session_id: &str,
    model: &str,
    prompt: String,
    system: Vec<String>,
    step_limit: usize,
    cancel: Option<Arc<AtomicBool>>,
) -> Result<SessionRun, String> {
    let (provider, model_id) = split_model(model).map_err(|e| e.to_string())?;
    let engine = runner.engines.build(model).map_err(|e| e.to_string())?;
    let tools: Arc<dyn ToolBox> = Arc::new(NativeToolBox::new(runner.root.clone()));
    let session = Session {
        id: session_id.to_string(),
        model: model_id.to_string(),
        provider: provider.as_str().to_string(),
        system,
        tools: native_tools::tool_definitions(),
        generation: Generation::default(),
        step_limit,
    };
    let primary = EventStoreSink::new(ctx.event_store().clone(), session_id)
        .await
        .map_err(|e| e.to_string())?;
    let secondary = BusSink::new(ctx.event_bus().clone(), session_id);
    let sink = FanOutSink::new(primary, secondary);
    run_gated(
        engine.as_ref(),
        tools,
        &sink,
        runner.gate.as_ref(),
        &session,
        vec![Message::user_text(prompt)],
        cancel,
    )
    .await
    .map_err(|e| e.to_string())
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
    let run = drive_one_turn(
        &state.ctx,
        &state.runner,
        &session_id,
        &payload.model,
        payload.prompt,
        payload.system,
        payload.step_limit,
        None,
    )
    .await
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let (outcome, steps) = match &run.outcome {
        SessionOutcome::Completed { steps } => ("completed", *steps),
        SessionOutcome::StepLimitReached { steps } => ("step_limit_reached", *steps),
        SessionOutcome::AwaitingPermission { steps } => ("awaiting_permission", *steps),
        SessionOutcome::Cancelled { steps } => ("cancelled", *steps),
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

/// Request body for the internal session-prompt proving route (`model` + `prompt` text; the public
/// contract's richer `Prompt` / `delivery` / `resume` land with the cutover).
#[derive(serde::Deserialize)]
struct PromptPayload {
    /// `provider/model` id (e.g. `anthropic/claude-...`).
    model: String,
    /// The user's prompt text.
    prompt: String,
    /// Optional system-prompt parts.
    #[serde(default)]
    system: Vec<String>,
    /// Max turns before stopping (the continuation step limit).
    #[serde(default = "default_step_limit")]
    step_limit: usize,
}

/// `POST /_rust/session/{sessionID}/prompt` — internal, gated (`session-prompt`) proving route for the
/// background coordinator: admit the prompt to the session's inbox and **return immediately** while a
/// background task drives the turn(s), persisting events to the store and announcing them on the bus
/// (observable on `/_rust/event`). Concurrent admits to the same session are wake-coalesced into one
/// task. This is the `admit + schedule` half of the eventual `POST /api/session/{id}/prompt` contract
/// cutover (which adds durable `session_input`, the rich `Prompt` type, idempotency, and `/event`).
async fn rust_session_prompt(
    State(state): State<ServerState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    Json(payload): Json<PromptPayload>,
) -> Result<Json<serde_json::Value>, (axum::http::StatusCode, String)> {
    use axum::http::StatusCode;
    // Validate the model up front so a bad request fails fast (before scheduling a task).
    split_model(&payload.model).map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))?;

    let live: Arc<dyn TurnRunner> = Arc::new(LiveTurnRunner {
        ctx: state.ctx.clone(),
        runner: state.runner.clone(),
    });
    state.coordinator.admit(
        live,
        session_id.clone(),
        AdmittedPrompt {
            model: payload.model,
            prompt: payload.prompt,
            system: payload.system,
            step_limit: payload.step_limit,
        },
    );

    Ok(Json(serde_json::json!({
        "session": session_id,
        "delivery": "steer",
        "status": "admitted",
    })))
}

/// `POST /_rust/session/{sessionID}/abort` — internal, gated (`session-prompt`) cooperative
/// cancellation: sets the session's cancel token so the background runner stops at the next step
/// boundary, recording `session.next.interrupt.requested`. Returns whether a running session was found.
/// (The public `session.abort` instance-route cutover is the follow-up PR.)
async fn rust_session_abort(
    State(state): State<ServerState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Json<serde_json::Value> {
    let cancelled = state.coordinator.cancel(&session_id);
    tracing::info!(session = %session_id, cancelled, "session abort requested");
    Json(serde_json::json!({ "session": session_id, "cancelled": cancelled }))
}

/// `POST /session/{sessionID}/abort` — request cancellation of a session (group `instance`; the first
/// V1 instance-route cutover). Matches the golden `session.abort`: 200 `true`, 400 union. Sets the
/// cooperative cancel token (the runner stops at the next step boundary, recording the interrupt) and
/// acks with `true` — cancellation is best-effort and idempotent (a no-op for an idle session).
#[utoipa::path(
    post,
    path = "/session/{sessionID}/abort",
    operation_id = "session.abort",
    params(
        ("sessionID" = String, Path, description = "Session id"),
        ("directory" = Option<String>, Query, description = "Directory context"),
        ("workspace" = Option<String>, Query, description = "Workspace id")
    ),
    responses(
        (status = 200, description = "Aborted session", body = bool, content_type = "application/json"),
        (status = 400, description = "Bad request", body = opencode_proto::RequestError)
    ),
    tag = "session"
)]
async fn session_abort(
    State(state): State<ServerState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Json<bool> {
    let cancelled = state.coordinator.cancel(&session_id);
    tracing::info!(session = %session_id, cancelled, "session abort (contract)");
    Json(true)
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

/// Wall-clock time in milliseconds since the epoch (the contract's `timestamp` / `timeCreated`).
fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

/// Error responder for `v2.session.prompt`: a contract-shaped 404 `SessionNotFoundError`, a 409
/// `ConflictError`, or a generic 500 envelope.
pub enum SessionPromptError {
    /// No session with that id (404).
    NotFound(String),
    /// An optimistic-concurrency conflict that didn't settle after retries (409 `ConflictError`).
    Conflict(String),
    /// Store/runner failure (500).
    Internal(String),
}

impl axum::response::IntoResponse for SessionPromptError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        match self {
            SessionPromptError::NotFound(id) => (
                StatusCode::NOT_FOUND,
                Json(opencode_proto::SessionNotFoundError {
                    tag: "SessionNotFoundError".to_string(),
                    message: format!("Session {id} not found"),
                    session_id: id,
                }),
            )
                .into_response(),
            SessionPromptError::Conflict(message) => (
                StatusCode::CONFLICT,
                Json(opencode_proto::ConflictError {
                    tag: "ConflictError".to_string(),
                    message,
                    resource: None,
                }),
            )
                .into_response(),
            SessionPromptError::Internal(message) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(opencode_proto::ErrorEnvelope {
                    tag: "InternalError".to_string(),
                    message,
                }),
            )
                .into_response(),
        }
    }
}

/// `POST /api/session/{sessionID}/prompt` — durably admit a prompt and schedule the background runner
/// (group `session`). Matches the golden `v2.session.prompt`: 200 `{ data: SessionInputAdmitted }`,
/// 400/401 typed errors, 404 `SessionNotFoundError`, 409 `ConflictError`. Admission appends a
/// `session.next.prompt.admitted.1` event to the shared store (its aggregate seq is `admittedSeq`); the
/// model comes from the session record (the request carries none). With `resume != false` the
/// background coordinator drives the turn(s) — events stream on `/api/event`.
#[utoipa::path(
    post,
    path = "/api/session/{sessionID}/prompt",
    operation_id = "v2.session.prompt",
    params(("sessionID" = String, Path, description = "Session id")),
    request_body = inline(opencode_proto::SessionPromptRequest),
    responses(
        (status = 200, description = "Admitted", body = inline(opencode_proto::SessionPromptResponse)),
        (status = 400, description = "Bad request", body = opencode_proto::InvalidRequestError),
        (status = 401, description = "Unauthorized", body = opencode_proto::UnauthorizedError),
        (status = 404, description = "Session not found", body = opencode_proto::SessionNotFoundError),
        (status = 409, description = "Conflict", body = opencode_proto::ConflictError)
    ),
    tag = "sessions"
)]
async fn v2_session_prompt(
    State(state): State<ServerState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    Json(request): Json<opencode_proto::SessionPromptRequest>,
) -> Result<Json<opencode_proto::SessionPromptResponse>, SessionPromptError> {
    // The session must exist and carry a model (the request doesn't include one).
    let record = state
        .ctx
        .sessions()
        .get(&session_id)
        .await
        .map_err(|e| SessionPromptError::Internal(e.to_string()))?
        .ok_or_else(|| SessionPromptError::NotFound(session_id.clone()))?;
    let model = record
        .model
        .as_ref()
        .and_then(|m| {
            let id = m.get("id").and_then(|v| v.as_str())?;
            let provider = m.get("providerID").and_then(|v| v.as_str())?;
            Some(format!("{provider}/{id}"))
        })
        .ok_or_else(|| {
            SessionPromptError::Internal(format!("session {session_id} has no model"))
        })?;

    let delivery = request.delivery.unwrap_or(opencode_proto::Delivery::Steer);
    let prompt = request.prompt;
    let now = now_ms();
    let prompt_value =
        serde_json::to_value(&prompt).map_err(|e| SessionPromptError::Internal(e.to_string()))?;
    let delivery_value =
        serde_json::to_value(delivery).map_err(|e| SessionPromptError::Internal(e.to_string()))?;

    // Idempotency: a re-sent admission (the caller supplied an `id` already admitted) replays the
    // original `Admitted`; the same `id` with a *different* prompt is a 409 (per the contract). Scanning
    // the log is fine here (admissions are infrequent); a projection/index is a later optimization.
    if let Some(id) = request.id.as_deref() {
        let log = state
            .ctx
            .event_store()
            .read(&session_id, 0)
            .await
            .map_err(|e| SessionPromptError::Internal(e.to_string()))?;
        if let Some(existing) = log.iter().find(|e| {
            e.kind == "session.next.prompt.admitted.1"
                && e.data.get("messageID").and_then(|v| v.as_str()) == Some(id)
        }) {
            if existing.data.get("prompt") == Some(&prompt_value)
                && existing.data.get("delivery") == Some(&delivery_value)
            {
                tracing::info!(session = %session_id, message = %id, "prompt admission replayed (idempotent)");
                let data = admitted_from_event(existing, &session_id)
                    .map_err(SessionPromptError::Internal)?;
                return Ok(Json(opencode_proto::SessionPromptResponse { data }));
            }
            tracing::warn!(session = %session_id, message = %id, "prompt admission conflict: a different prompt under an existing id");
            return Err(SessionPromptError::Conflict(format!(
                "message {id} was already admitted with a different prompt"
            )));
        }
    }

    let message_id = request
        .id
        .clone()
        .unwrap_or_else(|| format!("msg_{}", ulid::Ulid::new()));

    // Durably admit: append the lifecycle event; its aggregate sequence is `admittedSeq`. Retry on an
    // optimistic-concurrency conflict (the background runner appends to the same aggregate).
    let admitted_event = opencode_db::EventInput::new(
        "session.next.prompt.admitted.1",
        serde_json::json!({
            "timestamp": now,
            "sessionID": session_id,
            "messageID": message_id,
            "prompt": prompt_value,
            "delivery": delivery_value,
        }),
    );
    let mut admitted_seq = None;
    for _ in 0..8 {
        let head = state
            .ctx
            .event_store()
            .head_seq(&session_id)
            .await
            .map_err(|e| SessionPromptError::Internal(e.to_string()))?;
        match state
            .ctx
            .event_store()
            .append(&session_id, head, vec![admitted_event.clone()])
            .await
        {
            Ok(seq) => {
                admitted_seq = Some(seq);
                break;
            }
            Err(opencode_db::DbError::Conflict { .. }) => continue,
            Err(other) => return Err(SessionPromptError::Internal(other.to_string())),
        }
    }
    let admitted_seq = admitted_seq.ok_or_else(|| {
        SessionPromptError::Conflict(format!("session {session_id} is busy; retry"))
    })?;

    // Schedule the background turn(s) unless the caller opted out (`resume: false`).
    if request.resume != Some(false) {
        let live: Arc<dyn TurnRunner> = Arc::new(LiveTurnRunner {
            ctx: state.ctx.clone(),
            runner: state.runner.clone(),
        });
        state.coordinator.admit(
            live,
            session_id.clone(),
            AdmittedPrompt {
                model,
                prompt: prompt.text.clone(),
                system: Vec::new(),
                step_limit: default_step_limit(),
            },
        );
    }

    tracing::info!(session = %session_id, message = %message_id, admitted_seq, "prompt admitted");

    Ok(Json(opencode_proto::SessionPromptResponse {
        data: opencode_proto::SessionInputAdmitted {
            admitted_seq,
            id: message_id,
            session_id,
            prompt,
            delivery,
            time_created: now,
            promoted_seq: None,
        },
    }))
}

/// Reconstruct a [`opencode_proto::SessionInputAdmitted`] from a persisted
/// `session.next.prompt.admitted.1` event — used to replay an idempotent re-admission.
fn admitted_from_event(
    event: &opencode_db::StoredEvent,
    session_id: &str,
) -> Result<opencode_proto::SessionInputAdmitted, String> {
    let prompt = serde_json::from_value(event.data.get("prompt").cloned().unwrap_or_default())
        .map_err(|e| e.to_string())?;
    let delivery = serde_json::from_value(event.data.get("delivery").cloned().unwrap_or_default())
        .map_err(|e| e.to_string())?;
    Ok(opencode_proto::SessionInputAdmitted {
        admitted_seq: event.seq,
        id: event
            .data
            .get("messageID")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        session_id: session_id.to_string(),
        prompt,
        delivery,
        time_created: event
            .data
            .get("timestamp")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0),
        promoted_seq: None,
    })
}

/// Wrap an in-process [`opencode_effect::BusEvent`] as the contract live event envelope
/// (`{ id, type, properties }`) that `/api/event` delivers. The runner already emits the event's
/// `properties` payload as the bus event's `data`.
fn contract_event_payload(event: &opencode_effect::BusEvent) -> serde_json::Value {
    serde_json::json!({
        "id": format!("evt_{}", ulid::Ulid::new()),
        "type": event.kind.clone(),
        "properties": event.data.clone(),
    })
}

/// `GET /api/event` — native SSE stream of the Rust event bus (group `event`). Matches the golden
/// `v2.event.subscribe`: a `text/event-stream` of contract events. Every event is delivered under the
/// SSE event name `message` (the JSON `type` is the discriminator), carrying `{ id, type, properties }`.
#[utoipa::path(
    get,
    path = "/api/event",
    operation_id = "v2.event.subscribe",
    responses(
        (status = 200, description = "Success", content_type = "text/event-stream", body = String),
        (status = 400, description = "Bad request", body = opencode_proto::InvalidRequestError),
        (status = 401, description = "Unauthorized", body = opencode_proto::UnauthorizedError)
    ),
    tag = "events"
)]
async fn v2_event_subscribe(
    State(state): State<ServerState>,
) -> axum::response::Sse<
    impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>,
> {
    use futures::StreamExt;
    let stream = state.ctx.event_bus().subscribe().map(|event| {
        let data = serde_json::to_string(&contract_event_payload(&event))
            .unwrap_or_else(|_| "{}".to_string());
        Ok(axum::response::sse::Event::default()
            .event("message")
            .data(data))
    });
    axum::response::Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default())
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
        v2_session_prompt,
        project_list,
        project_current,
        v2_event_subscribe,
        session_abort
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
        opencode_proto::ProjectTime,
        opencode_proto::Prompt,
        opencode_proto::PromptSource,
        opencode_proto::PromptFileAttachment,
        opencode_proto::PromptAgentAttachment,
        opencode_proto::Delivery,
        opencode_proto::SessionInputAdmitted,
        opencode_proto::ConflictError
    )),
    tags(
        (name = "control", description = "Control-plane routes"),
        (name = "global", description = "Global control-plane routes"),
        (name = "instance", description = "Instance-scoped routes"),
        (name = "file", description = "File routes"),
        (name = "sessions", description = "Session routes"),
        (name = "project", description = "Project routes"),
        (name = "events", description = "Event stream routes")
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
        router = router.route("/session/{sessionID}/abort", post(session_abort));
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
        router = router.route("/api/session/{sessionID}/prompt", post(v2_session_prompt));
    }
    if state.routes.handles("event") {
        router = router.route("/api/event", get(v2_event_subscribe));
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
    // Internal Phase-4 proving route: admit a prompt + drive turns in a background task.
    if state.routes.handles("session-prompt") {
        router = router.route(
            "/_rust/session/{sessionID}/prompt",
            post(rust_session_prompt),
        );
        router = router.route("/_rust/session/{sessionID}/abort", post(rust_session_abort));
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
            coordinator: SessionCoordinator::default(),
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

    // ---- Phase 4: the public `/prompt` + `/event` cutover ----

    fn contract_state(url: String, root: std::path::PathBuf, session: &str) -> ServerState {
        let sessions = Arc::new(opencode_db::MemorySessionStore::new());
        sessions.insert(test_session_record(session));
        ServerState {
            ctx: AppContext::new(AppServices {
                sessions,
                ..Default::default()
            }),
            routes: RouteTable::parse("session,event"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices {
                engines: Arc::new(TestEngines { url }),
                gate: Arc::new(AllowAll),
                root,
            },
            coordinator: SessionCoordinator::default(),
        }
    }

    fn prompt_contract_request(session: &str, text: &str) -> axum::extract::Request {
        axum::extract::Request::builder()
            .method("POST")
            .uri(format!("/api/session/{session}/prompt"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({ "prompt": { "text": text } }).to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn v2_session_prompt_admits_and_schedules_the_runner() {
        use tower::ServiceExt;
        let url = spawn_anthropic(&[EXEC_TEXT_SSE]).await;
        let dir = tempfile::tempdir().unwrap();
        let state = contract_state(url, dir.path().to_path_buf(), "ses_cut");
        let mut bus = state.ctx.event_bus().subscribe();

        let resp = build_router(state.clone())
            .oneshot(prompt_contract_request("ses_cut", "hello"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Contract-faithful admission response (returned synchronously).
        assert_eq!(v["data"]["sessionID"], "ses_cut");
        assert_eq!(v["data"]["delivery"], "steer");
        assert!(v["data"]["id"].as_str().unwrap().starts_with("msg_"));
        assert_eq!(v["data"]["prompt"]["text"], "hello");
        assert_eq!(v["data"]["admittedSeq"], 1); // first event on the aggregate

        // The background run then appends its contract events (awaited on the bus).
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), bus.recv())
                .await
                .expect("a bus event before timeout")
                .expect("bus open");
            if event.kind == "session.next.step.ended" {
                break;
            }
        }
        // The admission is persisted first, then the run's events, under the session aggregate.
        let stored = state.ctx.event_store().read("ses_cut", 0).await.unwrap();
        assert_eq!(stored[0].kind, "session.next.prompt.admitted.1");
        assert!(stored.iter().any(|e| e.kind == "session.next.text.ended"));
    }

    #[tokio::test]
    async fn v2_session_prompt_is_idempotent_on_message_id() {
        use tower::ServiceExt;
        let url = spawn_anthropic(&[EXEC_TEXT_SSE]).await;
        let dir = tempfile::tempdir().unwrap();
        let state = contract_state(url, dir.path().to_path_buf(), "ses_idem");

        let post = |text: &str| {
            axum::extract::Request::builder()
                .method("POST")
                .uri("/api/session/ses_idem/prompt")
                .header("content-type", "application/json")
                .body(axum::body::Body::from(
                    serde_json::json!({ "id": "msg_fixed", "prompt": { "text": text } })
                        .to_string(),
                ))
                .unwrap()
        };

        // First admission lands at seq 1 (and schedules the background run).
        let r1 = build_router(state.clone())
            .oneshot(post("hello"))
            .await
            .unwrap();
        assert_eq!(r1.status(), 200);
        let b1 = axum::body::to_bytes(r1.into_body(), usize::MAX)
            .await
            .unwrap();
        let v1: serde_json::Value = serde_json::from_slice(&b1).unwrap();
        assert_eq!(v1["data"]["admittedSeq"], 1);

        // Re-sent with the same id + prompt → idempotent replay (same seq, no new admit).
        let r2 = build_router(state.clone())
            .oneshot(post("hello"))
            .await
            .unwrap();
        assert_eq!(r2.status(), 200);
        let b2 = axum::body::to_bytes(r2.into_body(), usize::MAX)
            .await
            .unwrap();
        let v2: serde_json::Value = serde_json::from_slice(&b2).unwrap();
        assert_eq!(v2["data"]["admittedSeq"], 1);

        // Same id, different prompt → 409 ConflictError.
        let r3 = build_router(state.clone())
            .oneshot(post("different"))
            .await
            .unwrap();
        assert_eq!(r3.status(), 409);
        let b3 = axum::body::to_bytes(r3.into_body(), usize::MAX)
            .await
            .unwrap();
        let v3: serde_json::Value = serde_json::from_slice(&b3).unwrap();
        assert_eq!(v3["_tag"], "ConflictError");

        // Exactly one admission was persisted (the replay didn't append a duplicate).
        let stored = state.ctx.event_store().read("ses_idem", 0).await.unwrap();
        let admits = stored
            .iter()
            .filter(|e| e.kind == "session.next.prompt.admitted.1")
            .count();
        assert_eq!(admits, 1);
    }

    #[tokio::test]
    async fn v2_session_prompt_404_for_unknown_session() {
        use tower::ServiceExt;
        let state = ServerState {
            ctx: AppContext::in_memory(), // no session inserted
            routes: RouteTable::parse("session"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(state)
            .oneshot(prompt_contract_request("ses_missing", "hi"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["_tag"], "SessionNotFoundError");
    }

    #[tokio::test]
    async fn v2_session_prompt_is_gated_behind_session() {
        use tower::ServiceExt;
        let state = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse(""), // group off → proxied → 502 (upstream down)
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(state)
            .oneshot(prompt_contract_request("ses_x", "hi"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }

    #[test]
    fn contract_event_payload_wraps_as_live_envelope() {
        let event = opencode_effect::BusEvent::for_aggregate(
            "session.next.step.started",
            "ses_e",
            serde_json::json!({ "sessionID": "ses_e", "assistantMessageID": "msg_1" }),
        );
        let payload = contract_event_payload(&event);
        assert_eq!(payload["type"], "session.next.step.started");
        assert_eq!(payload["properties"]["sessionID"], "ses_e");
        assert!(payload["id"].as_str().unwrap().starts_with("evt_"));
    }

    #[tokio::test]
    async fn v2_event_is_gated_behind_event() {
        use tower::ServiceExt;
        let state = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse(""), // group off → proxied → 502
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/event")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }

    #[test]
    fn openapi_has_v2_session_prompt_and_event_operations() {
        let json = serde_json::to_value(openapi_document()).unwrap();
        let prompt = &json["paths"]["/api/session/{sessionID}/prompt"]["post"];
        assert_eq!(prompt["operationId"], "v2.session.prompt");
        assert_eq!(
            prompt["responses"]["200"]["content"]["application/json"]["schema"]["properties"]
                ["data"]["$ref"],
            "#/components/schemas/SessionInputAdmitted"
        );
        for code in ["400", "401", "404", "409"] {
            assert!(
                prompt["responses"][code].is_object(),
                "prompt missing {code}"
            );
        }
        let event = &json["paths"]["/api/event"]["get"];
        assert_eq!(event["operationId"], "v2.event.subscribe");
        assert!(event["responses"]["200"]["content"]["text/event-stream"].is_object());
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
            coordinator: SessionCoordinator::default(),
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
            coordinator: SessionCoordinator::default(),
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
            coordinator: SessionCoordinator::default(),
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
            coordinator: SessionCoordinator::default(),
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
            coordinator: SessionCoordinator::default(),
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
            coordinator: SessionCoordinator::default(),
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
            coordinator: SessionCoordinator::default(),
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
            coordinator: SessionCoordinator::default(),
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
            coordinator: SessionCoordinator::default(),
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
            coordinator: SessionCoordinator::default(),
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
            coordinator: SessionCoordinator::default(),
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
            coordinator: SessionCoordinator::default(),
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
            routes: RouteTable::parse("session-exec,session-prompt"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices {
                engines: Arc::new(TestEngines { url }),
                gate: Arc::new(AllowAll),
                root,
            },
            coordinator: SessionCoordinator::default(),
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

        // Persisted under the session aggregate (contract text-turn lifecycle events).
        let stored = state.ctx.event_store().read("ses_exec", 0).await.unwrap();
        let kinds: Vec<&str> = stored.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "session.next.step.started",
                "session.next.text.started",
                "session.next.text.ended",
                "session.next.step.ended",
            ]
        );
        // The text block carries the assistant's answer.
        assert!(stored[2].data["text"].as_str().unwrap().contains("sunny"));

        // Announced on the bus.
        let mut seen = Vec::new();
        while let Ok(ev) = bus.try_recv() {
            seen.push(ev.kind);
        }
        assert!(seen
            .iter()
            .any(|k| k.as_str() == "session.next.step.started"));
        assert!(seen.iter().any(|k| k.as_str() == "session.next.step.ended"));
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
                // turn 0: the tool step
                "session.next.step.started",
                "session.next.tool.input.started",
                "session.next.tool.input.ended",
                "session.next.tool.called",
                "session.next.tool.success",
                "session.next.step.ended",
                // turn 1: the text-completion step
                "session.next.step.started",
                "session.next.text.started",
                "session.next.text.ended",
                "session.next.step.ended",
            ]
        );
        // The bash tool actually ran in the toolbox root; its stdout is in the tool.success result.
        let result = stored[4].data["result"].as_str().unwrap();
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
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(state)
            .oneshot(exec_request("ses_x", "anthropic/x", "hi"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }

    // ---- Phase 4: the background session coordinator ----

    fn admitted(text: &str) -> AdmittedPrompt {
        AdmittedPrompt {
            model: "anthropic/x".to_string(),
            prompt: text.to_string(),
            system: Vec::new(),
            step_limit: 1,
        }
    }

    /// A [`TurnRunner`] that reports each completed prompt on a channel.
    struct ReportingRunner {
        done: tokio::sync::mpsc::UnboundedSender<String>,
    }

    #[async_trait]
    impl TurnRunner for ReportingRunner {
        async fn run(&self, _session_id: &str, prompt: AdmittedPrompt, _cancel: Arc<AtomicBool>) {
            let _ = self.done.send(prompt.prompt);
        }
    }

    /// A [`TurnRunner`] that signals when each prompt starts and blocks until released, so a test can
    /// deterministically admit a second prompt while the first is mid-run.
    struct BlockingRunner {
        started: tokio::sync::mpsc::UnboundedSender<String>,
        done: tokio::sync::mpsc::UnboundedSender<String>,
        release: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait]
    impl TurnRunner for BlockingRunner {
        async fn run(&self, _session_id: &str, prompt: AdmittedPrompt, _cancel: Arc<AtomicBool>) {
            let _ = self.started.send(prompt.prompt.clone());
            let _permit = self.release.acquire().await.unwrap();
            let _ = self.done.send(prompt.prompt);
        }
    }

    /// A [`TurnRunner`] that simulates a long run: signals start, cooperatively polls the cancel token
    /// (as `run_gated` does at step boundaries) until it's set, then signals stop.
    struct CancelAwareRunner {
        started: tokio::sync::mpsc::UnboundedSender<()>,
        stopped: tokio::sync::mpsc::UnboundedSender<()>,
    }

    #[async_trait]
    impl TurnRunner for CancelAwareRunner {
        async fn run(&self, _session_id: &str, _prompt: AdmittedPrompt, cancel: Arc<AtomicBool>) {
            let _ = self.started.send(());
            while !cancel.load(std::sync::atomic::Ordering::SeqCst) {
                tokio::task::yield_now().await;
            }
            let _ = self.stopped.send(());
        }
    }

    #[tokio::test]
    async fn coordinator_cancel_stops_a_running_session() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (stopped_tx, mut stopped_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let runner: Arc<dyn TurnRunner> = Arc::new(CancelAwareRunner {
            started: started_tx,
            stopped: stopped_tx,
        });
        let coord = SessionCoordinator::default();

        coord.admit(runner, "ses".to_string(), admitted("p1"));
        started_rx.recv().await.unwrap(); // the run started and is polling the cancel token

        assert!(coord.cancel("ses")); // a running session is found and signalled
        stopped_rx.recv().await.unwrap(); // the runner observed the token and stopped

        // Cancelling an unknown session is a no-op.
        assert!(!coord.cancel("ses_absent"));
    }

    #[tokio::test]
    async fn abort_route_acks_and_is_gated() {
        use tower::ServiceExt;
        let abort_request = |session: &str| {
            axum::extract::Request::builder()
                .method("POST")
                .uri(format!("/_rust/session/{session}/abort"))
                .body(axum::body::Body::empty())
                .unwrap()
        };

        // Group enabled → 200 ack; no running session ⇒ cancelled=false.
        let on = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse("session-prompt"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(on)
            .oneshot(abort_request("ses_x"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["cancelled"], false);

        // Group disabled → proxied → 502.
        let off = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse(""),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(off)
            .oneshot(abort_request("ses_x"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }

    #[tokio::test]
    async fn session_abort_contract_route_acks_and_is_gated() {
        use tower::ServiceExt;
        let req = || {
            axum::extract::Request::builder()
                .method("POST")
                .uri("/session/ses_x/abort")
                .body(axum::body::Body::empty())
                .unwrap()
        };
        // Group `instance` enabled → 200 `true` (the contract's boolean ack).
        let on = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse("instance"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(on).oneshot(req()).await.unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v, serde_json::json!(true));
        // Group disabled → proxied → 502.
        let off = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse(""),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(off).oneshot(req()).await.unwrap();
        assert_eq!(resp.status(), 502);
    }

    #[test]
    fn openapi_has_session_abort_contract_operation() {
        let json = serde_json::to_value(openapi_document()).unwrap();
        let op = &json["paths"]["/session/{sessionID}/abort"]["post"];
        assert_eq!(op["operationId"], "session.abort");
        assert_eq!(
            op["responses"]["200"]["content"]["application/json"]["schema"]["type"],
            "boolean"
        );
        assert!(op["responses"]["400"].is_object());
    }

    #[tokio::test]
    async fn coordinator_spawns_drains_and_respawns_after_idle() {
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let runner: Arc<dyn TurnRunner> = Arc::new(ReportingRunner { done: done_tx });
        let coord = SessionCoordinator::default();

        coord.admit(runner.clone(), "ses".to_string(), admitted("p1"));
        assert_eq!(done_rx.recv().await.unwrap(), "p1");
        assert_eq!(coord.spawn_count(), 1);

        // The first task drained to empty and released; a later admit spawns a fresh task.
        coord.admit(runner, "ses".to_string(), admitted("p2"));
        assert_eq!(done_rx.recv().await.unwrap(), "p2");
        assert_eq!(coord.spawn_count(), 2);
    }

    #[tokio::test]
    async fn coordinator_coalesces_admits_while_running() {
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let runner: Arc<dyn TurnRunner> = Arc::new(BlockingRunner {
            started: started_tx,
            done: done_tx,
            release: release.clone(),
        });
        let coord = SessionCoordinator::default();

        coord.admit(runner.clone(), "ses".to_string(), admitted("p1"));
        assert_eq!(started_rx.recv().await.unwrap(), "p1"); // p1 is mid-run; the task owns the session

        // Admitted while the task owns the session → coalesced into the same task, no new spawn.
        coord.admit(runner, "ses".to_string(), admitted("p2"));
        assert_eq!(coord.spawn_count(), 1);

        release.add_permits(2); // let both finish
        assert_eq!(done_rx.recv().await.unwrap(), "p1");
        assert_eq!(done_rx.recv().await.unwrap(), "p2");
        assert_eq!(coord.spawn_count(), 1); // one task processed both
    }

    fn prompt_request(session: &str, model: &str, prompt: &str) -> axum::extract::Request {
        axum::extract::Request::builder()
            .method("POST")
            .uri(format!("/_rust/session/{session}/prompt"))
            .header("content-type", "application/json")
            .body(axum::body::Body::from(
                serde_json::json!({ "model": model, "prompt": prompt }).to_string(),
            ))
            .unwrap()
    }

    #[tokio::test]
    async fn prompt_runs_in_background_and_publishes() {
        use tower::ServiceExt;
        let url = spawn_anthropic(&[EXEC_TEXT_SSE]).await;
        let dir = tempfile::tempdir().unwrap();
        let state = exec_state(url, dir.path().to_path_buf());
        let mut bus = state.ctx.event_bus().subscribe();

        let resp = build_router(state.clone())
            .oneshot(prompt_request(
                "ses_bg",
                "anthropic/claude-haiku-4-5-20251001",
                "hello",
            ))
            .await
            .unwrap();
        // Admitted immediately (the turn runs in the background).
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], "admitted");

        // Await the background turn's events on the bus.
        let mut saw_text = false;
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(5), bus.recv())
                .await
                .expect("a bus event before timeout")
                .expect("bus open");
            if event.kind == "session.next.text.ended" {
                saw_text = true;
            }
            if event.kind == "session.next.step.ended" {
                break;
            }
        }
        assert!(saw_text);

        // Persisted under the session aggregate too (contract text-turn lifecycle events).
        let stored = state.ctx.event_store().read("ses_bg", 0).await.unwrap();
        let kinds: Vec<&str> = stored.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(
            kinds,
            vec![
                "session.next.step.started",
                "session.next.text.started",
                "session.next.text.ended",
                "session.next.step.ended",
            ]
        );
    }

    #[tokio::test]
    async fn prompt_is_gated_behind_session_prompt() {
        use tower::ServiceExt;
        // `session-prompt` not enabled → route not registered → proxy fallback → 502.
        let state = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse(""),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(state)
            .oneshot(prompt_request("ses_x", "anthropic/x", "hi"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }
}
