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
    routing::{get, patch, post},
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
    let started = std::time::Instant::now();
    let result = run_gated(
        engine.as_ref(),
        tools,
        &sink,
        runner.gate.as_ref(),
        &session,
        vec![Message::user_text(prompt)],
        cancel,
    )
    .await;
    let latency_ms = started.elapsed().as_millis() as u64;
    match &result {
        Ok(run) => {
            let steps = match run.outcome {
                SessionOutcome::Completed { steps }
                | SessionOutcome::StepLimitReached { steps }
                | SessionOutcome::AwaitingPermission { steps }
                | SessionOutcome::Cancelled { steps } => steps as u64,
            };
            ctx.metrics().record_turn(steps, latency_ms);
            if matches!(run.outcome, SessionOutcome::Cancelled { .. }) {
                ctx.metrics().record_cancellation();
            }
            // Project the finished turn into the `session_message` timeline so `v2.session.messages`
            // returns real data (the native write path; see `persist_timeline`).
            persist_timeline(
                ctx,
                session_id,
                provider.as_str(),
                model_id,
                &run.messages,
                &run.usage,
            )
            .await;
        }
        Err(_) => ctx.metrics().record_error(),
    }
    result.map_err(|e| e.to_string())
}

/// Split a typed [`opencode_proto::SessionMessage`] back into the `session_message` row columns
/// `(id, type, data)` — the inverse of [`row_to_session_message`] — so the store can persist it.
fn split_session_message(
    msg: &opencode_proto::SessionMessage,
) -> Result<(String, String, serde_json::Value), serde_json::Error> {
    let mut value = serde_json::to_value(msg)?;
    let obj = value
        .as_object_mut()
        .expect("a SessionMessage always serializes to a JSON object");
    let id = obj
        .remove("id")
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    let kind = obj
        .remove("type")
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default();
    Ok((id, kind, serde_json::Value::Object(std::mem::take(obj))))
}

