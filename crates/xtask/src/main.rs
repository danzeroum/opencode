//! `xtask` — workspace task runner (replaces ad-hoc `script/*.ts`).
//!
//! Run via `cargo run -p xtask -- <cmd>`. Implements `ci`, `openapi`, and `openapi-diff` — the
//! contract gate that compares the Rust-generated OpenAPI against `packages/sdk/openapi.json`.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use clap::{Parser, Subcommand};
use serde_json::{json, Map, Value};

/// Contract paths enforced as a hard gate. A route is added here once it is cut over to Rust;
/// `openapi-diff` then fails if its generated shape diverges from the golden contract.
const CUTOVER_PATHS: &[&str] = &[
    "/global/health",
    "/api/health",
    "/path",
    "/find/file",
    "/find",
    "/find/symbol",
    "/file",
    "/file/content",
    "/file/status",
    "/api/fs/list",
    "/api/fs/find",
    "/api/fs/read/*",
    "/log",
    "/permission",
    "/api/permission/request",
    "/api/permission/saved",
    "/api/session/{sessionID}/permission",
    "/question",
    "/api/question/request",
    "/api/session/{sessionID}/question",
    "/api/session/{sessionID}/permission/{requestID}/reply",
    "/api/session/{sessionID}/question/{requestID}/reply",
    "/api/session/{sessionID}/question/{requestID}/reject",
    "/mcp",
    "/lsp",
    "/vcs",
    "/agent",
    "/command",
    "/config",
    "/config/providers",
    "/global/config",
    "/api/session",
    "/api/session/{sessionID}",
    "/session/{sessionID}",
    "/session/{sessionID}/revert",
    "/session/{sessionID}/unrevert",
    "/session/{sessionID}/prompt_async",
    "/session/{sessionID}/permissions/{permissionID}",
    "/api/session/{sessionID}/message",
    "/api/session/{sessionID}/prompt",
    "/api/session/{sessionID}/context",
    "/session",
    "/session/status",
    "/session/{sessionID}/todo",
    "/session/{sessionID}/children",
    "/global/dispose",
    "/global/event",
    "/instance/dispose",
    "/api/event",
    "/session/{sessionID}/abort",
    "/project",
    "/project/current",
    "/project/{projectID}/directories",
    "/api/model",
    "/api/provider",
    "/api/provider/{providerID}",
    "/api/integration",
    "/api/skill",
    "/api/command",
    "/api/reference",
    "/api/agent",
    "/api/location",
];

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

/// The set of response status codes declared by an operation.
fn response_codes(op: &Value) -> Vec<String> {
    let mut codes: Vec<String> = op
        .get("responses")
        .and_then(|r| r.as_object())
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    codes.sort();
    codes
}

/// The JSON response schema for `code` (or `Null` if none).
fn response_schema(op: &Value, code: &str) -> Value {
    op.pointer(&format!(
        "/responses/{code}/content/application~1json/schema"
    ))
    .cloned()
    .unwrap_or(Value::Null)
}

/// Merge an object `value`'s fields into `out` without overwriting existing keys. Used when a
/// single-element `oneOf`/`anyOf`/`allOf` collapses to its sole member. Non-objects are ignored.
fn merge_into(out: &mut Map<String, Value>, value: Value) {
    if let Value::Object(fields) = value {
        for (k, v) in fields {
            out.entry(k).or_insert(v);
        }
    }
}

/// Whether a (normalized) schema is the bare null schema `{ "type": "null" }` — utoipa emits it as a
/// union member for `Option<T>` over a `$ref`. Nullability is captured by `required`, so it's dropped
/// from unions (the scalar-`type` array case is handled separately).
fn is_null_schema(value: &Value) -> bool {
    value
        .as_object()
        .map(|m| m.len() == 1 && m.get("type").and_then(Value::as_str) == Some("null"))
        .unwrap_or(false)
}

