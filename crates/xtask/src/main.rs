//! `xtask` — workspace task runner (replaces ad-hoc `script/*.ts`).
//!
//! Run via `cargo run -p xtask -- <cmd>`. Implements `ci`, `openapi`, and `openapi-diff` — the
//! contract gate that compares the Rust-generated OpenAPI against `packages/sdk/openapi.json`.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

use clap::{Parser, Subcommand};
use serde_json::Value;

/// Contract paths enforced as a hard gate. A route is added here once it is cut over to Rust;
/// `openapi-diff` then fails if its generated shape diverges from the golden contract. Empty until
/// the first route cutover, so the gate is green by construction during early phases.
const CUTOVER_PATHS: &[&str] = &["/global/health"];

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
    /// Diff the Rust-generated OpenAPI against packages/sdk/openapi.json per operation.
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

/// Collect referenced component names (`$ref` leaf segments) anywhere within `value`, so two
/// operations can be compared by the *set* of schemas they reference (ignoring `$ref` path prefix).
fn collect_refs(value: &Value, out: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (key, val) in map {
                if key == "$ref" {
                    if let Some(s) = val.as_str() {
                        out.insert(s.rsplit('/').next().unwrap_or(s).to_string());
                    }
                } else {
                    collect_refs(val, out);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect_refs(v, out)),
        _ => {}
    }
}

/// The set of response status codes declared by an operation.
fn response_codes(op: &Value) -> BTreeSet<String> {
    op.get("responses")
        .and_then(|r| r.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

/// The set of component schemas an operation references.
fn refs_of(op: &Value) -> BTreeSet<String> {
    let mut s = BTreeSet::new();
    collect_refs(op, &mut s);
    s
}

/// Compare the Rust-generated spec against the golden contract, per operation. Fails only for paths
/// listed in [`CUTOVER_PATHS`] (so the gate stays green until a route is actually migrated).
fn openapi_diff() -> anyhow::Result<()> {
    let generated = serde_json::to_value(opencode_server::openapi_document())?;
    let golden_path = golden_openapi_path();
    anyhow::ensure!(
        golden_path.exists(),
        "golden spec not found at {}",
        golden_path.display()
    );
    let golden: Value = serde_json::from_str(&fs::read_to_string(&golden_path)?)?;

    let empty = serde_json::Map::new();
    let gen_paths = generated
        .get("paths")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let gold_paths = golden
        .get("paths")
        .and_then(Value::as_object)
        .unwrap_or(&empty);

    println!(
        "OpenAPI contract diff — generated {} path(s) vs golden {} path(s)",
        gen_paths.len(),
        gold_paths.len()
    );

    let mut violations = Vec::new();
    for (path, methods) in gen_paths {
        let Some(methods) = methods.as_object() else {
            continue;
        };
        for (method, gen_op) in methods {
            let enforced = CUTOVER_PATHS.contains(&path.as_str());
            match gold_paths.get(path).and_then(|m| m.get(method)) {
                None => {
                    println!("  ~ {method} {path}: not in golden (new native route)");
                    if enforced {
                        violations.push(format!("{method} {path}: missing from golden contract"));
                    }
                }
                Some(gold_op) => {
                    let id_ok = gen_op.get("operationId") == gold_op.get("operationId");
                    let codes_ok = response_codes(gen_op) == response_codes(gold_op);
                    let refs_ok = refs_of(gen_op) == refs_of(gold_op);
                    let ok = id_ok && codes_ok && refs_ok;
                    println!(
                        "  {} {method} {path}: operationId={id_ok} responses={codes_ok} schemas={refs_ok}",
                        if ok { "ok" } else { "MISMATCH" }
                    );
                    if enforced && !ok {
                        violations.push(format!(
                            "{method} {path}: mismatch (operationId={id_ok}, responses={codes_ok}, schemas={refs_ok})"
                        ));
                    }
                }
            }
        }
    }

    println!(
        "enforced cut-over paths: {}",
        if CUTOVER_PATHS.is_empty() {
            "(none yet)".to_string()
        } else {
            CUTOVER_PATHS.join(", ")
        }
    );

    if violations.is_empty() {
        println!("OK: no cut-over route diverges from the golden contract");
        Ok(())
    } else {
        for v in &violations {
            eprintln!("  contract violation: {v}");
        }
        anyhow::bail!(
            "{} cut-over route(s) diverge from the golden contract",
            violations.len()
        )
    }
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
            if run("cargo", &["nextest", "run"]).is_err() {
                run("cargo", &["test", "--all"])?;
            }
            run_optional("cargo", &["deny", "check"]);
            Ok(())
        }
        Cmd::Openapi => {
            let doc = opencode_server::openapi_document();
            println!("{}", serde_json::to_string_pretty(&doc)?);
            Ok(())
        }
        Cmd::OpenapiDiff => openapi_diff(),
    }
}