/// Project a finished turn's conversation into the `session_message` timeline and append each entry.
///
/// Native write path: the Rust runner owns the turn it just executed, so it writes the timeline rows
/// directly (via [`opencode_core::session_timeline::project_turn`] + `append`) rather than relying on
/// the TS event-fold projector. Coexistence caveat: if a TypeScript server is *also* projecting the
/// same event stream, both would write — run only one projector when both servers are live (a Rust-only
/// deployment is the target). Append failures are logged, not fatal: the turn already succeeded.
async fn persist_timeline(
    ctx: &AppContext,
    session_id: &str,
    provider: &str,
    model_id: &str,
    messages: &[opencode_llm::Message],
    usage: &opencode_llm::Usage,
) {
    let agent = ctx
        .sessions()
        .get(session_id)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.agent)
        .unwrap_or_else(|| "build".to_string());
    let model = opencode_proto::ModelRef {
        id: model_id.to_string(),
        provider_id: provider.to_string(),
        variant: None,
    };
    let entries = opencode_core::session_timeline::project_turn(
        &agent,
        &model,
        messages,
        usage,
        now_ms(),
        || format!("msg_{}", ulid::Ulid::new()),
    );
    for entry in entries {
        match split_session_message(&entry) {
            Ok((id, kind, data)) => {
                if let Err(error) = ctx
                    .session_messages()
                    .append(session_id, &id, &kind, data)
                    .await
                {
                    tracing::warn!(session = session_id, %error, "failed to persist timeline entry");
                }
            }
            Err(error) => {
                tracing::warn!(session = session_id, %error, "failed to encode timeline entry")
            }
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

/// Always-native internal metrics snapshot (`/_rust/metrics`, not part of the contract): the runner's
/// in-process counters + turn-latency percentiles + total events published on the bus, as JSON.
async fn rust_metrics(State(state): State<ServerState>) -> Json<serde_json::Value> {
    let m = state.ctx.metrics().snapshot();
    Json(serde_json::json!({
        "prompts_total": m.prompts,
        "turns_total": m.turns,
        "steps_total": m.steps,
        "errors_total": m.errors,
        "cancellations_total": m.cancellations,
        "events_total": state.ctx.event_bus().published_count(),
        "turn_latency_ms": {
            "count": m.latency_count,
            "p50": m.latency_p50_ms,
            "p95": m.latency_p95_ms,
            "p99": m.latency_p99_ms,
            "max": m.latency_max_ms,
        },
    }))
}

/// Always-native Prometheus scrape endpoint (`/metrics`, **not** part of the OpenAPI contract): the
/// same runner counters + turn-latency percentiles + bus events as [`rust_metrics`], rendered in the
/// Prometheus text exposition format. Carries no secrets, but it's ops-only — restrict at the network
/// layer if exposure matters.
async fn prometheus_metrics(State(state): State<ServerState>) -> impl axum::response::IntoResponse {
    let body = state
        .ctx
        .metrics()
        .snapshot()
        .render_prometheus(state.ctx.event_bus().published_count());
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        body,
    )
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
    state.ctx.metrics().record_prompt();
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
    state.ctx.metrics().record_prompt();
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

/// Default page size when `limit` is omitted (mirrors TS `DefaultMessagesLimit`).
const DEFAULT_MESSAGES_LIMIT: i64 = 50;

/// Decoded payload of the opaque `v2.session.messages` cursor: `base64url(JSON)` of `{ id, order,
/// direction }` (mirrors the TS `Cursor` in `handlers/message.ts`). Rust-native bytes (not
/// byte-interchangeable with TS cursors mid-pagination — a documented rollback caveat); the paging
/// *behavior* matches TS (`session.ts` seq window).
#[derive(serde::Serialize, serde::Deserialize)]
struct MessageCursorPayload {
    id: String,
    order: String,
    direction: String,
}

fn encode_message_cursor(id: &str, order: &str, direction: &str) -> String {
    use base64::Engine;
    let json = serde_json::to_vec(&MessageCursorPayload {
        id: id.to_string(),
        order: order.to_string(),
        direction: direction.to_string(),
    })
    .unwrap_or_default();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

fn decode_message_cursor(raw: &str) -> Result<MessageCursorPayload, MessagesFailure> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(raw)
        .map_err(|_| MessagesFailure::InvalidCursor("Invalid cursor".to_string()))?;
    serde_json::from_slice(&bytes)
        .map_err(|_| MessagesFailure::InvalidCursor("Invalid cursor".to_string()))
}

/// Reconstruct a typed `SessionMessage` from a stored row by merging its discriminator + id back into
/// the JSON `data` (`{ ...data, id, type }`) — the inverse of how the projector stores it.
fn row_to_session_message(
    row: opencode_db::SessionMessageRow,
) -> Result<opencode_proto::SessionMessage, serde_json::Error> {
    let mut obj = match row.data {
        serde_json::Value::Object(m) => m,
        other => {
            let mut m = serde_json::Map::new();
            // A non-object `data` can't carry the variant fields; surface it under a key so the decode
            // fails loudly rather than silently dropping content.
            m.insert("data".to_string(), other);
            m
        }
    };
    obj.insert("id".to_string(), serde_json::Value::String(row.id));
    obj.insert("type".to_string(), serde_json::Value::String(row.kind));
    serde_json::from_value(serde_json::Value::Object(obj))
}

/// Error responder for `v2.session.messages`: the golden 400 union (`InvalidCursorError` /
/// `InvalidRequestError`), 404 `SessionNotFoundError`, or 500 `UnknownError`.
pub enum MessagesFailure {
    /// Invalid query parameter (400 `InvalidRequestError`).
    BadRequest(String),
    /// Bad/incompatible cursor (400 `InvalidCursorError`).
    InvalidCursor(String),
    /// No such session (404 `SessionNotFoundError`).
    NotFound(String),
    /// Store read / decode failed (500 `UnknownError`).
    Internal(String),
}

impl axum::response::IntoResponse for MessagesFailure {
    fn into_response(self) -> axum::response::Response {
        match self {
            MessagesFailure::BadRequest(message) => (
                axum::http::StatusCode::BAD_REQUEST,
                Json(opencode_proto::InvalidRequestError {
                    tag: "InvalidRequestError".to_string(),
                    message,
                    kind: None,
                    field: None,
                }),
            )
                .into_response(),
            MessagesFailure::InvalidCursor(message) => (
                axum::http::StatusCode::BAD_REQUEST,
                Json(opencode_proto::InvalidCursorError {
                    tag: "InvalidCursorError".to_string(),
                    message,
                }),
            )
                .into_response(),
            MessagesFailure::NotFound(session_id) => (
                axum::http::StatusCode::NOT_FOUND,
                Json(opencode_proto::SessionNotFoundError {
                    tag: "SessionNotFoundError".to_string(),
                    session_id: session_id.clone(),
                    message: format!("Session not found: {session_id}"),
                }),
            )
                .into_response(),
            MessagesFailure::Internal(message) => (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(opencode_proto::UnknownError {
                    tag: "UnknownError".to_string(),
                    message,
                    reference: None,
                }),
            )
                .into_response(),
        }
    }
}

/// `GET /api/session/{sessionID}/message` — the projected message timeline (group `session`). Matches
/// the golden `v2.session.messages`: 200 `SessionMessagesResponse`, 400 union, 401, 404, 500. Reads the
/// `session_message` projection with the same seq-windowed cursor paging as TS `V2Session.messages`
/// (`packages/core/src/session.ts`) and reconstructs each typed `SessionMessage` from its row.
#[utoipa::path(
    get,
    path = "/api/session/{sessionID}/message",
    operation_id = "v2.session.messages",
    params(
        ("sessionID" = String, Path, description = "Session id"),
        ("limit" = Option<i64>, Query, description = "Max results (default 50)"),
        ("order" = Option<String>, Query, description = "asc | desc (default desc); not with cursor"),
        ("cursor" = Option<String>, Query, description = "Opaque pagination cursor")
    ),
    responses(
        (status = 200, description = "Messages", body = opencode_proto::SessionMessagesResponse),
        (status = 400, description = "Bad request", body = opencode_proto::SessionListError),
        (status = 401, description = "Unauthorized", body = opencode_proto::UnauthorizedError),
        (status = 404, description = "Session not found", body = opencode_proto::SessionNotFoundError),
        (status = 500, description = "Server error", body = opencode_proto::UnknownError)
    ),
    tag = "session"
)]
async fn v2_session_messages(
    State(state): State<ServerState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<opencode_proto::SessionMessagesResponse>, MessagesFailure> {
    let pick = |key: &str| params.get(key).filter(|s| !s.is_empty()).cloned();
    let limit = match params.get("limit").filter(|s| !s.is_empty()) {
        None => DEFAULT_MESSAGES_LIMIT,
        Some(s) => s
            .parse::<i64>()
            .map_err(|_| MessagesFailure::BadRequest(format!("limit must be an integer: {s}")))?,
    };
    let order_q = pick("order");
    let cursor_raw = pick("cursor");
    // A cursor pins the order it was created with, so the two can't be combined (mirrors TS).
    if cursor_raw.is_some() && order_q.is_some() {
        return Err(MessagesFailure::InvalidCursor(
            "Cursor cannot be combined with order".to_string(),
        ));
    }
    if let Some(o) = &order_q {
        if o != "asc" && o != "desc" {
            return Err(MessagesFailure::BadRequest(format!(
                "order must be 'asc' or 'desc': {o}"
            )));
        }
    }
    let decoded = match &cursor_raw {
        Some(raw) => Some(decode_message_cursor(raw)?),
        None => None,
    };
    // Resolved order: cursor's order, else the query's, else newest-first.
    let order = decoded
        .as_ref()
        .map(|d| d.order.clone())
        .or(order_q)
        .unwrap_or_else(|| "desc".to_string());

    // The session must exist (TS `result.get` → NotFound) before we read its timeline.
    if state
        .ctx
        .sessions()
        .get(&session_id)
        .await
        .map_err(|e| MessagesFailure::Internal(e.to_string()))?
        .is_none()
    {
        return Err(MessagesFailure::NotFound(session_id));
    }

    // Seq window: paging "previous" flips the scan order, then the page is reversed back to `order`.
    let direction = decoded
        .as_ref()
        .map(|d| d.direction.clone())
        .unwrap_or_else(|| "next".to_string());
    let effective_order = match (direction.as_str(), order.as_str()) {
        ("previous", "asc") => opencode_db::MessageOrder::Desc,
        ("previous", _) => opencode_db::MessageOrder::Asc,
        (_, "asc") => opencode_db::MessageOrder::Asc,
        _ => opencode_db::MessageOrder::Desc,
    };
    let store = state.ctx.session_messages();
    // Resolve the cursor anchor → an exclusive seq bound (a cursor with no matching row ⇒ empty page).
    let (after_seq, before_seq, empty) = match &decoded {
        Some(d) => match store
            .seq_of(&session_id, &d.id)
            .await
            .map_err(|e| MessagesFailure::Internal(e.to_string()))?
        {
            None => (None, None, true),
            Some(seq) => match effective_order {
                opencode_db::MessageOrder::Asc => (Some(seq), None, false),
                opencode_db::MessageOrder::Desc => (None, Some(seq), false),
            },
        },
        None => (None, None, false),
    };
    let rows = if empty {
        Vec::new()
    } else {
        store
            .list(
                &session_id,
                after_seq,
                before_seq,
                effective_order,
                Some(limit),
            )
            .await
            .map_err(|e| MessagesFailure::Internal(e.to_string()))?
    };
    let rows: Vec<opencode_db::SessionMessageRow> = if direction == "previous" {
        rows.into_iter().rev().collect()
    } else {
        rows
    };

    // Adjacent-page cursors are the first/last ids in the *resolved* order (captured before decode).
    let first_id = rows.first().map(|r| r.id.clone());
    let last_id = rows.last().map(|r| r.id.clone());
    let mut data = Vec::with_capacity(rows.len());
    for row in rows {
        data.push(
            row_to_session_message(row).map_err(|e| MessagesFailure::Internal(e.to_string()))?,
        );
    }
    let cursor = opencode_proto::MessageCursor {
        previous: first_id.map(|id| encode_message_cursor(&id, &order, "previous")),
        next: last_id.map(|id| encode_message_cursor(&id, &order, "next")),
    };
    Ok(Json(opencode_proto::SessionMessagesResponse {
        data,
        cursor,
    }))
}

/// Error responder for `session.todo`: 404 `NotFoundError` (no such session) or a generic 500 (a store
/// failure; not a declared response, but axum needs a body). The declared 400 union is never produced
/// at runtime (the route takes no body/params), matching how the golden declares but rarely returns it.
pub enum TodoFailure {
    /// No such session (404 `NotFoundError`).
    NotFound(String),
    /// Store read failed (500).
    Internal(String),
}

impl axum::response::IntoResponse for TodoFailure {
    fn into_response(self) -> axum::response::Response {
        match self {
            TodoFailure::NotFound(session_id) => (
                axum::http::StatusCode::NOT_FOUND,
                Json(opencode_proto::NotFoundError {
                    name: "NotFoundError".to_string(),
                    data: opencode_proto::NotFoundData {
                        message: format!("Session not found: {session_id}"),
                    },
                }),
            )
                .into_response(),
            TodoFailure::Internal(message) => (
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

/// `GET /session/{sessionID}/todo` — a session's todo list (group `session`). Matches the golden
/// `session.todo`: 200 `[Todo]`, 400 union, 404 `NotFoundError`. Reads the `todo` table ordered by
/// position; 404s an unknown session.
#[utoipa::path(
    get,
    path = "/session/{sessionID}/todo",
    operation_id = "session.todo",
    params(("sessionID" = String, Path, description = "Session id")),
    responses(
        (status = 200, description = "Todo list", body = Vec<opencode_proto::Todo>),
        (status = 400, description = "Bad request", body = opencode_proto::RequestError),
        (status = 404, description = "Session not found", body = opencode_proto::NotFoundError)
    ),
    tag = "session"
)]
async fn session_todo(
    State(state): State<ServerState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Result<Json<Vec<opencode_proto::Todo>>, TodoFailure> {
    if state
        .ctx
        .sessions()
        .get(&session_id)
        .await
        .map_err(|e| TodoFailure::Internal(e.to_string()))?
        .is_none()
    {
        return Err(TodoFailure::NotFound(session_id));
    }
    let todos = state
        .ctx
        .todos()
        .list(&session_id)
        .await
        .map_err(|e| TodoFailure::Internal(e.to_string()))?
        .into_iter()
        .map(|t| opencode_proto::Todo {
            content: t.content,
            status: t.status,
            priority: t.priority,
        })
        .collect();
    Ok(Json(todos))
}

/// Map a full V1 session row to the `Session` wire shape, parsing the JSON columns
/// (`model`/`permission`/`revert`/`summary_diffs`) into their typed forms.
fn session_v1_from_record(r: opencode_db::SessionV1Record) -> opencode_proto::Session {
    let summary = r.summary.map(
        |(additions, deletions, files)| opencode_proto::SessionSummary {
            additions: additions as f64,
            deletions: deletions as f64,
            files: files as f64,
            diffs: r.summary_diffs.and_then(|v| serde_json::from_value(v).ok()),
        },
    );
    opencode_proto::Session {
        id: r.id,
        slug: r.slug,
        project_id: r.project_id,
        workspace_id: r.workspace_id,
        directory: r.directory,
        path: r.path,
        parent_id: r.parent_id,
        summary,
        cost: Some(r.cost),
        tokens: Some(opencode_proto::SessionTokens {
            input: r.tokens.0 as f64,
            output: r.tokens.1 as f64,
            reasoning: r.tokens.2 as f64,
            cache: opencode_proto::TokenCache {
                read: r.tokens.3 as f64,
                write: r.tokens.4 as f64,
            },
        }),
        share: r.share_url.map(|url| opencode_proto::SessionShare { url }),
        title: r.title,
        agent: r.agent,
        model: r.model.and_then(|v| serde_json::from_value(v).ok()),
        version: r.version,
        metadata: r.metadata,
        time: opencode_proto::SessionV1Time {
            created: r.time_created,
            updated: r.time_updated,
            compacting: r.time_compacting,
            archived: r.time_archived.map(|x| x as f64),
        },
        permission: r.permission.and_then(|v| serde_json::from_value(v).ok()),
        revert: r.revert.and_then(|v| serde_json::from_value(v).ok()),
    }
}

/// Request body for `session.update` (all fields optional; `None` leaves a field unchanged).
#[derive(Default, serde::Deserialize, utoipa::ToSchema)]
struct SessionUpdateBody {
    /// New title.
    #[serde(default)]
    title: Option<String>,
    /// Replacement metadata.
    #[serde(default)]
    #[schema(value_type = Object)]
    metadata: Option<serde_json::Value>,
    /// Replacement permission ruleset.
    #[serde(default)]
    #[schema(value_type = Object)]
    permission: Option<serde_json::Value>,
}

/// Error responder for `session.update`: 404 `NotFoundError`, or a generic 500. The declared 400 union
/// covers a malformed body (axum rejects it before the handler).
pub enum SessionMutateFailure {
    /// No such session (404 `NotFoundError`).
    NotFound(String),
    /// Store failure (500).
    Internal(String),
}

impl axum::response::IntoResponse for SessionMutateFailure {
    fn into_response(self) -> axum::response::Response {
        match self {
            SessionMutateFailure::NotFound(session_id) => (
                axum::http::StatusCode::NOT_FOUND,
                Json(opencode_proto::NotFoundError {
                    name: "NotFoundError".to_string(),
                    data: opencode_proto::NotFoundData {
                        message: format!("Session not found: {session_id}"),
                    },
                }),
            )
                .into_response(),
            SessionMutateFailure::Internal(message) => (
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

/// `PATCH /session/{sessionID}` — update a session's mutable fields (group `session`). Matches the
/// golden `session.update`: 200 `Session`, 400 union, 404 `NotFoundError`. Applies any of
/// `title`/`metadata`/`permission`, then returns the updated session.
#[utoipa::path(
    patch,
    path = "/session/{sessionID}",
    operation_id = "session.update",
    params(("sessionID" = String, Path, description = "Session id")),
    request_body = SessionUpdateBody,
    responses(
        (status = 200, description = "Updated session", body = opencode_proto::Session),
        (status = 400, description = "Bad request", body = opencode_proto::RequestError),
        (status = 404, description = "Session not found", body = opencode_proto::NotFoundError)
    ),
    tag = "session"
)]
async fn session_update(
    State(state): State<ServerState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    body: Option<Json<SessionUpdateBody>>,
) -> Result<Json<opencode_proto::Session>, SessionMutateFailure> {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let existed = state
        .ctx
        .sessions()
        .update(
            &session_id,
            body.title.as_deref(),
            body.metadata.as_ref(),
            body.permission.as_ref(),
        )
        .await
        .map_err(|e| SessionMutateFailure::Internal(e.to_string()))?;
    if !existed {
        return Err(SessionMutateFailure::NotFound(session_id));
    }
    let record = state
        .ctx
        .sessions()
        .get_full(&session_id)
        .await
        .map_err(|e| SessionMutateFailure::Internal(e.to_string()))?
        .ok_or_else(|| SessionMutateFailure::NotFound(session_id.clone()))?;
    Ok(Json(session_v1_from_record(record)))
}

/// Read the updated session and map it to the V1 `Session` wire shape (shared by the mutation routes).
async fn mutated_session(
    state: &ServerState,
    session_id: &str,
) -> Result<Json<opencode_proto::Session>, SessionMutateFailure> {
    let record = state
        .ctx
        .sessions()
        .get_full(session_id)
        .await
        .map_err(|e| SessionMutateFailure::Internal(e.to_string()))?
        .ok_or_else(|| SessionMutateFailure::NotFound(session_id.to_string()))?;
    Ok(Json(session_v1_from_record(record)))
}

/// Request body for `session.revert` (`{ messageID, partID? }`).
#[derive(serde::Deserialize, utoipa::ToSchema)]
struct RevertBody {
    /// The message to revert to.
    #[serde(rename = "messageID")]
    message_id: String,
    /// The part within the message, if finer-grained.
    #[serde(rename = "partID", default)]
    part_id: Option<String>,
}

/// `POST /session/{sessionID}/revert` — set the session's revert pointer (group `session`). Matches the
/// golden `session.revert`: 200 `Session`, 400 union, 404 `NotFoundError`, 409 `SessionBusyError`.
#[utoipa::path(
    post,
    path = "/session/{sessionID}/revert",
    operation_id = "session.revert",
    params(("sessionID" = String, Path, description = "Session id")),
    request_body = RevertBody,
    responses(
        (status = 200, description = "Reverted session", body = opencode_proto::Session),
        (status = 400, description = "Bad request", body = opencode_proto::RequestError),
        (status = 404, description = "Session not found", body = opencode_proto::NotFoundError),
        (status = 409, description = "Session busy", body = opencode_proto::SessionBusyError)
    ),
    tag = "session"
)]
async fn session_revert(
    State(state): State<ServerState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
    Json(body): Json<RevertBody>,
) -> Result<Json<opencode_proto::Session>, SessionMutateFailure> {
    let mut revert = serde_json::Map::new();
    revert.insert(
        "messageID".to_string(),
        serde_json::Value::String(body.message_id),
    );
    if let Some(part_id) = body.part_id {
        revert.insert("partID".to_string(), serde_json::Value::String(part_id));
    }
    let existed = state
        .ctx
        .sessions()
        .set_revert(&session_id, Some(&serde_json::Value::Object(revert)))
        .await
        .map_err(|e| SessionMutateFailure::Internal(e.to_string()))?;
    if !existed {
        return Err(SessionMutateFailure::NotFound(session_id));
    }
    mutated_session(&state, &session_id).await
}

/// `POST /session/{sessionID}/unrevert` — clear the session's revert pointer (group `session`).
/// Matches the golden `session.unrevert`: 200 `Session`, 400 union, 404 `NotFoundError`, 409
/// `SessionBusyError`.
#[utoipa::path(
    post,
    path = "/session/{sessionID}/unrevert",
    operation_id = "session.unrevert",
    params(("sessionID" = String, Path, description = "Session id")),
    responses(
        (status = 200, description = "Unreverted session", body = opencode_proto::Session),
        (status = 400, description = "Bad request", body = opencode_proto::RequestError),
        (status = 404, description = "Session not found", body = opencode_proto::NotFoundError),
        (status = 409, description = "Session busy", body = opencode_proto::SessionBusyError)
    ),
    tag = "session"
)]
async fn session_unrevert(
    State(state): State<ServerState>,
    axum::extract::Path(session_id): axum::extract::Path<String>,
) -> Result<Json<opencode_proto::Session>, SessionMutateFailure> {
    let existed = state
        .ctx
        .sessions()
        .set_revert(&session_id, None)
        .await
        .map_err(|e| SessionMutateFailure::Internal(e.to_string()))?;
    if !existed {
        return Err(SessionMutateFailure::NotFound(session_id));
    }
    mutated_session(&state, &session_id).await
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

/// Resolve the request `location` (the `Location.response` wrapper's `location` field) from the query.
/// Mirrors the TS location middleware's `ref()` (query `location[directory]` / `location[workspace]`,
/// else cwd), then resolves the project from the shared `project` store by worktree. Find-or-create and
/// the non-repo "global" fallback are documented follow-ups, so a directory with no known project is a
/// 400 here (the native model/provider groups are opt-in via `OPENCODE_RUST_ROUTES`; production proxies
/// to TypeScript, which owns project creation).
async fn resolve_location(
    state: &ServerState,
    params: &std::collections::HashMap<String, String>,
) -> Result<opencode_proto::LocationInfo, ApiError> {
    let directory = params
        .get("location[directory]")
        .or_else(|| params.get("directory"))
        .cloned()
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        });
    let workspace_id = params
        .get("location[workspace]")
        .or_else(|| params.get("workspace"))
        .cloned();
    let worktree = opencode_tools::git::root(std::path::Path::new(&directory))
        .await
        .ok()
        .flatten()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| directory.clone());
    let project = state
        .ctx
        .projects()
        .get_by_worktree(&worktree)
        .await
        .map_err(|e| {
            ApiError(opencode_effect::AppError::Other(anyhow::anyhow!(
                e.to_string()
            )))
        })?
        .ok_or_else(|| {
            ApiError(opencode_effect::AppError::BadRequest(format!(
                "no project for directory: {directory}"
            )))
        })?;
    Ok(opencode_proto::LocationInfo {
        directory,
        workspace_id,
        project: opencode_proto::LocationProject {
            id: project.id,
            directory: project.worktree,
        },
    })
}