/// Normalize a schema to its *structural skeleton* — resolving `$ref` against `components` and
/// keeping only `type` / `properties` / `required` (as a set) / `items` / `*Of`. Annotations and
/// value-constraints (`description`, `format`, `enum`, `minimum`, `additionalProperties`, …) are
/// dropped so that representation differences (e.g. utoipa `$ref` vs an inline object, `u64`'s
/// `minimum:0`, doc-comment descriptions) don't cause false mismatches.
fn normalize_schema(schema: &Value, components: &Map<String, Value>, depth: u8) -> Value {
    if depth == 0 {
        return json!("<max-depth>");
    }
    let Value::Object(map) = schema else {
        return schema.clone();
    };
    if let Some(reference) = map.get("$ref").and_then(|v| v.as_str()) {
        let name = reference.rsplit('/').next().unwrap_or(reference);
        return match components.get(name) {
            Some(def) => normalize_schema(def, components, depth - 1),
            None => json!({ "$ref": name }),
        };
    }
    let mut out = Map::new();
    if let Some(t) = map.get("type") {
        // Treat `[T, "null"]` (OpenAPI 3.1 nullable, e.g. utoipa's `Option<T>`) as just `T` —
        // optionality is already captured by `required`.
        let t = match t {
            Value::Array(arr) => {
                let mut kept: Vec<Value> = arr
                    .iter()
                    .filter(|x| x.as_str() != Some("null"))
                    .cloned()
                    .collect();
                if kept.len() == 1 {
                    kept.remove(0)
                } else {
                    Value::Array(kept)
                }
            }
            other => other.clone(),
        };
        out.insert("type".into(), t);
    }
    if let Some(props) = map.get("properties").and_then(|v| v.as_object()) {
        let mut np = Map::new();
        for (k, v) in props {
            np.insert(k.clone(), normalize_schema(v, components, depth - 1));
        }
        out.insert("properties".into(), Value::Object(np));
    }
    if let Some(req) = map.get("required").and_then(|v| v.as_array()) {
        let mut r: Vec<String> = req
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect();
        r.sort();
        r.dedup();
        out.insert("required".into(), json!(r));
    }
    // Keep `items` only when it's a real subschema. utoipa emits a boolean `items` (`false`) for fixed
    // tuples (`prefixItems`); the golden spec omits it, so drop the boolean form as a representation
    // difference.
    if let Some(items) = map.get("items").filter(|v| !v.is_boolean()) {
        out.insert(
            "items".into(),
            normalize_schema(items, components, depth - 1),
        );
    }
    // Collapse `oneOf`/`anyOf` into a single order-independent union (utoipa emits `oneOf` for
    // untagged enums where the golden spec uses `anyOf`). `allOf` (intersection) stays separate.
    let mut union: Vec<Value> = Vec::new();
    for key in ["oneOf", "anyOf"] {
        if let Some(arr) = map.get(key).and_then(|v| v.as_array()) {
            union.extend(
                arr.iter()
                    .map(|s| normalize_schema(s, components, depth - 1))
                    // Drop the `{ "type": "null" }` member utoipa adds for `Option<$ref>`.
                    .filter(|s| !is_null_schema(s)),
            );
        }
    }
    if !union.is_empty() {
        union.sort_by_key(Value::to_string);
        union.dedup();
        // A single-variant union ≡ the variant itself — collapses the golden's `anyOf[X, X]`
        // (duplicated `$ref`) against a bare `$ref`.
        if union.len() == 1 {
            merge_into(&mut out, union.remove(0));
        } else {
            out.insert("anyOf".into(), Value::Array(union));
        }
    }
    if let Some(arr) = map.get("allOf").and_then(|v| v.as_array()) {
        let mut all: Vec<Value> = arr
            .iter()
            .map(|s| normalize_schema(s, components, depth - 1))
            .collect();
        all.sort_by_key(Value::to_string);
        all.dedup();
        // A single-element `allOf` ≡ the element — collapses utoipa's `Option<NestedStruct>` wrapper
        // (`allOf: [{ $ref }]`) against the golden's inline object.
        if all.len() == 1 {
            merge_into(&mut out, all.remove(0));
        } else {
            out.insert("allOf".into(), Value::Array(all));
        }
    }
    Value::Object(out)
}

fn schemas_dir(spec: &Value) -> Map<String, Value> {
    spec.pointer("/components/schemas")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

/// Compare the Rust-generated spec against the golden contract, per operation, *structurally*.
/// Fails only for paths in [`CUTOVER_PATHS`] (so the gate stays green until a route is migrated).
fn openapi_diff() -> anyhow::Result<()> {
    let generated = serde_json::to_value(opencode_server::openapi_document())?;
    let golden_path = golden_openapi_path();
    anyhow::ensure!(
        golden_path.exists(),
        "golden spec not found at {}",
        golden_path.display()
    );
    let golden: Value = serde_json::from_str(&fs::read_to_string(&golden_path)?)?;

    let gen_comps = schemas_dir(&generated);
    let gold_comps = schemas_dir(&golden);
    let empty = Map::new();
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
            let Some(gold_op) = gold_paths.get(path).and_then(|m| m.get(method)) else {
                println!("  ~ {method} {path}: not in golden (new native route)");
                if enforced {
                    violations.push(format!("{method} {path}: missing from golden contract"));
                }
                continue;
            };

            let id_ok = gen_op.get("operationId") == gold_op.get("operationId");
            let gen_codes = response_codes(gen_op);
            let gold_codes = response_codes(gold_op);
            let codes_ok = gen_codes == gold_codes;
            let schemas_ok = gen_codes
                .iter()
                .filter(|c| gold_codes.contains(c))
                .all(|code| {
                    normalize_schema(&response_schema(gen_op, code), &gen_comps, 32)
                        == normalize_schema(&response_schema(gold_op, code), &gold_comps, 32)
                });

            let ok = id_ok && codes_ok && schemas_ok;
            println!(
                "  {} {method} {path}: operationId={id_ok} responses={codes_ok} schemas={schemas_ok}",
                if ok { "ok" } else { "MISMATCH" }
            );
            if enforced && !ok {
                violations.push(format!(
                    "{method} {path}: mismatch (operationId={id_ok}, responses={codes_ok}, schemas={schemas_ok})"
                ));
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
