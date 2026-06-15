# opencode — Backend Rust Migration Roadmap

Reference for the incremental migration of the opencode **backend** (core + server +
llm + utilities) from TypeScript/Bun to Rust. Frontends (tui/web/desktop/ui/app)
stay TypeScript. Strategy: **strangler-fig** behind the existing HTTP/OpenAPI contract.

> Status legend: ✅ done · 🟡 in progress · ⬜ not started

## Fixed decisions

1. **Strategy:** incremental strangler-fig, behind the same HTTP/OpenAPI contract.
2. **Scope:** entire backend (`core` + `server` + `llm` + utilities). Frontends stay TS.
3. **LLM:** native Rust crates (no TS sidecar). Anthropic/Claude first.
4. **CI:** Rust toolchain + `xtask` (cargo/clippy/rustfmt/nextest), GitHub Actions as executor.

## The seam (how cutover works)

`opencode-bin` runs an **axum** server that fronts a **reverse proxy**. A route table parsed
from `OPENCODE_RUST_ROUTES` (comma-separated group names) decides, per route group, whether
the request is served natively by Rust or proxied to the existing TypeScript server. With an
empty table everything proxies to TS. A group is "cut over" by adding it to the table; rolling
back is removing it. Frontends/SDK keep seeing one unchanged endpoint and one `openapi.json`.

## Workspace layout (`crates/`)

| Crate | Responsibility |
|---|---|
| `opencode-bin` | `opencode` binary: CLI (clap) + boots the server |
| `opencode-server` | axum router, reverse-proxy fallback (seam), OpenAPI (utoipa) |
| `opencode-core` | domain: sessions, events, permissions, agents, projects, jobs |
| `opencode-tools` | tools (bash/read/edit/glob/grep/git/…) + fs/git/search/pty helpers |
| `opencode-llm` | LLM protocol router (ported from `packages/llm`) |
| `opencode-db` | sqlx pool, migration runner, event-store/session-store repos |
| `opencode-config` | JSONC config, schema, catalog (models.dev) merge |
| `opencode-proto` | external HTTP contract types (mirror `packages/sdk/openapi.json`) |
| `opencode-events` | internal event-store contract (versioned events + upcasters) |
| `opencode-effect` | DI/runtime: `AppContext`, error taxonomy, tracing |
| `opencode-plugin` | out-of-process JS plugin host + MCP client (rmcp) |
| `opencode-integration` | github/gitlab/slack adapters |
| `xtask` | build/CI/release/codegen orchestration |

## Key crate choices

tokio · axum 0.8 · utoipa 5 (code-first OpenAPI) · sqlx 0.8 (SQLite) · reqwest 0.12 ·
serde · garde (validation) · schemars (tool JSON Schema) · gix (git) · grep/ignore/globset
(ripgrep libs) · portable-pty · notify · rmcp (MCP) · `aws-sdk-bedrockruntime` (Bedrock) ·
tracing/OpenTelemetry · thiserror/anyhow · clap · insta · cargo-nextest · cargo-zigbuild ·
cargo-dist · cargo-deny. **Out of scope:** tree-sitter (TUI-only).

## Phase checklist

### Phase 0 — Foundations ✅ (this PR)
- ✅ Cargo workspace + all crate skeletons; toolchain/deny/nextest config
- ✅ Reverse-proxy seam (`OPENCODE_RUST_ROUTES`) + native `/_rust/health`
- ✅ Code-first OpenAPI (utoipa) + `xtask` (`ci` / `openapi` / `openapi-diff`)
- ✅ Error contract: `AppError` → HTTP status + `ErrorEnvelope` (`_tag`) in `opencode-server`
- ✅ `rust.yml` CI (fmt + clippy -D warnings + nextest + cargo-deny + openapi gate), side-by-side with TS CI
- 🟡 `openapi-diff` contract gate: per-operation compare (operationId + response codes + referenced schema names) vs `packages/sdk/openapi.json`, hard-failing only for an explicit cut-over allowlist (`CUTOVER_PATHS`, empty until first cutover) — landed; deeper normalization (nullable vs Option, params) as routes migrate
- ⬜ Audit the MCP patch (`patches/@modelcontextprotocol%2Fsdk@1.29.0.patch`, reconnect/`onsessionexpired`)
- ⬜ Spikes: `die`/`catchDefect` → `TurnOutcome` enum; `FiberSet` → `ToolExecutor`; `state.ts` `Draft<T>`/replay
- ⬜ Pin `effect@4.0.0-beta.74`; align with the in-flight V2 refactor (`specs/v2`)

### Phase 1 — Leaf / low-risk modules 🟡
- 🟡 `opencode-config`: JSONC loader (`jsonc-parser`, comment/trailing-comma parity) + typed `Config` (top-level V1 subset; unmodeled keys preserved via `extra`) — landed; remaining config submodules in progress
- 🟡 `opencode-tools`: `read`/`glob`/`grep` (ripgrep libs), `write`/`edit`/`ls`, `process::run_command` + `run_shell` (shell exec + output cap — bash-tool base), `git` (shells out to the `git` binary — faithful to `git.ts`; no `gix` dep) — landed; PTY next
- ⬜ Cutover: `health`, `fs`, `location`, `reference`, `command`, `skill`

### Phase 2 — Persistence + event core 🟡
- 🟡 `opencode-events` (`EventInput`/`StoredEvent`) + `opencode-db` `EventStore` trait + `MemoryEventStore` (optimistic concurrency via `expected_head`) + `opencode-core::Projector`/`project` fold — landed (in-memory); sqlx/SQLite + migration-compat + `session_context_epoch`/`session_input` next
- ⬜ Channels (`watch`/`async-broadcast`), SSE `/event`
- ⬜ Cutover: `agent`, `project`, `project-copy`, `credential`, `model`, `provider`, then `event`

### Phase 3 — LLM ⬜
- `opencode-llm` protocol router: anthropic-messages → openai-chat/responses → gemini → bedrock-converse; transports; executor (retry/redaction). Parity via recorded fixtures.

### Phase 4 — Session runner ⬜
- `session/runner/*` (loop via `TurnOutcome` + `ToolExecutor`), `permission`, `question`, `message`
- Cutover: `session`, `message`, `permission`, `question`

### Phase 5 — Plugins, MCP, integrations + instance routes ⬜
- JS plugin host (third-party) + `rmcp`; `background-job`, github/gitlab/slack
- Cutover: `integration` + the 21 instance route groups in `packages/opencode/src/server/routes/instance/httpapi/groups`

### Phase 6 — Decommission TS backend ⬜
- Remove proxy fallback; stop publishing `core/server/llm`; repoint the `opencode-ai` npm wrapper to Rust artifacts.

## Verification

- `cargo run -p xtask -- ci` = fmt --check + clippy -D warnings + nextest + cargo-deny
- `cargo run -p xtask -- openapi` emits the generated OpenAPI; `openapi-diff` checks it vs the golden spec
- Seam smoke: empty table → `/_rust/health` native, others proxied; `OPENCODE_RUST_ROUTES=health` → `/health` native
- Per route, before cutover: OpenAPI slice matches + differential parity (Rust vs proxied TS)