/// The production env predicate for catalog gating: whether an environment variable is set. A provider
/// is enabled when one of its `env` keys is present (mirrors `packages/core/src/plugin/env.ts`).
fn env_present(name: &str) -> bool {
    std::env::var_os(name).is_some()
}

/// Build the `providerID → credentialID` map the catalog enabling consults (credential precedence over
/// env). Best-effort: a credential-store read failure degrades to env-only gating (logged), so the
/// catalog routes keep serving. Newest credential per integration wins (rows are oldest-first, so later
/// entries overwrite — matching the TS `active` map).
async fn credential_map(state: &ServerState) -> std::collections::BTreeMap<String, String> {
    match state.ctx.credentials().all().await {
        Ok(creds) => creds
            .into_iter()
            .map(|c| (c.integration_id, c.id))
            .collect(),
        Err(err) => {
            tracing::warn!(error = %err, "credential read failed; falling back to env-only gating");
            std::collections::BTreeMap::new()
        }
    }
}

/// `GET /api/model` — list models (group `model`). Matches the golden `v2.model.list`: 200
/// `{ location, data }` + 400/401/503. Returns the `available()` models from the in-memory models.dev
/// catalog ([`opencode_effect::AppContext::catalog`]) via [`opencode_core::catalog_v2`] — models of
/// enabled providers (a stored credential, or an `env` key set), ordered by release date (newest first,
/// mirroring the TS).
#[utoipa::path(
    get,
    path = "/api/model",
    operation_id = "v2.model.list",
    params(("location" = Option<String>, Query, description = "Location context (deepObject)")),
    responses(
        (status = 200, description = "Models", body = opencode_proto::ModelListResponse),
        (status = 400, description = "Bad request", body = opencode_proto::InvalidRequestError),
        (status = 401, description = "Unauthorized", body = opencode_proto::UnauthorizedError),
        (status = 503, description = "Catalog unavailable", body = opencode_proto::ServiceUnavailableError)
    ),
    tag = "model"
)]
async fn v2_model_list(
    State(state): State<ServerState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<opencode_proto::ModelListResponse>, ApiError> {
    let location = resolve_location(&state, &params).await?;
    let credentials = credential_map(&state).await;
    let catalog = state.ctx.catalog();
    let data = opencode_core::catalog_v2::available_models(&catalog, &env_present, &credentials);
    Ok(Json(opencode_proto::ModelListResponse { location, data }))
}

/// `GET /api/provider` — list providers (group `provider`). Matches the golden `v2.provider.list`: 200
/// `{ location, data }` + 400/401/503. Returns the `available()` providers from the in-memory catalog
/// via [`opencode_core::catalog_v2`] — those enabled by a stored credential (`{ via: "credential" }`,
/// precedence) or a set `env` key (`{ via: "env", name }`).
#[utoipa::path(
    get,
    path = "/api/provider",
    operation_id = "v2.provider.list",
    params(("location" = Option<String>, Query, description = "Location context (deepObject)")),
    responses(
        (status = 200, description = "Providers", body = opencode_proto::ProviderListResponse),
        (status = 400, description = "Bad request", body = opencode_proto::InvalidRequestError),
        (status = 401, description = "Unauthorized", body = opencode_proto::UnauthorizedError),
        (status = 503, description = "Catalog unavailable", body = opencode_proto::ServiceUnavailableError)
    ),
    tag = "provider"
)]
async fn v2_provider_list(
    State(state): State<ServerState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<opencode_proto::ProviderListResponse>, ApiError> {
    let location = resolve_location(&state, &params).await?;
    let credentials = credential_map(&state).await;
    let catalog = state.ctx.catalog();
    let data = opencode_core::catalog_v2::available_providers(&catalog, &env_present, &credentials);
    Ok(Json(opencode_proto::ProviderListResponse {
        location,
        data,
    }))
}

/// `GET /api/location` — resolve the request location (group `location`). Matches the golden
/// `v2.location.get`: 200 `LocationInfo` + 400/401. Returns [`resolve_location`]'s result directly
/// (no `{ location, data }` wrapper, unlike the list/get catalog routes).
#[utoipa::path(
    get,
    path = "/api/location",
    operation_id = "v2.location.get",
    params(("location" = Option<String>, Query, description = "Location context (deepObject)")),
    responses(
        (status = 200, description = "Resolved location", body = opencode_proto::LocationInfo),
        (status = 400, description = "Bad request", body = opencode_proto::InvalidRequestError),
        (status = 401, description = "Unauthorized", body = opencode_proto::UnauthorizedError)
    ),
    tag = "location"
)]
async fn v2_location_get(
    State(state): State<ServerState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<opencode_proto::LocationInfo>, ApiError> {
    let location = resolve_location(&state, &params).await?;
    Ok(Json(location))
}

/// Error responder for `v2.provider.get`: a 404 `ProviderNotFoundError`, a 400 `InvalidRequestError`
/// (from location resolution), or a generic 500 envelope.
enum ProviderGetError {
    /// No provider with that id (404).
    NotFound(String),
    /// The request location couldn't be resolved (400).
    BadRequest(String),
    /// Store/resolver failure (500).
    Internal(String),
}

impl axum::response::IntoResponse for ProviderGetError {
    fn into_response(self) -> axum::response::Response {
        use axum::http::StatusCode;
        match self {
            ProviderGetError::NotFound(id) => (
                StatusCode::NOT_FOUND,
                Json(opencode_proto::ProviderNotFoundError {
                    tag: "ProviderNotFoundError".to_string(),
                    message: format!("Provider not found: {id}"),
                    provider_id: id,
                }),
            )
                .into_response(),
            ProviderGetError::BadRequest(message) => (
                StatusCode::BAD_REQUEST,
                Json(opencode_proto::InvalidRequestError {
                    tag: "InvalidRequestError".to_string(),
                    message,
                    kind: None,
                    field: None,
                }),
            )
                .into_response(),
            ProviderGetError::Internal(message) => (
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

/// `GET /api/provider/{providerID}` — get one provider (group `provider`). Matches the golden
/// `v2.provider.get`: 200 `{ location, data }` + 400/401/404/503. Looks the provider up in the catalog
/// by id (unfiltered — a disabled provider is still returned, with its env-derived `enabled`); a missing
/// id is a 404 `ProviderNotFoundError`.
#[utoipa::path(
    get,
    path = "/api/provider/{providerID}",
    operation_id = "v2.provider.get",
    params(
        ("providerID" = String, Path, description = "Provider id"),
        ("location" = Option<String>, Query, description = "Location context (deepObject)")
    ),
    responses(
        (status = 200, description = "Provider", body = opencode_proto::ProviderGetResponse),
        (status = 400, description = "Bad request", body = opencode_proto::InvalidRequestError),
        (status = 401, description = "Unauthorized", body = opencode_proto::UnauthorizedError),
        (status = 404, description = "Provider not found", body = opencode_proto::ProviderNotFoundError),
        (status = 503, description = "Catalog unavailable", body = opencode_proto::ServiceUnavailableError)
    ),
    tag = "provider"
)]
async fn v2_provider_get(
    State(state): State<ServerState>,
    axum::extract::Path(provider_id): axum::extract::Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<opencode_proto::ProviderGetResponse>, ProviderGetError> {
    let location = resolve_location(&state, &params)
        .await
        .map_err(|e| match e.0 {
            opencode_effect::AppError::BadRequest(m) => ProviderGetError::BadRequest(m),
            other => ProviderGetError::Internal(other.to_string()),
        })?;
    let credentials = credential_map(&state).await;
    let catalog = state.ctx.catalog();
    let provider = catalog
        .get(&provider_id)
        .ok_or_else(|| ProviderGetError::NotFound(provider_id.clone()))?;
    let data = opencode_core::catalog_v2::provider_info(provider, &env_present, &credentials);
    Ok(Json(opencode_proto::ProviderGetResponse { location, data }))
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
        v2_session_messages,
        v2_session_prompt,
        session_todo,
        session_update,
        session_revert,
        session_unrevert,
        app_agents,
        command_list,
        config_get,
        global_config_get,
        global_dispose,
        instance_dispose,
        file_list,
        permission_list,
        question_list,
        vcs_get,
        project_list,
        project_current,
        v2_event_subscribe,
        session_abort,
        v2_model_list,
        v2_provider_list,
        v2_location_get,
        v2_provider_get
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
        opencode_proto::ConflictError,
        opencode_proto::ModelListResponse,
        opencode_proto::ProviderListResponse,
        opencode_proto::ProviderGetResponse,
        opencode_proto::ProviderNotFoundError,
        opencode_proto::ServiceUnavailableError,
        opencode_proto::ModelV2Info,
        opencode_proto::ModelApi,
        opencode_proto::ModelCapabilities,
        opencode_proto::ModelGeneration,
        opencode_proto::ModelRequest,
        opencode_proto::ModelVariant,
        opencode_proto::ModelCost,
        opencode_proto::ModelCostTier,
        opencode_proto::ModelCostCache,
        opencode_proto::ModelTime,
        opencode_proto::ModelLimit,
        opencode_proto::ModelStatus,
        opencode_proto::EffectNumber,
        opencode_proto::EffectNonFinite,
        opencode_proto::ProviderV2Info,
        opencode_proto::ProviderApi,
        opencode_proto::ProviderEnabled,
        opencode_proto::ProviderRequest,
        opencode_proto::LocationInfo,
        opencode_proto::LocationProject,
        opencode_proto::SessionMessagesResponse,
        opencode_proto::MessageCursor,
        opencode_proto::SessionMessage,
        opencode_proto::SessionMessageAssistantContent,
        opencode_proto::SessionMessageToolState,
        opencode_proto::ToolContent,
        opencode_proto::AssistantToolProvider,
        opencode_proto::AssistantSnapshot,
        opencode_proto::AssistantTokens,
        opencode_proto::SessionErrorUnknown,
        opencode_proto::MessageTime,
        opencode_proto::MessageTimeCompleted,
        opencode_proto::ToolTime,
        opencode_proto::UnknownError,
        opencode_proto::Todo,
        opencode_proto::NotFoundError,
        opencode_proto::NotFoundData,
        opencode_proto::FileNode,
        opencode_proto::PermissionRequest,
        opencode_proto::PermissionRequestTool,
        opencode_proto::QuestionRequest,
        opencode_proto::QuestionInfo,
        opencode_proto::QuestionOption,
        opencode_proto::QuestionTool,
        opencode_proto::VcsInfo,
        opencode_proto::Session,
        opencode_proto::SessionSummary,
        opencode_proto::SnapshotFileDiff,
        opencode_proto::SessionShare,
        opencode_proto::SessionV1Time,
        opencode_proto::SessionRevert,
        opencode_proto::PermissionRule,
        opencode_proto::PermissionAction,
        opencode_proto::SessionBusyError,
        opencode_proto::Agent,
        opencode_proto::AgentModel,
        opencode_proto::Command,
        opencode_proto::Config,
        opencode_proto::LogLevel,
        opencode_proto::LayoutConfig,
        opencode_proto::PolicyEffect,
        opencode_proto::ServerConfig,
        opencode_proto::AttachmentConfig,
        opencode_proto::ImageAttachmentConfig,
        opencode_proto::ConfigV2ReferenceGit,
        opencode_proto::ConfigV2ReferenceLocal,
        opencode_proto::ConfigV2ExperimentalPolicy,
        opencode_proto::PermissionActionConfig,
        opencode_proto::PermissionObjectConfig,
        opencode_proto::PermissionRuleConfig,
        opencode_proto::PermissionDetailedConfig,
        opencode_proto::PermissionConfig,
        opencode_proto::McpOAuthConfig,
        opencode_proto::McpOAuthSetting,
        opencode_proto::McpLocalConfig,
        opencode_proto::McpRemoteConfig,
        opencode_proto::AgentConfig,
        opencode_proto::TimeoutConfig,
        opencode_proto::ProviderOptionsConfig,
        opencode_proto::ProviderConfig,
        opencode_proto::ConfigSkills,
        opencode_proto::ConfigWatcher,
        opencode_proto::ConfigMode,
        opencode_proto::ConfigAgents,
        opencode_proto::ConfigEnterprise,
        opencode_proto::ConfigToolOutput,
        opencode_proto::ConfigCompaction,
        opencode_proto::ConfigExperimental,
        opencode_proto::AutoupdateConfig,
        opencode_proto::FormatterConfig,
        opencode_proto::LspConfig,
        opencode_proto::PluginEntry,
        SessionUpdateBody,
        RevertBody
    )),
    tags(
        (name = "control", description = "Control-plane routes"),
        (name = "global", description = "Global control-plane routes"),
        (name = "instance", description = "Instance-scoped routes"),
        (name = "file", description = "File routes"),
        (name = "sessions", description = "Session routes"),
        (name = "project", description = "Project routes"),
        (name = "events", description = "Event stream routes"),
        (name = "model", description = "Model catalog routes"),
        (name = "provider", description = "Provider catalog routes"),
        (name = "location", description = "Location routes"),
        (name = "config", description = "Configuration routes")
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

/// `GET /file` — list a directory's immediate entries (group `file`). Matches the golden `file.list`:
/// 200 `[FileNode]`, 400 `BadRequestError`. Lists `directory`/`path` via `opencode_tools`, flagging
/// gitignored entries; `path` defaults to the directory root, `directory` to the server cwd.
#[utoipa::path(
    get,
    path = "/file",
    operation_id = "file.list",
    params(
        ("directory" = Option<String>, Query, description = "Base directory (defaults to cwd)"),
        ("workspace" = Option<String>, Query, description = "Workspace id"),
        ("path" = Option<String>, Query, description = "Subpath within the directory to list")
    ),
    responses(
        (status = 200, description = "Files and directories", body = Vec<opencode_proto::FileNode>),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "file"
)]
async fn file_list(
    State(_state): State<ServerState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Vec<opencode_proto::FileNode>>, ApiBadRequest> {
    let base = params
        .get("directory")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        });
    let base = std::path::PathBuf::from(base);
    let sub = params.get("path").filter(|s| !s.is_empty()).cloned();
    let target = match &sub {
        Some(p) => base.join(p),
        None => base.clone(),
    };
    let nodes = opencode_tools::files::list_dir_nodes(&target)
        .map_err(|e| bad_request(format!("cannot list directory: {e}"), "Path"))?;
    let data = nodes
        .into_iter()
        .map(|n| {
            let rel = match &sub {
                Some(p) => format!("{}/{}", p.trim_end_matches('/'), n.name),
                None => n.name.clone(),
            };
            let absolute = target.join(&n.name).display().to_string();
            opencode_proto::FileNode {
                name: n.name,
                path: rel,
                absolute,
                kind: if n.is_dir { "directory" } else { "file" }.to_string(),
                ignored: n.ignored,
            }
        })
        .collect();
    Ok(Json(data))
}

/// Run `git -C <dir> <args>` and return trimmed stdout, or `None` on any failure (not a repo, git
/// missing, empty output). Best-effort so `vcs.get` degrades gracefully outside a repository.
async fn git_field(dir: &str, args: &[&str]) -> Option<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// `GET /vcs` — version-control info for a directory (group `instance`). Matches the golden `vcs.get`:
/// 200 `VcsInfo`, 400 `BadRequestError`. Best-effort `git` queries; fields are omitted outside a repo.
#[utoipa::path(
    get,
    path = "/vcs",
    operation_id = "vcs.get",
    params(
        ("directory" = Option<String>, Query, description = "Directory (defaults to cwd)"),
        ("workspace" = Option<String>, Query, description = "Workspace id")
    ),
    responses(
        (status = 200, description = "VCS info", body = opencode_proto::VcsInfo),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "instance"
)]
async fn vcs_get(
    State(_state): State<ServerState>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Json<opencode_proto::VcsInfo> {
    let dir = params
        .get("directory")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| {
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        });
    let branch = git_field(&dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .filter(|b| b != "HEAD"); // detached HEAD → no branch
    let default_branch = git_field(&dir, &["rev-parse", "--abbrev-ref", "origin/HEAD"])
        .await
        .map(|s| s.strip_prefix("origin/").unwrap_or(&s).to_string());
    Json(opencode_proto::VcsInfo {
        branch,
        default_branch,
    })
}

/// `GET /agent` — list available agents (group `instance`). Matches the golden `app.agents`: 200
/// `[Agent]`, 400 `BadRequestError`. Agent *loading* (built-in defaults + `.opencode/agents/*.md`) is a
/// follow-up; until then this is empty.
#[utoipa::path(
    get,
    path = "/agent",
    operation_id = "app.agents",
    params(
        ("directory" = Option<String>, Query, description = "Location context"),
        ("workspace" = Option<String>, Query, description = "Workspace id")
    ),
    responses(
        (status = 200, description = "List of agents", body = Vec<opencode_proto::Agent>),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "instance"
)]
async fn app_agents(State(_state): State<ServerState>) -> Json<Vec<opencode_proto::Agent>> {
    Json(Vec::new())
}

/// `GET /command` — list available commands (group `instance`). Matches the golden `command.list`: 200
/// `[Command]`, 400 `BadRequestError`. Command *loading* (built-in + `.opencode/command/*.md` + MCP/
/// skills) is a follow-up; until then this is empty.
#[utoipa::path(
    get,
    path = "/command",
    operation_id = "command.list",
    params(
        ("directory" = Option<String>, Query, description = "Location context"),
        ("workspace" = Option<String>, Query, description = "Workspace id")
    ),
    responses(
        (status = 200, description = "List of commands", body = Vec<opencode_proto::Command>),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "instance"
)]
async fn command_list(State(_state): State<ServerState>) -> Json<Vec<opencode_proto::Command>> {
    Json(Vec::new())
}

