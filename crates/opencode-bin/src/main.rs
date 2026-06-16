//! `opencode` binary (Rust). Boots the axum server that fronts the strangler-fig reverse proxy:
//! native routes (per `OPENCODE_RUST_ROUTES`) are served by Rust, the rest proxied to the TS server.
//!
//! [`build_app_context`] is the composition root (the Rust analog of Effect's `Layer.provide`): it
//! opens the single shared SQLite pool, verifies the migration journal, and wires the services into
//! the [`AppContext`] threaded through axum.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use opencode_db::Database;
use opencode_effect::{init_tracing, AppContext, AppServices};
use opencode_server::{
    proxy::Upstream, RouteTable, RunnerServices, ServerState, SessionCoordinator,
};

#[derive(Parser, Debug)]
#[command(name = "opencode", version, about = "opencode backend (Rust)")]
struct Cli {
    /// Address to bind the public HTTP server.
    #[arg(long, env = "OPENCODE_BIND", default_value = "127.0.0.1:4096")]
    bind: String,

    /// Upstream TypeScript server base URL for proxied (not-yet-migrated) routes.
    #[arg(
        long,
        env = "OPENCODE_UPSTREAM",
        default_value = "http://127.0.0.1:4097"
    )]
    upstream: String,

    /// SQLite database. `:memory:` for ephemeral; an absolute path is used as-is; a relative path is
    /// resolved under the opencode data dir. Defaults to `<data-dir>/opencode.db` (mirrors the TS
    /// `OPENCODE_DB` flag).
    #[arg(long, env = "OPENCODE_DB")]
    db: Option<String>,
}

/// The opencode data directory (`$XDG_DATA_HOME/opencode`, falling back to `~/.local/share/opencode`),
/// mirroring `packages/core/src/global.ts`.
fn data_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("opencode");
        }
    }
    let home = std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".local/share/opencode")
}

/// Resolve the database path from the `--db`/`OPENCODE_DB` flag, mirroring TS `Database.path()`:
/// `:memory:` and absolute paths pass through; a relative value is joined under `data_dir`; an absent
/// flag defaults to `<data_dir>/opencode.db`.
fn resolve_db_path(flag: Option<&str>, data_dir: &Path) -> PathBuf {
    match flag {
        Some(":memory:") => PathBuf::from(":memory:"),
        Some(f) if Path::new(f).is_absolute() => PathBuf::from(f),
        Some(f) => data_dir.join(f),
        None => data_dir.join("opencode.db"),
    }
}

/// Composition root: open the shared pool, verify the migration journal, and build the [`AppContext`].
///
/// Migration policy ("TS migrates, Rust verifies"): a database that is **behind** this build (missing
/// expected migrations) is fatal — the TypeScript server must apply migrations first. A database
/// **ahead** of this build, or one with no journal yet, only warns.
async fn build_app_context(db_path: &Path) -> anyhow::Result<AppContext> {
    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() && db_path.as_os_str() != ":memory:" {
            std::fs::create_dir_all(parent).ok();
        }
    }
    let db = Database::connect(db_path).await?;

    let report = db.verify_migrations().await?;
    if report.journal_present && report.is_behind() {
        anyhow::bail!(
            "database at {} is behind this opencode build: missing {} migration(s), starting with `{}`. \
             Start the TypeScript opencode server to apply migrations (it owns the schema during the Rust migration).",
            db_path.display(),
            report.missing.len(),
            report.missing.first().map(String::as_str).unwrap_or("?"),
        );
    }
    if report.is_ahead() {
        tracing::warn!(
            newer = report.unknown.len(),
            first = report.unknown.first().map(String::as_str).unwrap_or(""),
            "database has migrations newer than this opencode build; native routes assume this build's schema",
        );
    }
    if !report.journal_present {
        tracing::warn!(
            db = %db_path.display(),
            "database has no migration journal (fresh/uninitialized); the TypeScript server owns schema creation",
        );
    } else if report.in_sync() {
        tracing::info!(
            applied = report.applied.len(),
            "database migrations verified"
        );
    }

    Ok(AppContext::new(AppServices {
        event_store: db.event_store(),
        sessions: db.session_store(),
        projects: db.project_store(),
        ..Default::default()
    }))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let db_path = resolve_db_path(cli.db.as_deref(), &data_dir());
    let ctx = build_app_context(&db_path).await?;

    // The native tools resolve relative paths against the server's working directory.
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let state = ServerState {
        ctx,
        routes: RouteTable::from_env(),
        proxy: Arc::new(Upstream::new(cli.upstream.clone())),
        runner: RunnerServices::from_env(root)?,
        coordinator: SessionCoordinator::default(),
    };
    tracing::info!(
        bind = %cli.bind,
        upstream = %cli.upstream,
        db = %db_path.display(),
        native_routes = state.routes.len(),
        "starting opencode (rust)"
    );

    opencode_server::serve(state, &cli.bind).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_db_path_handles_memory_absolute_and_relative() {
        let data = Path::new("/data/opencode");
        assert_eq!(
            resolve_db_path(Some(":memory:"), data),
            PathBuf::from(":memory:")
        );
        assert_eq!(
            resolve_db_path(Some("/abs/custom.db"), data),
            PathBuf::from("/abs/custom.db")
        );
        assert_eq!(
            resolve_db_path(Some("custom.db"), data),
            PathBuf::from("/data/opencode/custom.db")
        );
        assert_eq!(
            resolve_db_path(None, data),
            PathBuf::from("/data/opencode/opencode.db")
        );
    }

    #[tokio::test]
    async fn build_app_context_wires_a_working_event_store() {
        // A fresh file DB has no journal → warns (not fatal) and yields a usable event store.
        let dir = tempfile::tempdir().unwrap();
        let ctx = build_app_context(&dir.path().join("opencode.db"))
            .await
            .unwrap();
        let store = ctx.event_store();
        let head = store
            .append(
                "ses_1",
                0,
                vec![opencode_db::EventInput::new("x", serde_json::Value::Null)],
            )
            .await
            .unwrap();
        assert_eq!(head, 1);
        assert_eq!(store.head_seq("ses_1").await.unwrap(), 1);
    }
}
