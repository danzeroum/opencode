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
use opencode_effect::AppContext;
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
    /// Application/DI context (services land here in later phases).
    pub ctx: AppContext,
    /// Native-vs-proxy routing decisions.
    pub routes: RouteTable,
    /// Upstream TypeScript server used for proxied routes.
    pub proxy: Arc<proxy::Upstream>,
}

/// Always-native internal liveness/readiness route (not part of the public OpenAPI contract).
async fn rust_health() -> Json<Health> {
    Json(Health {
        ok: true,
        backend: "rust".to_string(),
        version: VERSION.to_string(),
    })
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

/// Code-first OpenAPI document. `xtask openapi` emits it; `xtask openapi-diff` checks it against
/// `packages/sdk/openapi.json` per route group.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(health, global_health, path_get, find_files, find_text, app_log, v2_session_get),
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
        opencode_proto::UnauthorizedError
    )),
    tags(
        (name = "control", description = "Control-plane routes"),
        (name = "global", description = "Global control-plane routes"),
        (name = "instance", description = "Instance-scoped routes"),
        (name = "file", description = "File routes"),
        (name = "sessions", description = "Session routes")
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
    let mut router = Router::new().route("/_rust/health", get(rust_health));

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
        router = router.route("/api/session/{sessionID}", get(v2_session_get));
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
            ctx: AppContext::new(Arc::new(opencode_db::MemoryEventStore::new()), sessions),
            routes: RouteTable::parse("session"),
            proxy: Arc::new(proxy::Upstream::new("http://127.0.0.1:1")),
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
}