/// `GET /config` — the merged opencode configuration (group `config`). Matches the golden `config.get`:
/// 200 `Config`, 400 `BadRequestError`. Config *loading* (the 7-level merge of `opencode.json` etc.) is
/// a follow-up (PENDENCIAS #1); until then this returns the empty/default config.
#[utoipa::path(
    get,
    path = "/config",
    operation_id = "config.get",
    responses(
        (status = 200, description = "Config", body = opencode_proto::Config),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "config"
)]
async fn config_get(State(_state): State<ServerState>) -> Json<opencode_proto::Config> {
    Json(opencode_proto::Config::default())
}

/// `GET /global/config` — the global configuration (group `global`). Matches the golden
/// `global.config.get`: 200 `Config`, 400 `BadRequestError`. Loading is a follow-up (PENDENCIAS #1).
#[utoipa::path(
    get,
    path = "/global/config",
    operation_id = "global.config.get",
    responses(
        (status = 200, description = "Config", body = opencode_proto::Config),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "global"
)]
async fn global_config_get(State(_state): State<ServerState>) -> Json<opencode_proto::Config> {
    Json(opencode_proto::Config::default())
}

/// `GET /permission` — pending permission requests (group `permission`). Matches the golden
/// `permission.list`: 200 `[PermissionRequest]`, 400 `BadRequestError`. Pending requests are ephemeral
/// execution state; until the native runner produces them this is empty (no in-flight approvals).
#[utoipa::path(
    get,
    path = "/permission",
    operation_id = "permission.list",
    params(
        ("directory" = Option<String>, Query, description = "Location context"),
        ("workspace" = Option<String>, Query, description = "Workspace id")
    ),
    responses(
        (status = 200, description = "List of pending permissions", body = Vec<opencode_proto::PermissionRequest>),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "permission"
)]
async fn permission_list(
    State(_state): State<ServerState>,
) -> Json<Vec<opencode_proto::PermissionRequest>> {
    Json(Vec::new())
}

