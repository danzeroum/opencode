//! Fetching + caching the **models.dev catalog** ([`opencode_config::catalog::Catalog`]).
//!
//! Port target: the I/O half of `packages/core/src/models-dev.ts` (the types + parser live in
//! [`opencode_config::catalog`], pure + dependency-free). [`fetch_catalog`] GETs `<source>/api.json`
//! over an **injected** HTTP client (so tests use a plain in-test server; production passes an HTTPS
//! client). [`populate`] mirrors the TS load order (explicit path → on-disk cache → fetch + write) and
//! [`refresh`] re-fetches when the cache is stale. The cache file is written **verbatim** (the raw
//! `api.json` text), so nothing is lost to our lenient parse.
//!
//! Deferred (later increments): the cross-process cache lock (TS `Flock`), the periodic background
//! refresh task, and resolving the platform cache directory (the caller supplies `cache_path`).

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use opencode_config::catalog::{parse_catalog, Catalog};

/// Default catalog host.
const DEFAULT_SOURCE: &str = "https://models.dev";
/// Per-request timeout for a catalog fetch (mirrors the TS 10s).
const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long an on-disk cache is considered fresh (mirrors the TS 5-minute `ttl`).
pub const CACHE_TTL: Duration = Duration::from_secs(5 * 60);

/// Errors fetching or caching the catalog.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    /// The HTTP fetch failed, or the server returned a non-success status.
    #[error("catalog fetch error: {0}")]
    Fetch(String),
    /// Reading or writing the on-disk cache failed.
    #[error("catalog cache i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// The fetched/cached text didn't parse as a catalog.
    #[error("catalog parse error: {0}")]
    Parse(String),
}

/// How [`populate`] sources the catalog. Built from the environment via [`PopulateOpts::from_env`] in
/// production, or explicitly in tests (so no global env mutation is needed).
#[derive(Debug, Clone)]
pub struct PopulateOpts {
    /// `OPENCODE_MODELS_PATH`: load the catalog from this exact file and nothing else.
    pub models_path: Option<PathBuf>,
    /// `OPENCODE_DISABLE_MODELS_FETCH`: never hit the network (an absent cache yields an empty catalog).
    pub disable_fetch: bool,
    /// `OPENCODE_MODELS_URL`, or [`DEFAULT_SOURCE`].
    pub source: String,
}

impl Default for PopulateOpts {
    fn default() -> Self {
        Self {
            models_path: None,
            disable_fetch: false,
            source: DEFAULT_SOURCE.to_string(),
        }
    }
}

impl PopulateOpts {
    /// Resolve options from the `OPENCODE_MODELS_*` environment variables.
    pub fn from_env() -> Self {
        Self {
            models_path: std::env::var_os("OPENCODE_MODELS_PATH").map(PathBuf::from),
            disable_fetch: std::env::var_os("OPENCODE_DISABLE_MODELS_FETCH").is_some_and(|v| {
                let v = v.to_string_lossy();
                !v.is_empty() && v != "0" && v != "false"
            }),
            source: std::env::var("OPENCODE_MODELS_URL")
                .unwrap_or_else(|_| DEFAULT_SOURCE.to_string()),
        }
    }
}

/// The `User-Agent` sent on catalog fetches.
fn user_agent() -> String {
    format!("opencode-rust/{}", env!("CARGO_PKG_VERSION"))
}

/// GET `<source>/api.json` over `client` and return the raw response text.
async fn fetch_raw(client: &reqwest::Client, source: &str) -> Result<String, CatalogError> {
    let url = format!("{}/api.json", source.trim_end_matches('/'));
    client
        .get(&url)
        .header("user-agent", user_agent())
        .timeout(FETCH_TIMEOUT)
        .send()
        .await
        .map_err(|e| CatalogError::Fetch(e.to_string()))?
        .error_for_status()
        .map_err(|e| CatalogError::Fetch(e.to_string()))?
        .text()
        .await
        .map_err(|e| CatalogError::Fetch(e.to_string()))
}

/// Fetch + parse the catalog from `<source>/api.json`.
pub async fn fetch_catalog(
    client: &reqwest::Client,
    source: &str,
) -> Result<Catalog, CatalogError> {
    let text = fetch_raw(client, source).await?;
    parse_catalog(&text).map_err(|e| CatalogError::Parse(e.to_string()))
}

/// Read + parse a catalog file.
fn load_file(path: &Path) -> Result<Catalog, CatalogError> {
    let text = std::fs::read_to_string(path)?;
    parse_catalog(&text).map_err(|e| CatalogError::Parse(e.to_string()))
}

