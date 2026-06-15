# Database migration policy (TS ↔ Rust coexistence)

During the incremental Rust backend migration the TypeScript server and the Rust binary run against
the **same SQLite database**. To avoid two writers fighting over schema evolution, the rule is:

> **TS migrates, Rust verifies.** The TypeScript server is the *single owner* of schema evolution —
> it applies migrations and records them in the `migration(id, time_completed)` journal
> (`packages/core/src/database/migration.ts`). The Rust binary **never applies migrations**; on boot
> it *reads* the journal and checks the database is compatible with the schema this build assumes.

Full Rust-applies migration support is deferred to **Phase 6** (decommissioning the TS backend), at
which point this policy is revisited (see [Post-coexistence](#post-coexistence)).

## Boot-time verification

`opencode-bin`'s composition root (`build_app_context()`) calls `Database::verify_migrations()`
before serving traffic. The verifier (`opencode-db::migration`) reads the applied-migration set and
diffs it against `EXPECTED_MIGRATIONS` — the ordered id list baked into the binary, mirroring
`packages/core/src/database/migration.gen.ts`.

Reading the applied set mirrors the TS `applyOnly` bridge: if the `migration` table is empty/absent
but a legacy Drizzle journal (`__drizzle_migrations`) exists, its migration `name`s count as applied
— **read-only** (Rust never writes the bridge rows; that stays TS's job).

| Situation | Meaning | Action |
|---|---|---|
| **DB behind** this build (missing expected migrations) **and** a journal is present | TS hasn't applied migrations this build requires; native routes could read/write a schema that doesn't exist yet | 🔴 **Fatal** — abort boot with a clear message ("start the TypeScript server to apply migrations") |
| **DB ahead** of this build (has migrations this build doesn't know) | TS shipped a newer migration than this Rust build understands | 🟡 **Warn** — boot proceeds; native routes assume this build's schema |
| **No journal** (fresh / uninitialized DB) | Nothing to compare; TS owns schema *creation* | 🟡 **Warn** — boot proceeds (Rust only auto-creates the `event`/`event_sequence` tables it needs) |
| **Journal exactly matches** `EXPECTED_MIGRATIONS` | In sync | ✅ Info log, proceed |

### Why *ahead* only warns (not fatal)

During coexistence TS is frequently a migration or two ahead (it ships first). Making *ahead* fatal
would mean the Rust binary refuses to boot every time TS publishes a new migration — operationally
unacceptable. Native Rust routes only touch the tables their build knows; an additive migration TS
applied elsewhere doesn't endanger them. *Behind*, by contrast, is genuinely unsafe (a route could
assume a column/table that isn't there), so it is fatal.

> **Note (current phase):** the routes cut over so far (`health`, `path`, `find.*`, `app.log`) don't
> touch the database at all, so the verifier is *preparatory* — the safety gate that protects the
> session/event read/write cutovers (PR #18+).

## Keeping `EXPECTED_MIGRATIONS` in sync

When TS adds a migration (a new entry in `migration.gen.ts`), append its id to
`EXPECTED_MIGRATIONS` in `crates/opencode-db/src/migration.rs`. A unit test enforces the list is
unique and chronologically ordered. Until the id is added, a fully-migrated TS database will show
that migration as *ahead* (a warning) — visible but non-fatal.

## Post-coexistence

Once the TS backend is decommissioned (Phase 6) and Rust owns schema evolution:

1. Rust gains a migration **runner** (applies, not just verifies), re-expressing the TS migration
   SQL so existing user databases upgrade without replay.
2. The policy can tighten to **strict in both directions** — an unrecognized (ahead) migration
   becomes an error too, since there is no longer a second writer legitimately racing ahead.

Until then, this asymmetric policy (behind = fatal, ahead = warn) is the safe default for two
cooperating processes sharing one database.