/// `GET /question` — pending question requests (group `question`). Matches the golden `question.list`:
/// 200 `[QuestionRequest]`, 400 `BadRequestError`. Like permissions, pending questions are ephemeral
/// execution state; empty until the native runner produces them.
#[utoipa::path(
    get,
    path = "/question",
    operation_id = "question.list",
    params(
        ("directory" = Option<String>, Query, description = "Location context"),
        ("workspace" = Option<String>, Query, description = "Workspace id")
    ),
    responses(
        (status = 200, description = "List of pending questions", body = Vec<opencode_proto::QuestionRequest>),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "question"
)]
async fn question_list(
    State(_state): State<ServerState>,
) -> Json<Vec<opencode_proto::QuestionRequest>> {
    Json(Vec::new())
}

/// `POST /global/dispose` — dispose the global runtime (group `global`). Matches the golden
/// `global.dispose`: 200 `boolean`, 400 `BadRequestError`. The Rust server holds no per-call global
/// state to tear down (DB pool + buses are process-scoped), so this acknowledges the lifecycle hook
/// with `true`.
#[utoipa::path(
    post,
    path = "/global/dispose",
    operation_id = "global.dispose",
    responses(
        (status = 200, description = "Global disposed", body = bool, content_type = "application/json"),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "global"
)]
async fn global_dispose() -> Json<bool> {
    Json(true)
}

