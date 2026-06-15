# opencode — Backend Rust Migration Roadmap

Reference for the incremental migration of the opencode **backend** (core + server +
llm + utilities) from TypeScript/Bun to Rust. Frontends (tui/web/desktop/ui/app)
stay TypeScript. Strategy: **strangler-fig** behind the existing HTTP/OpenAPI contract.

> Status legend: ✅ done · 🟡 in progress · ⬜ not started

> **Branching (decided 2026-06-15):** cutover PRs target a long-lived **`rust-migration`** integration
> branch (NOT `dev`), promoted to `dev` only at deliberate milestones — because pushing to `dev`
> triggers `publish.yml` (release) + `deploy.yml` (deploy). PR #14 (Phase 0/1 + `global.health`) is
> **merged into `rust-migration`**. Merge cadence: ~weekly per cutover PR; CI is the gate.

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
- ✅ Reverse-proxy seam (`OPENCODE_RUST_ROUTES`) + native `/_rust/health` + a request-level integration test (axum `oneshot`: native route → 200, un-migrated route → proxy fallback → 502)
- ✅ Code-first OpenAPI (utoipa) + `xtask` (`ci` / `openapi` / `openapi-diff`)
- ✅ Error contract: `AppError` → HTTP status + error envelope in `opencode-server` (NB: the placeholder uses a `_tag` shape, but the real Effect HttpApi typed errors are `{name, data}` — see `BadRequestError` in `opencode-proto`; the `ApiError` mapping will adopt `{name, data}` as typed-error routes are cut over)
- ✅ `rust.yml` CI (fmt + clippy -D warnings + nextest + cargo-deny + openapi gate), side-by-side with TS CI
- ✅ `openapi-diff` contract gate (**structural**): per-operation compare of operationId + response status codes + **structurally-normalized response schemas** (resolves `$ref`, drops nullable/format/enum/description/additionalProperties), so `$ref`-vs-inline representations compare correctly. Hard-fails for `CUTOVER_PATHS` against `packages/sdk/openapi.json`.
- ⬜ Audit the MCP patch (`patches/@modelcontextprotocol%2Fsdk@1.29.0.patch`, reconnect/`onsessionexpired`)
- ⬜ Spikes: `die`/`catchDefect` → `TurnOutcome` enum; `FiberSet` → `ToolExecutor`; `state.ts` `Draft<T>`/replay
- ⬜ Pin `effect@4.0.0-beta.74`; align with the in-flight V2 refactor (`specs/v2`)

### Phase 1 — Leaf / low-risk modules 🟡
- 🟡 `opencode-config`: JSONC loader (`jsonc-parser`, comment/trailing-comma parity) + typed `Config` (top-level V1 subset; unmodeled keys preserved via `extra`) — landed; remaining config submodules in progress
- 🟡 `opencode-tools`: `read`/`glob`/`grep` (ripgrep libs), `write`/`edit`/`ls`, `process::run_command` + `run_shell` (shell exec + output cap — bash-tool base), `git` (shells out to the `git` binary — faithful to `git.ts`; no `gix` dep) — landed; PTY next
- ✅ **First real route cutover** (merged): `GET /global/health` (group `global`) — native axum handler matches the golden contract (operationId/responses/schemas), enforced by `openapi-diff` (`CUTOVER_PATHS`); seam verified (native 200 vs proxied 502).
- 🟡 Cutovers (batch PR into `rust-migration`), all contract-enforced: **`/path`** (`path.get`), **`/find/file`** (`find.files`), **`/find`** (`find.text`, reuses `grep_detailed` w/ submatches+offsets) — GET, 400 `BadRequestError`; and **`POST /log`** (`app.log`, first POST/request-body route; 400 is the mutation-route union `anyOf[effect_HttpApiError_BadRequest, InvalidRequestError]`, modeled once as `RequestError` and reusable). The gate now treats `oneOf`≡`anyOf` (order-independent unions). **Finding:** GET routes use a `BadRequestError` 400 while mutation routes use that union. Next: `config.get` (model `Config`), `app.agents`/`command.list` (enumeration logic).

### Phase 2 — Persistence + event core 🟡
- ✅ Event store: `opencode-events` reconciled to the real `event` columns (`id`/`aggregate_id`/`seq`/`type`/`data`); `EventStore` trait + `MemoryEventStore` + **`SqlxEventStore`** (sqlx + SQLite, reuses the exact DDL, WAL + foreign keys, optimistic concurrency, persists on disk across reopen) + `opencode-core::Projector`/`project`.
- ✅ **Dependency injection** (PR #17): `AppContext` carries `Arc<dyn EventStore>`, assembled in a single composition-root `build_app_context()` over **one shared `sqlx::SqlitePool`** (TS PRAGMAs: WAL + `synchronous=NORMAL` + busy-timeout + 64 MiB cache + foreign keys, applied per-connection); `AppContext::in_memory()` for tests. `Database` mints the event store + session repos from the shared pool.
- ✅ **Migration-journal verification** (PR #17, "TS migrates / Rust verifies"): boot health check reads the `migration` journal (+ **read-only** `__drizzle_migrations` bridge) and diffs against `EXPECTED_MIGRATIONS` (mirrors `migration.gen.ts`). Behind+journal-present → fatal with a clear message; ahead/no-journal → warn (policy documented in [`MIGRATION_POLICY.md`](./MIGRATION_POLICY.md), owner-approved).
- ✅ **Session-store tables** (PR #17): `session_input` (event-sourced steering inbox) + `session_context_epoch` (per-session baseline/agent) models + repositories in `opencode-db`, DDL mirroring the final TS schema. Next: first session/event **read** route cutover (e.g. `session.list`) wiring a repo through `AppContext`.
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
