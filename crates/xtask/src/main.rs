//! `xtask` — workspace task runner (replaces ad-hoc `script/*.ts`).
//!
//! Run via `cargo run -p xtask -- <cmd>`. Phase 0 implements `ci`, `openapi`, and a placeholder
//! `openapi-diff` (the central contract-test spike to be fleshed out next).

use std::path::PathBuf;
use std::process::Command;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "xtask", about = "opencode Rust workspace tasks")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the full local CI suite: fmt --check + clippy -D warnings + nextest + deny.
    Ci,
    /// Emit the Rust-generated OpenAPI document (pretty JSON) to stdout.
    Openapi,
    /// Diff the Rust-generated OpenAPI against packages/sdk/openapi.json (Phase 0 spike — WIP).
    OpenapiDiff,
}

fn run(cmd: &str, args: &[&str]) -> anyhow::Result<()> {
    println!("$ {cmd} {}", args.join(" "));
    let status = Command::new(cmd).args(args).status()?;
    anyhow::ensure!(
        status.success(),
        "`{cmd} {}` failed ({status})",
        args.join(" ")
    );
    Ok(())
}

fn run_optional(cmd: &str, args: &[&str]) {
    if let Err(err) = run(cmd, args) {
        eprintln!("warning: skipping `{cmd}` ({err})");
    }
}

fn golden_openapi_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../packages/sdk/openapi.json")
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Ci => {
            run("cargo", &["fmt", "--all", "--check"])?;
            run(
                "cargo",
                &["clippy", "--all-targets", "--", "-D", "warnings"],
            )?;
            // Prefer nextest; fall back to `cargo test` if nextest isn't installed.
            if run("cargo", &["nextest", "run"]).is_err() {
                run("cargo", &["test", "--all"])?;
            }
            // cargo-deny is optional locally; required in CI.
            run_optional("cargo", &["deny", "check"]);
            Ok(())
        }
        Cmd::Openapi => {
            let doc = opencode_server::openapi_document();
            println!("{}", serde_json::to_string_pretty(&doc)?);
            Ok(())
        }
        Cmd::OpenapiDiff => {
            let generated = opencode_server::openapi_document();
            let generated = serde_json::to_value(&generated)?;
            let golden_path = golden_openapi_path();

            let generated_paths = generated
                .get("paths")
                .and_then(|p| p.as_object())
                .map(|o| o.len())
                .unwrap_or(0);
            println!(
                "rust-generated OpenAPI: {generated_paths} path(s); golden = {}",
                golden_path.display()
            );

            if !golden_path.exists() {
                eprintln!("note: golden spec not found at {}", golden_path.display());
            }
            eprintln!(
                "openapi-diff: Phase 0 spike — full per-group semantic diff (normalize $ref names, \
                 nullable vs Option, key order) is the next deliverable."
            );
            Ok(())
        }
    }
}