/// `POST /instance/dispose` — dispose the instance (group `instance`). Matches the golden
/// `instance.dispose`: 200 `boolean`, 400 `BadRequestError`. As with `global.dispose`, the Rust server
/// has no per-call instance to dispose, so it acknowledges with `true`.
#[utoipa::path(
    post,
    path = "/instance/dispose",
    operation_id = "instance.dispose",
    responses(
        (status = 200, description = "Instance disposed", body = bool, content_type = "application/json"),
        (status = 400, description = "Bad request", body = opencode_proto::BadRequestError)
    ),
    tag = "instance"
)]
async fn instance_dispose() -> Json<bool> {
    Json(true)
}

/// Build the axum router: always-native liveness + cut-over contract routes + proxy fallback.
pub fn build_router(state: ServerState) -> Router {
    let mut router = Router::new()
        .route("/_rust/health", get(rust_health))
        .route("/_rust/event", get(rust_event))
        .route("/_rust/metrics", get(rust_metrics))
        .route("/metrics", get(prometheus_metrics));

    // Native contract routes are enabled here as they are cut over, gated by the route table.
    if state.routes.handles("health") {
        router = router.route("/health", get(health));
    }
    if state.routes.handles("global") {
        router = router.route("/global/health", get(global_health));
        router = router.route("/global/dispose", post(global_dispose));
        router = router.route("/global/config", get(global_config_get));
    }
    if state.routes.handles("config") {
        router = router.route("/config", get(config_get));
    }
    if state.routes.handles("instance") {
        router = router.route("/path", get(path_get));
        router = router.route("/session/{sessionID}/abort", post(session_abort));
        router = router.route("/instance/dispose", post(instance_dispose));
        router = router.route("/vcs", get(vcs_get));
        router = router.route("/agent", get(app_agents));
        router = router.route("/command", get(command_list));
    }
    if state.routes.handles("file") {
        router = router.route("/find/file", get(find_files));
        router = router.route("/find", get(find_text));
        router = router.route("/file", get(file_list));
    }
    if state.routes.handles("control") {
        router = router.route("/log", post(app_log));
    }
    if state.routes.handles("permission") {
        router = router.route("/permission", get(permission_list));
    }
    if state.routes.handles("question") {
        router = router.route("/question", get(question_list));
    }
    if state.routes.handles("session") {
        router = router.route("/api/session", get(v2_session_list));
        router = router.route("/api/session/{sessionID}", get(v2_session_get));
        router = router.route("/api/session/{sessionID}/message", get(v2_session_messages));
        router = router.route("/api/session/{sessionID}/prompt", post(v2_session_prompt));
        router = router.route("/session/{sessionID}/todo", get(session_todo));
        router = router.route("/session/{sessionID}", patch(session_update));
        router = router.route("/session/{sessionID}/revert", post(session_revert));
        router = router.route("/session/{sessionID}/unrevert", post(session_unrevert));
    }
    if state.routes.handles("event") {
        router = router.route("/api/event", get(v2_event_subscribe));
    }
    if state.routes.handles("project") {
        router = router.route("/project", get(project_list));
        router = router.route("/project/current", get(project_current));
    }
    if state.routes.handles("model") {
        router = router.route("/api/model", get(v2_model_list));
    }
    if state.routes.handles("provider") {
        router = router.route("/api/provider", get(v2_provider_list));
        router = router.route("/api/provider/{providerID}", get(v2_provider_get));
    }
    if state.routes.handles("location") {
        router = router.route("/api/location", get(v2_location_get));
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

    fn messages_state() -> ServerState {
        let sessions = Arc::new(opencode_db::MemorySessionStore::new());
        sessions.insert(test_session_record("ses_1"));
        let messages = Arc::new(opencode_db::MemorySessionMessageStore::new());
        // A user message then an assistant reply (data = SessionMessage minus id + type).
        messages.insert(opencode_db::SessionMessageRow {
            id: "msg_1".into(),
            session_id: "ses_1".into(),
            kind: "user".into(),
            seq: 1,
            data: serde_json::json!({ "time": { "created": 100 }, "text": "hi" }),
        });
        messages.insert(opencode_db::SessionMessageRow {
            id: "msg_2".into(),
            session_id: "ses_1".into(),
            kind: "assistant".into(),
            seq: 2,
            data: serde_json::json!({
                "time": { "created": 110 },
                "agent": "build",
                "model": { "id": "claude", "providerID": "anthropic" },
                "content": [{ "type": "text", "id": "c1", "text": "hello" }]
            }),
        });
        ServerState {
            ctx: AppContext::new(AppServices {
                sessions,
                session_messages: messages,
                ..Default::default()
            }),
            routes: RouteTable::parse("session"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        }
    }

    #[tokio::test]
    async fn v2_session_messages_returns_reconstructed_timeline() {
        use tower::ServiceExt;
        // Default order is desc (newest first): msg_2 then msg_1.
        let resp = build_router(messages_state())
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/session/ses_1/message")
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
        assert_eq!(v["data"][0]["id"], "msg_2");
        assert_eq!(v["data"][0]["type"], "assistant");
        assert_eq!(v["data"][0]["content"][0]["text"], "hello");
        assert_eq!(v["data"][1]["id"], "msg_1");
        assert_eq!(v["data"][1]["type"], "user");
        assert_eq!(v["data"][1]["text"], "hi");
        // Cursors are present (opaque tokens) when there's a page.
        assert!(v["cursor"]["previous"].is_string());
        assert!(v["cursor"]["next"].is_string());
    }

    #[tokio::test]
    async fn v2_session_messages_orders_ascending_and_limits() {
        use tower::ServiceExt;
        let resp = build_router(messages_state())
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/session/ses_1/message?order=asc&limit=1")
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
        // asc + limit 1 → just the oldest (msg_1).
        assert_eq!(v["data"].as_array().unwrap().len(), 1);
        assert_eq!(v["data"][0]["id"], "msg_1");
    }

    #[tokio::test]
    async fn v2_session_messages_missing_session_is_404() {
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
                    .uri("/api/session/ses_missing/message")
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
    }

    #[tokio::test]
    async fn v2_session_messages_rejects_cursor_with_order() {
        use tower::ServiceExt;
        let resp = build_router(messages_state())
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/session/ses_1/message?cursor=abc&order=asc")
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

    #[tokio::test]
    async fn session_todo_returns_ordered_list() {
        use tower::ServiceExt;
        let sessions = Arc::new(opencode_db::MemorySessionStore::new());
        sessions.insert(test_session_record("ses_1"));
        let todos = Arc::new(opencode_db::MemoryTodoStore::new());
        todos.insert(
            "ses_1",
            opencode_db::TodoRecord {
                content: "second".into(),
                status: "pending".into(),
                priority: "low".into(),
                position: 1,
            },
        );
        todos.insert(
            "ses_1",
            opencode_db::TodoRecord {
                content: "first".into(),
                status: "completed".into(),
                priority: "high".into(),
                position: 0,
            },
        );
        let state = ServerState {
            ctx: AppContext::new(AppServices {
                sessions,
                todos,
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
                    .uri("/session/ses_1/todo")
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
        assert_eq!(v[0]["content"], "first");
        assert_eq!(v[0]["status"], "completed");
        assert_eq!(v[1]["content"], "second");
    }

    #[tokio::test]
    async fn session_update_renames_and_returns_session() {
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
                    .method("PATCH")
                    .uri("/session/ses_1")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"title":"Renamed"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["id"], "ses_1");
        assert_eq!(v["title"], "Renamed");
        // The V1 shape is present (slug/version filled, time as integers).
        assert!(v["slug"].is_string());
        assert!(v["version"].is_string());
    }

    #[tokio::test]
    async fn session_revert_then_unrevert_round_trips() {
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
        // Revert sets the pointer.
        let resp = build_router(state.clone())
            .oneshot(
                axum::extract::Request::builder()
                    .method("POST")
                    .uri("/session/ses_1/revert")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"messageID":"msg_9"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v["revert"]["messageID"], "msg_9");
        // Unrevert clears it.
        let resp2 = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .method("POST")
                    .uri("/session/ses_1/unrevert")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp2.status(), 200);
        let v2: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(resp2.into_body(), usize::MAX)
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(v2.get("revert").is_none());
    }

    #[tokio::test]
    async fn session_update_missing_session_is_404() {
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
                    .method("PATCH")
                    .uri("/session/ses_missing")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"title":"x"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["name"], "NotFoundError");
    }

    #[tokio::test]
    async fn session_todo_missing_session_is_404_not_found() {
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
                    .uri("/session/ses_missing/todo")
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
        assert_eq!(v["name"], "NotFoundError");
        assert_eq!(v["data"]["message"], "Session not found: ses_missing");
    }

    #[tokio::test]
    async fn vcs_get_outside_repo_returns_empty_object() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap(); // a fresh temp dir is not a git repo
        let state = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse("instance"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let uri = format!("/vcs?directory={}", dir.path().display());
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri(&uri)
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
        // No repo → both fields omitted (object present, branch absent).
        assert!(v.is_object());
        assert!(v.get("branch").is_none());
    }

    #[tokio::test]
    async fn config_routes_return_an_object() {
        use tower::ServiceExt;
        for (group, uri) in [("config", "/config"), ("global", "/global/config")] {
            let state = ServerState {
                ctx: AppContext::in_memory(),
                routes: RouteTable::parse(group),
                proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
                runner: RunnerServices::default(),
                coordinator: SessionCoordinator::default(),
            };
            let resp = build_router(state)
                .oneshot(
                    axum::extract::Request::builder()
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "{uri}");
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            // Empty/default config until loading lands → a JSON object (all fields omitted).
            assert!(v.is_object(), "{uri}");
        }
    }

    #[tokio::test]
    async fn agent_and_command_lists_are_empty_until_loading_lands() {
        use tower::ServiceExt;
        for uri in ["/agent", "/command"] {
            let state = ServerState {
                ctx: AppContext::in_memory(),
                routes: RouteTable::parse("instance"),
                proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
                runner: RunnerServices::default(),
                coordinator: SessionCoordinator::default(),
            };
            let resp = build_router(state)
                .oneshot(
                    axum::extract::Request::builder()
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "{uri}");
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v, serde_json::json!([]), "{uri}");
        }
    }

    #[tokio::test]
    async fn permission_and_question_lists_are_empty_when_idle() {
        use tower::ServiceExt;
        for (group, uri) in [("permission", "/permission"), ("question", "/question")] {
            let state = ServerState {
                ctx: AppContext::in_memory(),
                routes: RouteTable::parse(group),
                proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
                runner: RunnerServices::default(),
                coordinator: SessionCoordinator::default(),
            };
            let resp = build_router(state)
                .oneshot(
                    axum::extract::Request::builder()
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "{uri}");
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v, serde_json::json!([]), "{uri}");
        }
    }

    #[tokio::test]
    async fn file_list_returns_directory_entries() {
        use tower::ServiceExt;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let state = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse("file"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let uri = format!("/file?directory={}", dir.path().display());
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri(&uri)
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
        let names: Vec<&str> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"a.txt"));
        assert!(names.contains(&"sub"));
        // The directory entry is typed accordingly.
        let sub = v
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["name"] == "sub")
            .unwrap();
        assert_eq!(sub["type"], "directory");
        assert_eq!(sub["ignored"], false);
    }

    #[tokio::test]
    async fn dispose_routes_acknowledge_with_true() {
        use tower::ServiceExt;
        for (group, uri) in [
            ("global", "/global/dispose"),
            ("instance", "/instance/dispose"),
        ] {
            let state = ServerState {
                ctx: AppContext::in_memory(),
                routes: RouteTable::parse(group),
                proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
                runner: RunnerServices::default(),
                coordinator: SessionCoordinator::default(),
            };
            let resp = build_router(state)
                .oneshot(
                    axum::extract::Request::builder()
                        .method("POST")
                        .uri(uri)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "{uri}");
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(v, serde_json::json!(true), "{uri}");
        }
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

    // ---- Phase 6: model/provider catalog routes ----

    /// A unique env key the sample catalog's provider gates on — set by the tests that exercise the
    /// `available()` path so the gating is deterministic regardless of the ambient environment.
    const TEST_ENV_KEY: &str = "OPENCODE_TEST_CATALOG_KEY";

    /// A small parsed catalog: one aisdk provider (`anthropic`, gated on [`TEST_ENV_KEY`]) with one
    /// model (no per-model hint).
    fn sample_catalog() -> opencode_config::catalog::Catalog {
        opencode_config::catalog::parse_catalog(
            r#"{ "anthropic": { "id":"anthropic","name":"Anthropic","env":["OPENCODE_TEST_CATALOG_KEY"],
                 "api":"https://api.anthropic.com","npm":"@ai-sdk/anthropic","models":{
                   "claude-x":{"id":"claude-x","name":"Claude X","release_date":"2026-01-01",
                     "tool_call":true,"limit":{"context":200000,"output":64000}} } } }"#,
        )
        .unwrap()
    }

    fn catalog_state(group: &str) -> ServerState {
        let projects = Arc::new(opencode_db::MemoryProjectStore::new());
        projects.insert(test_project_record("prj_1")); // worktree "/repo"
        ServerState {
            ctx: AppContext::new(AppServices {
                catalog: opencode_effect::catalog_handle(sample_catalog()),
                projects,
                ..Default::default()
            }),
            routes: RouteTable::parse(group),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        }
    }

    #[tokio::test]
    async fn v2_model_list_returns_available_models() {
        use tower::ServiceExt;
        // The provider gates on TEST_ENV_KEY; set it so the model is available (idempotent, unique key).
        std::env::set_var(TEST_ENV_KEY, "x");
        let resp = build_router(catalog_state("model"))
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/model?directory=/repo")
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
        // location resolved from the seeded project (worktree "/repo").
        assert_eq!(v["location"]["directory"], "/repo");
        assert_eq!(v["location"]["project"]["id"], "prj_1");
        // model projected from the catalog (no per-model hint → native, carrying the id).
        assert_eq!(v["data"][0]["id"], "claude-x");
        assert_eq!(v["data"][0]["providerID"], "anthropic");
        assert_eq!(v["data"][0]["api"]["type"], "native");
        assert_eq!(v["data"][0]["status"], "active");
        assert_eq!(v["data"][0]["enabled"], true);
    }

    #[tokio::test]
    async fn v2_provider_list_returns_available_providers() {
        use tower::ServiceExt;
        std::env::set_var(TEST_ENV_KEY, "x"); // enable the provider via env
        let resp = build_router(catalog_state("provider"))
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/provider?directory=/repo")
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
        assert_eq!(v["location"]["project"]["id"], "prj_1");
        assert_eq!(v["data"][0]["id"], "anthropic");
        // npm advertised → aisdk; env-enabled → `{ via: "env", name }`.
        assert_eq!(v["data"][0]["api"]["type"], "aisdk");
        assert_eq!(v["data"][0]["enabled"]["via"], "env");
        assert_eq!(v["data"][0]["enabled"]["name"], TEST_ENV_KEY);
    }

    #[tokio::test]
    async fn v2_provider_list_excludes_env_disabled_providers() {
        use tower::ServiceExt;
        // A provider gating on a never-set key is disabled → filtered out → empty `data`. Uses its own
        // catalog (and mutates no env) so it can't race the `set_var` tests above.
        let projects = Arc::new(opencode_db::MemoryProjectStore::new());
        projects.insert(test_project_record("prj_1"));
        let catalog = opencode_config::catalog::parse_catalog(
            r#"{ "anthropic": { "id":"anthropic","name":"Anthropic",
                 "env":["OPENCODE_DEFINITELY_UNSET_KEY_FOR_TEST"],"models":{} } }"#,
        )
        .unwrap();
        let state = ServerState {
            ctx: AppContext::new(AppServices {
                catalog: opencode_effect::catalog_handle(catalog),
                projects,
                ..Default::default()
            }),
            routes: RouteTable::parse("provider"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/provider?directory=/repo")
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
        assert_eq!(v["location"]["project"]["id"], "prj_1");
        assert_eq!(v["data"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn v2_model_list_proxies_when_group_disabled() {
        use tower::ServiceExt;
        let state = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse(""), // `model` not enabled → proxy fallback
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/model")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }

    #[tokio::test]
    async fn v2_location_get_resolves_from_project() {
        use tower::ServiceExt;
        // No catalog needed — location resolves the seeded project by worktree ("/repo").
        let projects = Arc::new(opencode_db::MemoryProjectStore::new());
        projects.insert(test_project_record("prj_1"));
        let state = ServerState {
            ctx: AppContext::new(AppServices {
                projects,
                ..Default::default()
            }),
            routes: RouteTable::parse("location"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/location?directory=/repo")
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
        // 200 body is the bare LocationInfo (no `{ location, data }` wrapper).
        assert_eq!(v["directory"], "/repo");
        assert_eq!(v["project"]["id"], "prj_1");
        assert_eq!(v["project"]["directory"], "/repo");
    }

    #[tokio::test]
    async fn v2_location_get_proxies_when_group_disabled() {
        use tower::ServiceExt;
        let state = ServerState {
            ctx: AppContext::in_memory(),
            routes: RouteTable::parse(""), // `location` not enabled → proxy fallback
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/location")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), 502);
    }

    /// State for the `v2.provider.get` tests: one provider (`anthropic`) gating on a never-set key, so
    /// `enabled` is deterministically `false` (and `provider.get` returns it anyway — it doesn't filter).
    fn provider_get_state() -> ServerState {
        let projects = Arc::new(opencode_db::MemoryProjectStore::new());
        projects.insert(test_project_record("prj_1"));
        let catalog = opencode_config::catalog::parse_catalog(
            r#"{ "anthropic": { "id":"anthropic","name":"Anthropic",
                 "env":["OPENCODE_DEFINITELY_UNSET_KEY_FOR_TEST"],"npm":"@ai-sdk/anthropic","models":{} } }"#,
        )
        .unwrap();
        ServerState {
            ctx: AppContext::new(AppServices {
                catalog: opencode_effect::catalog_handle(catalog),
                projects,
                ..Default::default()
            }),
            routes: RouteTable::parse("provider"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        }
    }

    #[tokio::test]
    async fn v2_provider_get_returns_the_provider() {
        use tower::ServiceExt;
        let resp = build_router(provider_get_state())
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/provider/anthropic?directory=/repo")
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
        // `{ location, data }` wrapper around a single provider; get returns it even though disabled.
        assert_eq!(v["location"]["project"]["id"], "prj_1");
        assert_eq!(v["data"]["id"], "anthropic");
        assert_eq!(v["data"]["api"]["type"], "aisdk");
        assert_eq!(v["data"]["enabled"], false);
    }

    #[tokio::test]
    async fn v2_provider_get_unknown_is_not_found() {
        use tower::ServiceExt;
        let resp = build_router(provider_get_state())
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/provider/ghost?directory=/repo")
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
        assert_eq!(v["_tag"], "ProviderNotFoundError");
        assert_eq!(v["providerID"], "ghost");
    }

    #[tokio::test]
    async fn v2_provider_list_includes_credential_enabled_providers() {
        use tower::ServiceExt;
        // A stored credential enables `anthropic` via the credential store — no env var involved.
        let projects = Arc::new(opencode_db::MemoryProjectStore::new());
        projects.insert(test_project_record("prj_1"));
        let credentials = Arc::new(opencode_db::MemoryCredentialStore::new());
        credentials.insert(opencode_db::CredentialRecord {
            id: "cred_1".to_string(),
            integration_id: "anthropic".to_string(),
            label: "default".to_string(),
            value: serde_json::json!({ "type": "key", "key": "sk-x" }),
        });
        let state = ServerState {
            ctx: AppContext::new(AppServices {
                catalog: opencode_effect::catalog_handle(sample_catalog()),
                projects,
                credentials,
                ..Default::default()
            }),
            routes: RouteTable::parse("provider"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
            runner: RunnerServices::default(),
            coordinator: SessionCoordinator::default(),
        };
        let resp = build_router(state)
            .oneshot(
                axum::extract::Request::builder()
                    .uri("/api/provider?directory=/repo")
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
        // Credential precedence: enabled is `{ via: "credential", credentialID }` regardless of env.
        assert_eq!(v["data"][0]["id"], "anthropic");
        assert_eq!(v["data"][0]["enabled"]["via"], "credential");
        assert_eq!(v["data"][0]["enabled"]["credentialID"], "cred_1");
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
    async fn metrics_endpoint_reports_runner_activity() {
        use tower::ServiceExt;
        let url = spawn_anthropic(&[EXEC_TEXT_SSE]).await;
        let dir = tempfile::tempdir().unwrap();
        let state = exec_state(url, dir.path().to_path_buf());
        let metrics_req = || {
            axum::extract::Request::builder()
                .uri("/_rust/metrics")
                .body(axum::body::Body::empty())
                .unwrap()
        };

        // Zero before any activity.
        let resp = build_router(state.clone())
            .oneshot(metrics_req())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["turns_total"], 0);
        assert_eq!(v["events_total"], 0);

        // Drive one turn via the internal execute route.
        let resp = build_router(state.clone())
            .oneshot(exec_request(
                "ses_m",
                "anthropic/claude-haiku-4-5-20251001",
                "hi",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // Metrics now reflect the run (one turn, one step, events published, one latency sample).
        let resp = build_router(state.clone())
            .oneshot(metrics_req())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["turns_total"], 1);
        assert_eq!(v["steps_total"], 1);
        assert!(v["events_total"].as_u64().unwrap() >= 1);
        assert_eq!(v["turn_latency_ms"]["count"], 1);
    }

    #[tokio::test]
    async fn prometheus_metrics_endpoint_renders_text_format() {
        use tower::ServiceExt;
        let url = spawn_anthropic(&[EXEC_TEXT_SSE]).await;
        let dir = tempfile::tempdir().unwrap();
        let state = exec_state(url, dir.path().to_path_buf());
        let scrape = || {
            axum::extract::Request::builder()
                .uri("/metrics")
                .body(axum::body::Body::empty())
                .unwrap()
        };

        // Before any activity: 200, Prometheus content-type, zeroed counters.
        let resp = build_router(state.clone()).oneshot(scrape()).await.unwrap();
        assert_eq!(resp.status(), 200);
        let ctype = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(ctype.starts_with("text/plain"));
        assert!(ctype.contains("version=0.0.4"));
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("# TYPE opencode_turns_total counter"));
        assert!(body.contains("\nopencode_turns_total 0\n"));

        // Drive one turn via the internal execute route.
        let resp = build_router(state.clone())
            .oneshot(exec_request(
                "ses_prom",
                "anthropic/claude-haiku-4-5-20251001",
                "hi",
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);

        // The scrape now reflects the run.
        let resp = build_router(state.clone()).oneshot(scrape()).await.unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("\nopencode_turns_total 1\n"));
        assert!(body.contains("\nopencode_steps_total 1\n"));
        assert!(body.contains("opencode_turn_latency_ms_count 1\n"));
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

        // Native projector (write path): the finished turn is now in the `session_message` timeline,
        // so `v2.session.messages` would return it.
        let timeline = state
            .ctx
            .session_messages()
            .list("ses_exec", None, None, opencode_db::MessageOrder::Asc, None)
            .await
            .unwrap();
        assert_eq!(timeline.len(), 2, "user + assistant entries");
        assert_eq!(timeline[0].kind, "user");
        assert_eq!(timeline[0].data["text"], "weather in Paris?");
        assert_eq!(timeline[1].kind, "assistant");
        let content = timeline[1].data["content"].as_array().unwrap();
        assert!(content
            .iter()
            .any(|c| c["type"] == "text" && c["text"].as_str().unwrap_or("").contains("sunny")));
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
