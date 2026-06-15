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

use axum::{extract::State, response::Json, routing::get, Router};
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

/// Code-first OpenAPI document. `xtask openapi` emits it; `xtask openapi-diff` checks it against
/// `packages/sdk/openapi.json` per route group.
#[derive(utoipa::OpenApi)]
#[openapi(
    paths(health),
    components(schemas(opencode_proto::Health, opencode_proto::ErrorEnvelope)),
    tags((name = "control", description = "Control-plane routes")),
    info(title = "opencode", version = VERSION)
)]
pub struct ApiDoc;

/// Return the generated OpenAPI document.
pub fn openapi_document() -> utoipa::openapi::OpenApi {
    use utoipa::OpenApi;
    ApiDoc::openapi()
}

/// Build the axum router: always-native liveness + cut-over contract routes + proxy fallback.
pub fn build_router(state: ServerState) -> Router {
    let mut router = Router::new().route("/_rust/health", get(rust_health));

    // Native contract routes are enabled here as they are cut over, gated by the route table.
    if state.routes.handles("health") {
        router = router.route("/health", get(health));
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
}
