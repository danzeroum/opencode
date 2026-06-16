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
- 🟡 Spikes: ✅ `die`/`catchDefect` → explicit restart driver + `FiberSet` → `ToolExecutor` landed (PR #24, `opencode-core::runner`); ⬜ `state.ts` `Draft<T>`/replay still open
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
- ✅ **Session-store tables** (PR #17): `session_input` (event-sourced steering inbox) + `session_context_epoch` (per-session baseline/agent) models + repositories in `opencode-db`, DDL mirroring the final TS schema.
- ✅ **First session read-route cutover** (PR #18): `GET /api/session/{sessionID}` (`v2.session.get`, group `session`) → `SessionV2Info`, served natively by reading the shared **`session` projection table** via a `SessionStore` wired through `AppContext`. **Finding:** the V2 read routes read projection tables (the projector folds events into them on the *write* path), not the event log per request — so a faithful Rust read is a `SELECT`, not an event fold. `session.status` is **not** cuttable yet (it reads per-process in-memory runner state, which lives in TS until Phase 4). Contract enforced by `openapi-diff` (now collapses `anyOf[X,X]` and drops the `{type:null}` member utoipa adds for `Option<$ref>`); seam-gated (`OPENCODE_RUST_ROUTES=session`, off by default). A separate test exercises `event_store.read()` against externally (TS-)written event rows.
- ✅ **Session list cutover** (PR #19): `GET /api/session` (`v2.session.list`, group `session`) → `SessionsResponse` (`{ data: SessionV2Info[], cursor }`), same `SessionStore` with `limit`/`order`/`search`/`project`/`workspace`/`directory` filters. Contract-enforced (`/api/session`); the 400 union (`anyOf[InvalidCursorError, InvalidRequestError]`) reuses the dedup-collapsing normalizer.
- ✅ **Keyset cursor pagination** (PR #20): `v2.session.list` now returns real `previous`/`next` cursors and follows an inbound `cursor`. Faithful TS keyset semantics (`session.ts`): anchor `(time_created, id)`, order-flip + result-reverse for `previous`, default limit 50; an undecodable cursor → 400 `InvalidCursorError`. The cursor token is a Rust-native `base64url(JSON)` (opaque per the contract; not byte-interchangeable with TS cursors across a mid-pagination rollback — documented). Added the `base64` crate.
- ✅ **Second entity read cutover** (PR #21): `GET /project` (`project.list`, group `project`) → `array<Project>`, served natively from the shared **`project` projection table** via a new `ProjectStore` — proving the `AppContext → Store → projection` pattern generalizes beyond sessions (`icon_*` columns fold into `icon`; `sandboxes`/`commands` are JSON). `directory`/`workspace` scoping is a documented follow-up (returns all projects). `AppContext` now takes an **`AppServices`** bundle (`Default` = in-memory) so adding a store doesn't churn call sites. Contract-enforced (`/project`, 400 reuses `BadRequestError`).
- ✅ **Project-by-worktree lookup** (PR #23): `GET /project/current` (`project.current`, group `project`) → single `Project`, resolving `directory` (param/cwd) → git worktree (reusing `opencode_tools::git::root`) → `ProjectStore::get_by_worktree` — a non-PK lookup keyed by directory context. Reuses the `Project` schema (no new types). TS remote-id derivation + non-repo "global" fallback are documented follow-ups (unknown worktree → 400).
- ✅ **Event-bus infrastructure** (PR #22, infra — *not* a cutover): `opencode-effect::bus::EventBus` — global stream via **`async-broadcast`** (overflow/drop-oldest so a stalled SSE client never blocks producers) + per-aggregate sliding-1 via **`tokio::sync::watch`**, wired into `AppContext` (`AppServices.event_bus`). An always-native internal SSE route `/_rust/event` streams it (proving the SSE+channel plumbing end-to-end). **Finding:** the contract `/event` is fed by an in-process PubSub (`event.ts`), so a native cutover would see none of the TS runner's events cross-process (empty stream, like `session.status`) — it stays **proxied to TS** (the proxy already streams SSE) until the runner is in Rust (Phase 4). This bus is that runner's outlet and de-risks plan risk #3 (PubSub/backpressure).
- ⬜ Cutover: `agent`, `project`, `project-copy`, `credential`, `model`, `provider`, then `event` (the latter once Rust produces events)

### Phase 3 — LLM 🟡
- ✅ **Protocol decode pipeline** (PR #25): `opencode-llm` defines the normalized `LlmEvent`/`Usage`/`FinishReason` model + the `Protocol` trait (decode side: `decode_frame` → `step(state) → Vec<LlmEvent>` → `terminal`) + `sse_frames`/`decode_sse` (the `sseFraming` analog). First impl: **anthropic-messages** — folds the Anthropic SSE stream (`message_start`/`content_block_*`/`message_delta`/`message_stop`/`ping`/`error`) into normalized events, threading open blocks + streaming tool-arg buffers + merged usage. **Parity proven network-free** by replaying real `http-recorder` cassettes (`streams-text`, `streams-tool-call`) and asserting the event stream (text, tool-call input `{"city":"Paris"}`, finish reason, token usage).
- ⬜ Next: anthropic-messages **request lowering** (`body.from`) + the reqwest/SSE **transport** (needs a TLS backend re-added); then `route/executor.ts` (retry/backoff + redaction); then openai-chat → openai-responses → gemini → bedrock-converse. No public cutover (the runner consumes it; Phase 4).

### Phase 4 — Session runner 🟡
- ✅ **Control-flow spike** (PR #24, pure logic — no IO/cutover): `opencode-core::runner` proves the `session/runner/llm.ts` control flow maps to panic-free Rust. `die(TurnTransitionError)`/`catchDefect` → `TurnTransition` (`RebuildPreparedTurn{promotion}` / `ContinueAfterOverflowCompaction`) returned as `Err` + the `run_turn` restart driver (flips `OverflowRecovery` `Enabled→Disabled`; a second overflow is `DoubleOverflow`); `needsContinuation` → `TurnOutcome::{Continue,Done}`; the outer continuation loop → `run_session` (step-limited); `FiberSet` + `raceFirst(join,awaitEmpty)` → `ToolExecutor` (over `tokio::task::JoinSet`) with `drain` (fail-fast vs all-settled) + `cancel_all`. 14 unit tests exercise every transition + tool race/timeout/cancel.
- ⬜ Port the real `session/runner/*` over this skeleton (LLM stream → persist incrementally → tool exec via `ToolExecutor` → project history → next turn), `permission`, `question`, `message`; depends on Phase 3 (LLM) + the `session_context_epoch`/`session_input` tables.
- ⬜ Cutover: `session`, `message`, `permission`, `question`, and finally the real `/event` (Rust now produces events)

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
