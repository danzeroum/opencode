//! `opencode` binary (Rust). Boots the axum server that fronts the strangler-fig reverse proxy:
//! native routes (per `OPENCODE_RUST_ROUTES`) are served by Rust, the rest proxied to the TS server.

use std::sync::Arc;

use clap::Parser;
use opencode_effect::{init_tracing, AppContext};
use opencode_server::{proxy::Upstream, RouteTable, ServerState};

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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    let cli = Cli::parse();

    let state = ServerState {
        ctx: AppContext::new(),
        routes: RouteTable::from_env(),
        proxy: Arc::new(Upstream::new(cli.upstream.clone())),
    };
    tracing::info!(
        bind = %cli.bind,
        upstream = %cli.upstream,
        native_routes = state.routes.len(),
        "starting opencode (rust)"
    );

    opencode_server::serve(state, &cli.bind).await
}
