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
    routing::get,
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

/// Code-first OpenAPI document. `xtask openapi` emits it; `xtask openapi-diff` checks it against
/// `packages/sdk/openapi.json` per route group.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(health, global_health, path_get),
    components(schemas(
        opencode_proto::Health,
        opencode_proto::ErrorEnvelope,
        opencode_proto::BadRequestError,
        opencode_proto::BadRequestData,
        opencode_proto::Path
    )),
    tags(
        (name = "control", description = "Control-plane routes"),
        (name = "global", description = "Global control-plane routes"),
        (name = "instance", description = "Instance-scoped routes")
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
}
