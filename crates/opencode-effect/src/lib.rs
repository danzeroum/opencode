//! Runtime/DI support — the Rust analog of the Effect framework's `Layer`/`Context.Service`.
//!
//! - `Effect<A, E, R>` → `async fn(..) -> Result<A, AppError>`; the `R` (required services) is
//!   **not** encoded in the signature but passed explicitly via [`AppContext`].
//! - `Layer.provide` / `provideService` → a single `build_app_context()` constructor (later phases)
//!   that wires concrete services into [`AppContext`] (held as `Arc<dyn Trait>`).
//! - `Effect.withSpan` / `Effect.fn("Name")` → `#[tracing::instrument]` (see [`init_tracing`]).

use std::sync::Arc;

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
/// Services (`EventStore`, `Database`, `LlmClient`, `Config`, …) are added here as
/// `Arc<dyn Trait>` in later phases; for Phase 0 it is intentionally empty.
#[derive(Clone, Default)]
pub struct AppContext {
    #[allow(dead_code)]
    inner: Arc<AppContextInner>,
}

#[derive(Default)]
struct AppContextInner {
    // services land here (e.g. `event_store: Arc<dyn opencode_db::EventStore>`).
}

impl AppContext {
    /// Construct an empty context. Real wiring happens in `build_app_context()` (later phases).
    pub fn new() -> Self {
        Self::default()
    }
}

/// Initialize global tracing. Idempotent and safe to call once at startup.
pub fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,opencode=debug"));
    let _ = fmt().with_env_filter(filter).try_init();
}
