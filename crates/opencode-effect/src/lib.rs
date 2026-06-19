//! Runtime/DI support — the Rust analog of the Effect framework's `Layer`/`Context.Service`.
//!
//! - `Effect<A, E, R>` → `async fn(..) -> Result<A, AppError>`; the `R` (required services) is
//!   **not** encoded in the signature but passed explicitly via [`AppContext`].
//! - `Layer.provide` / `provideService` → a single `build_app_context()` constructor (later phases)
//!   that wires concrete services into [`AppContext`] (held as `Arc<dyn Trait>`).
//! - `Effect.withSpan` / `Effect.fn("Name")` → `#[tracing::instrument]` (see [`init_tracing`]).

pub mod bus;
pub mod metrics;

use std::sync::{Arc, RwLock};

use opencode_config::catalog::Catalog;
use opencode_db::{EventStore, ProjectStore, SessionStore};

pub use bus::{BusEvent, EventBus};
pub use metrics::{AppMetrics, MetricsSnapshot};

/// A hot-swappable handle to the models.dev catalog: an `Arc<RwLock<Arc<Catalog>>>` shared between the
/// server (which reads snapshots) and the background refresh task (which swaps in a fresh catalog).
/// Reads take a brief read lock and clone the inner `Arc` (cheap); the refresh swaps under a write lock.
pub type CatalogHandle = Arc<RwLock<Arc<Catalog>>>;

/// Wrap a catalog in a fresh [`CatalogHandle`].
pub fn catalog_handle(catalog: Catalog) -> CatalogHandle {
    Arc::new(RwLock::new(Arc::new(catalog)))
}

/// Binary-edge error taxonomy. Libraries return their own `thiserror` enums; these are mapped to
/// HTTP responses (and to the `opencode_proto::ErrorEnvelope` `_tag` shape) at the server edge.
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    /// Resource not found (maps to 404).
    #[error("not found: {0}")]
    NotFound(String),
    /// Optimistic-concurrency / state conflict (maps to 409).
    #[error("conflict: {0}")]
    Conflict(String),
    /// Invalid request (maps to 400).
    #[error("invalid request: {0}")]
    BadRequest(String),
    /// Any other error (maps to 500).
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// The HTTP status a given [`AppError`] should produce.
impl AppError {
    pub fn status_code(&self) -> u16 {
        match self {
            AppError::NotFound(_) => 404,
            AppError::Conflict(_) => 409,
            AppError::BadRequest(_) => 400,
            AppError::Other(_) => 500,
        }
    }

    /// Discriminator tag for the `_tag` field of the error envelope.
    pub fn tag(&self) -> &'static str {
        match self {
            AppError::NotFound(_) => "NotFoundError",
            AppError::Conflict(_) => "ConflictError",
            AppError::BadRequest(_) => "BadRequestError",
            AppError::Other(_) => "InternalError",
        }
    }
}

/// The services wired into an [`AppContext`] (the Rust analog of an Effect `Layer`-provided context).
///
/// `Default` provides in-memory stores (tests / ephemeral runs); the composition root
/// (`build_app_context()` in `opencode-bin`) overrides the fields it has real backings for via
/// struct-update syntax — `AppServices { event_store: db.event_store(), ..Default::default() }` — so
/// adding a new store doesn't churn every call site.
#[derive(Clone)]
pub struct AppServices {
    /// Append-only event store.
    pub event_store: Arc<dyn EventStore>,
    /// `session` projection read store.
    pub sessions: Arc<dyn SessionStore>,
    /// `project` projection read store.
    pub projects: Arc<dyn ProjectStore>,
    /// In-process event bus (global stream + per-aggregate watch).
    pub event_bus: Arc<EventBus>,
    /// In-process runner metrics (counters + turn-latency percentiles).
    pub metrics: Arc<AppMetrics>,
    /// The models.dev catalog (`providerID → Provider`), hot-swappable for background refresh. Read-only
    /// reference data the model/provider routes project into the V2 wire contract; defaults to empty.
    pub catalog: CatalogHandle,
}

