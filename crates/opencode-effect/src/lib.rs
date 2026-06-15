//! Runtime/DI support — the Rust analog of the Effect framework's `Layer`/`Context.Service`.
//!
//! - `Effect<A, E, R>` → `async fn(..) -> Result<A, AppError>`; the `R` (required services) is
//!   **not** encoded in the signature but passed explicitly via [`AppContext`].
//! - `Layer.provide` / `provideService` → a single `build_app_context()` constructor (later phases)
//!   that wires concrete services into [`AppContext`] (held as `Arc<dyn Trait>`).
//! - `Effect.withSpan` / `Effect.fn("Name")` → `#[tracing::instrument]` (see [`init_tracing`]).

use std::sync::Arc;

use opencode_db::{EventStore, SessionStore};

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

/// Shared application context — cheap to clone (`Arc`), threaded through axum via `State`.
///
/// This is the Rust analog of an Effect `Layer`-provided context: services are held as
/// `Arc<dyn Trait>` and wired once at the composition root (`build_app_context()` in `opencode-bin`).
/// More services (`Database` repos, `LlmClient`, `Config`, …) are added here as routes are cut over.
#[derive(Clone)]
pub struct AppContext {
    inner: Arc<AppContextInner>,
}

struct AppContextInner {
    event_store: Arc<dyn EventStore>,
    sessions: Arc<dyn SessionStore>,
}

impl AppContext {
    /// Construct a context wired with the given stores. Production wiring (single shared SQLite pool,
    /// migration verification) happens in `build_app_context()`.
    pub fn new(event_store: Arc<dyn EventStore>, sessions: Arc<dyn SessionStore>) -> Self {
        Self {
            inner: Arc::new(AppContextInner {
                event_store,
                sessions,
            }),
        }
    }

    /// A context backed by in-memory stores — for tests and ephemeral runs.
    pub fn in_memory() -> Self {
        Self::new(
            Arc::new(opencode_db::MemoryEventStore::new()),
            Arc::new(opencode_db::MemorySessionStore::new()),
        )
    }

    /// The wired event store.
    pub fn event_store(&self) -> &Arc<dyn EventStore> {
        &self.inner.event_store
    }

    /// The wired session (projection) store.
    pub fn sessions(&self) -> &Arc<dyn SessionStore> {
        &self.inner.sessions
    }
}

/// Initialize global tracing. Idempotent and safe to call once at startup.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,opencode=debug"));
    let _ = fmt().with_env_filter(filter).try_init();
}
