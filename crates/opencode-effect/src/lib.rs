//! Runtime/DI support — the Rust analog of the Effect framework's `Layer`/`Context.Service`.
//!
//! - `Effect<A, E, R>` → `async fn(..) -> Result<A, AppError>`; the `R` (required services) is
//!   **not** encoded in the signature but passed explicitly via [`AppContext`].
//! - `Layer.provide` / `provideService` → a single `build_app_context()` constructor (later phases)
//!   that wires concrete services into [`AppContext`] (held as `Arc<dyn Trait>`).
//! - `Effect.withSpan` / `Effect.fn("Name")` → `#[tracing::instrument]` (see [`init_tracing`]).

use std::sync::Arc;

use opencode_db::{EventStore, ProjectStore, SessionStore};

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
}

impl Default for AppServices {
    fn default() -> Self {
        Self {
            event_store: Arc::new(opencode_db::MemoryEventStore::new()),
            sessions: Arc::new(opencode_db::MemorySessionStore::new()),
            projects: Arc::new(opencode_db::MemoryProjectStore::new()),
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
}

/// Initialize global tracing. Idempotent and safe to call once at startup.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,opencode=debug"));
    let _ = fmt().with_env_filter(filter).try_init();
}