impl Default for AppServices {
    fn default() -> Self {
        Self {
            event_store: Arc::new(opencode_db::MemoryEventStore::new()),
            sessions: Arc::new(opencode_db::MemorySessionStore::new()),
            projects: Arc::new(opencode_db::MemoryProjectStore::new()),
            event_bus: Arc::new(EventBus::new()),
            metrics: Arc::new(AppMetrics::default()),
            catalog: catalog_handle(Catalog::default()),
        }
    }
}

/// Shared application context — cheap to clone (`Arc`), threaded through axum via `State`.
#[derive(Clone)]
pub struct AppContext {
    inner: Arc<AppServices>,
}

impl AppContext {
    /// Construct a context from a wired [`AppServices`].
    pub fn new(services: AppServices) -> Self {
        Self {
            inner: Arc::new(services),
        }
    }

    /// A context backed entirely by in-memory stores — for tests and ephemeral runs.
    pub fn in_memory() -> Self {
        Self::new(AppServices::default())
    }

    /// The wired event store.
    pub fn event_store(&self) -> &Arc<dyn EventStore> {
        &self.inner.event_store
    }

    /// The wired session (projection) store.
    pub fn sessions(&self) -> &Arc<dyn SessionStore> {
        &self.inner.sessions
    }

    /// The wired project (projection) store.
    pub fn projects(&self) -> &Arc<dyn ProjectStore> {
        &self.inner.projects
    }

    /// The in-process event bus.
    pub fn event_bus(&self) -> &Arc<EventBus> {
        &self.inner.event_bus
    }

    /// The in-process runner metrics.
    pub fn metrics(&self) -> &Arc<AppMetrics> {
        &self.inner.metrics
    }

    /// A snapshot of the models.dev catalog (read-only reference data; empty unless wired at the
    /// composition root). Cheap: a brief read lock plus an `Arc` clone — callers get a stable snapshot
    /// even if a background refresh swaps the catalog concurrently.
    pub fn catalog(&self) -> Arc<Catalog> {
        self.inner
            .catalog
            .read()
            .expect("catalog lock poisoned")
            .clone()
    }
}

/// Initialize global tracing. Idempotent and safe to call once at startup.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,opencode=debug"));
    let _ = fmt().with_env_filter(filter).try_init();
}

#[cfg(test)]
mod context_tests {
    use super::*;
    use opencode_config::catalog::Provider;

    #[test]
    fn in_memory_catalog_is_empty() {
        let ctx = AppContext::in_memory();
        assert!(ctx.catalog().is_empty());
    }

    #[test]
    fn wired_catalog_is_accessible() {
        let mut catalog = Catalog::new();
        catalog.insert(
            "anthropic".to_string(),
            Provider {
                id: "anthropic".to_string(),
                name: "Anthropic".to_string(),
                env: vec![],
                api: None,
                npm: None,
                models: Default::default(),
            },
        );
        let ctx = AppContext::new(AppServices {
            catalog: catalog_handle(catalog),
            ..Default::default()
        });
        assert_eq!(ctx.catalog().len(), 1);
        assert!(ctx.catalog().contains_key("anthropic"));
    }

    #[test]
    fn catalog_hot_swap_is_visible_through_the_accessor() {
        // Proves the path the background refresh task uses: storing a fresh catalog under the shared
        // handle's write lock is observed by `AppContext::catalog()` snapshots.
        let handle = catalog_handle(Catalog::new());
        let ctx = AppContext::new(AppServices {
            catalog: handle.clone(),
            ..Default::default()
        });
        assert!(ctx.catalog().is_empty());

        let mut fresh = Catalog::new();
        fresh.insert(
            "p".to_string(),
            Provider {
                id: "p".to_string(),
                name: "P".to_string(),
                env: vec![],
                api: None,
                npm: None,
                models: Default::default(),
            },
        );
        *handle.write().unwrap() = Arc::new(fresh);

        assert_eq!(ctx.catalog().len(), 1);
        assert!(ctx.catalog().contains_key("p"));
    }
}
