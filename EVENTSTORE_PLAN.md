# EventStore (Phase 2) — preview for review (@danzeroum)

Preview of the `sqlx`+SQLite-backed `EventStore`, focused on **reusing the TS DDL** and the
**migration journal**, and on **TS ↔ Rust coexistence safety**. Requested before implementation.
Source of truth: `packages/core/src/event/sql.ts` and `packages/core/src/database/migration.ts`.

## 1. Reuse the exact TS DDL (on-disk compatibility)

Reproduce the existing schema verbatim so a DB written by the TS server is read/written by Rust and
vice-versa (no new tables, same names/columns/indexes):

- **`event_sequence`**: `aggregate_id TEXT PRIMARY KEY NOT NULL`, `seq INTEGER NOT NULL`, `owner_id TEXT`.
- **`event`**: `id TEXT PRIMARY KEY` (event ULID), `aggregate_id TEXT NOT NULL REFERENCES event_sequence(aggregate_id) ON DELETE CASCADE`, `seq INTEGER NOT NULL`, `type TEXT NOT NULL`, `data TEXT NOT NULL` (JSON).
  - `UNIQUE INDEX event_aggregate_seq_idx (aggregate_id, seq)`
  - `INDEX event_aggregate_type_seq_idx (aggregate_id, type, seq)`

**Event-model reconciliation (heads-up):** the current in-memory `StoredEvent` is a simplification
(`version`/`replay` fields). The real persisted columns are `id, aggregate_id, seq, type, data`.
The version is encoded in `type` (the registry's `type.N` convention) and `replay` is a *runtime*
flag (not persisted). The proto/event types will be reconciled to the real columns for the sqlx impl.

## 2. Migration journal compatibility

The TS journal is a `migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)` table.
`database/migration.ts` does: if `session` table exists → apply only *pending* migrations; if empty →
create the full schema + record all migration ids as a baseline; and a **one-time bridge** that seeds
`migration` from Drizzle's `__drizzle_migrations.name` so old installs don't replay old SQL.

**Rust strategy (coexistence-safe):**
- Read/write the **same `migration` table** (same shape) and understand the `__drizzle_migrations` bridge.
- **During coexistence, TS remains the single owner of schema evolution.** Rust does **not** apply
  migrations; on startup it opens the existing (TS-created) DB and **verifies** the recorded migration
  set matches the version it expects, erroring loudly if the DB is ahead/behind rather than mutating
  the schema. This avoids dual-writer migration races.
- Full migration-apply capability in Rust (porting `schema.gen` + the dated migrations, plus the
  bridge) is deferred to **Phase 6** (when Rust owns a standalone DB).

## 3. EventStore impl (`SqlxEventStore`, same trait as `MemoryEventStore`)

- `append(aggregate_id, expected_head, events)`: `BEGIN IMMEDIATE` → read `event_sequence.seq` →
  if `!= expected_head` → `DbError::Conflict` → else INSERT events at `seq = head+1..` and UPSERT
  `event_sequence.seq`. A `UNIQUE(aggregate_id, seq)` violation is also mapped to `Conflict` (backstop
  for the same race the TS server handles). `COMMIT`.
- `read(aggregate_id, from_seq)`: `SELECT ... WHERE aggregate_id=? AND seq>? ORDER BY seq`.
- `head_seq(aggregate_id)`: `SELECT seq FROM event_sequence WHERE aggregate_id=?`.

## 4. Coexistence safety (TS ↔ Rust on the same file)

- **WAL mode** (already used by TS) + `busy_timeout` (e.g. 5s): many readers + one writer; SQLite
  serializes writers.
- **Correctness under concurrent writers is guaranteed by `UNIQUE(aggregate_id, seq)` + optimistic
  retry** — the loser gets a constraint error → `Conflict` → retry, exactly as the TS server already
  behaves. No corruption even if both services write.
- **To avoid churn**, cut over *write* paths per aggregate atomically (a given session's writes go to
  one service at a time via the route table). Reads can be served by either.

## 5. sqlx / CI / cross-compile

- `sqlx` 0.8 with `runtime-tokio` + `sqlite`. Use **runtime queries** (`query`/`query_as`), **not** the
  compile-time `query!` macros, so no `DATABASE_URL` is needed at build time (keeps CI simple).
- `libsqlite3-sys` (bundled SQLite, C) compiles fine on the CI ubuntu runner; musl/windows-arm
  cross-compile is handled in the **release** matrix (zig/cross), not the per-PR `rust` CI. License
  (MIT) added to `deny.toml` if not already allowed.

## 6. Tests (incl. the TS↔Rust compat test you asked about)

- Run the **same `EventStore` suite** as `MemoryEventStore` against a temp SQLite file, plus a
  contention test asserting `Conflict` on a stale `expected_head`.
- **Compat test**: create a DB with the exact TS DDL (+ a `migration` baseline, and a variant with a
  legacy `__drizzle_migrations` table to exercise the bridge), then round-trip an event via Rust and
  assert schema/journal handling. Ideally also open a fixture DB produced by the real TS server.

---
**Open question for you:** OK to keep TS as the sole schema-migrator during coexistence (Rust verifies,
doesn't migrate), porting full migrate-apply to Rust only at Phase 6? That's the safest TS↔Rust story.