/// Write `text` to `path` atomically (temp file in the same dir, then rename), creating parent dirs.
fn write_cache(path: &Path, text: &str) -> Result<(), CatalogError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension(format!("tmp.{}", std::process::id()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Whether `path` exists and was modified within `ttl`.
pub fn cache_is_fresh(path: &Path, ttl: Duration) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    let Ok(modified) = meta.modified() else {
        return false;
    };
    SystemTime::now()
        .duration_since(modified)
        .is_ok_and(|age| age < ttl)
}

/// Load the catalog, mirroring `ModelsDev.populate`:
/// 1. `models_path` override → load that file only;
/// 2. an existing on-disk cache at `cache_path` → load it (the TS background refresh / coexistence
///    keeps it current);
/// 3. `disable_fetch` → an empty catalog;
/// 4. otherwise fetch `<source>/api.json`, write it to `cache_path` verbatim, and parse it.
pub async fn populate(
    client: &reqwest::Client,
    cache_path: &Path,
    opts: &PopulateOpts,
) -> Result<Catalog, CatalogError> {
    if let Some(path) = &opts.models_path {
        return load_file(path);
    }
    if cache_path.exists() {
        if let Ok(cat) = load_file(cache_path) {
            return Ok(cat);
        }
    }
    if opts.disable_fetch {
        return Ok(Catalog::new());
    }
    let text = fetch_raw(client, &opts.source).await?;
    write_cache(cache_path, &text)?;
    parse_catalog(&text).map_err(|e| CatalogError::Parse(e.to_string()))
}

/// Re-fetch the catalog into `cache_path` when the cache is stale (or `force`). Mirrors
/// `ModelsDev.refresh`; a fetch failure is returned (the caller decides whether to ignore it).
pub async fn refresh(
    client: &reqwest::Client,
    cache_path: &Path,
    source: &str,
    force: bool,
) -> Result<(), CatalogError> {
    if !force && cache_is_fresh(cache_path, CACHE_TTL) {
        return Ok(());
    }
    let text = fetch_raw(client, source).await?;
    write_cache(cache_path, &text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Router};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    const CATALOG: &str = r#"{
      "anthropic": {
        "id": "anthropic", "name": "Anthropic", "env": ["ANTHROPIC_API_KEY"],
        "models": { "claude": { "id": "claude", "name": "Claude",
          "limit": { "context": 200000, "output": 64000 } } }
      }
    }"#;

    async fn spawn(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    fn opts(source: &str) -> PopulateOpts {
        PopulateOpts {
            models_path: None,
            disable_fetch: false,
            source: source.to_string(),
        }
    }

    /// A `/api.json` route returning the canned catalog and counting hits.
    fn counting_app(hits: Arc<AtomicU32>) -> Router {
        Router::new().route(
            "/api.json",
            get(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    CATALOG
                }
            }),
        )
    }

    #[tokio::test]
    async fn fetch_catalog_parses_remote() {
        let base = spawn(counting_app(Arc::new(AtomicU32::new(0)))).await;
        let client = reqwest::Client::new();
        let cat = fetch_catalog(&client, &base).await.unwrap();
        assert_eq!(cat["anthropic"].name, "Anthropic");
        assert!(cat["anthropic"].models.contains_key("claude"));
    }

    #[tokio::test]
    async fn populate_fetches_then_reuses_disk_cache() {
        let hits = Arc::new(AtomicU32::new(0));
        let base = spawn(counting_app(hits.clone())).await;
        let client = reqwest::Client::new();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("models.json");

        // No cache → fetch + write + parse.
        let cat = populate(&client, &cache, &opts(&base)).await.unwrap();
        assert!(cat.contains_key("anthropic"));
        assert!(cache.exists());
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Cache present → no second fetch.
        let again = populate(&client, &cache, &opts(&base)).await.unwrap();
        assert!(again.contains_key("anthropic"));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "disk cache must avoid a second fetch"
        );
    }

    #[tokio::test]
    async fn disable_fetch_without_cache_is_empty() {
        let client = reqwest::Client::new();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("models.json");
        let opts = PopulateOpts {
            models_path: None,
            disable_fetch: true,
            source: "http://127.0.0.1:1".into(),
        };
        let cat = populate(&client, &cache, &opts).await.unwrap();
        assert!(cat.is_empty());
        assert!(!cache.exists());
    }

    #[tokio::test]
    async fn models_path_override_loads_that_file_and_skips_cache() {
        let client = reqwest::Client::new();
        let dir = tempfile::tempdir().unwrap();
        let explicit = dir.path().join("custom.json");
        std::fs::write(&explicit, CATALOG).unwrap();
        let cache = dir.path().join("models.json");
        let opts = PopulateOpts {
            models_path: Some(explicit),
            disable_fetch: false,
            source: "http://127.0.0.1:1".into(),
        };
        let cat = populate(&client, &cache, &opts).await.unwrap();
        assert!(cat.contains_key("anthropic"));
        assert!(!cache.exists(), "an explicit path must not touch the cache");
    }

    #[tokio::test]
    async fn refresh_writes_when_stale_and_skips_when_fresh() {
        let hits = Arc::new(AtomicU32::new(0));
        let base = spawn(counting_app(hits.clone())).await;
        let client = reqwest::Client::new();
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("models.json");

        // No cache → stale → fetch + write.
        refresh(&client, &cache, &base, false).await.unwrap();
        assert!(cache.exists());
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // Fresh cache → skip.
        refresh(&client, &cache, &base, false).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // force → fetch regardless.
        refresh(&client, &cache, &base, true).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2);
    }
}
