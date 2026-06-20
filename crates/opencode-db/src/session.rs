//! Session-store repositories for the two event-sourced tables the session runner depends on:
//! `session_input` (the event-sourced steering inbox) and `session_context_epoch` (per-session
//! context baseline that detects mid-run agent/model changes).
//!
//! The DDL mirrors the final TypeScript schema (`packages/core/src/session/sql.ts` plus migrations
//! `…_event_sourced_session_input` and `…_add_context_epoch_agent`). TS owns schema creation during
//! the migration, so these tables are **not** auto-created by [`crate::Database::connect`]; the
//! [`SESSION_INPUT_DDL`]/[`SESSION_CONTEXT_EPOCH_DDL`] constants exist for tests and for a future
//! Rust-applies path. The repositories assume the tables already exist (created by TS).

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::DbError;

/// `session_input` DDL (final shape, post `event_sourced_session_input`). FK → `session(id)`.
pub const SESSION_INPUT_DDL: &str = "\
CREATE TABLE IF NOT EXISTS session_input (\
  id text PRIMARY KEY,\
  session_id text NOT NULL,\
  prompt text NOT NULL,\
  delivery text NOT NULL,\
  admitted_seq integer NOT NULL,\
  promoted_seq integer,\
  time_created integer NOT NULL,\
  CONSTRAINT fk_session_input_session_id_session_id_fk FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE\
);\
CREATE INDEX IF NOT EXISTS session_input_session_pending_delivery_seq_idx ON session_input (session_id, promoted_seq, delivery, admitted_seq);\
CREATE UNIQUE INDEX IF NOT EXISTS session_input_session_admitted_seq_idx ON session_input (session_id, admitted_seq);\
CREATE UNIQUE INDEX IF NOT EXISTS session_input_session_promoted_seq_idx ON session_input (session_id, promoted_seq);";

/// `session_context_epoch` DDL (final shape, with the `agent` column). FK → `session(id)`.
pub const SESSION_CONTEXT_EPOCH_DDL: &str = "\
CREATE TABLE IF NOT EXISTS session_context_epoch (\
  session_id text PRIMARY KEY,\
  baseline text NOT NULL,\
  agent text DEFAULT 'build' NOT NULL,\
  snapshot text NOT NULL,\
  baseline_seq integer NOT NULL,\
  replacement_seq integer,\
  revision integer DEFAULT 0 NOT NULL,\
  CONSTRAINT fk_session_context_epoch_session_id_session_id_fk FOREIGN KEY (session_id) REFERENCES session(id) ON DELETE CASCADE\
);";

/// A row of the event-sourced steering inbox (`session_input`). A row is *pending* while
/// `promoted_seq` is `NULL` (admitted but not yet folded into a turn).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionInput {
    /// Input id (a `SessionMessage` id).
    pub id: String,
    /// Owning session.
    pub session_id: String,
    /// The prompt payload (JSON `prompt` column).
    pub prompt: Value,
    /// Delivery mode (e.g. `"steer"`).
    pub delivery: String,
    /// Monotonic admit sequence (unique per session).
    pub admitted_seq: i64,
    /// Promotion sequence once folded into a turn; `None` while pending.
    pub promoted_seq: Option<i64>,
    /// Creation timestamp (ms since epoch).
    pub time_created: i64,
}

/// Columns selected for [`SessionInput`], in struct order.
const SESSION_INPUT_COLS: &str =
    "id, session_id, prompt, delivery, admitted_seq, promoted_seq, time_created";

type SessionInputRow = (String, String, String, String, i64, Option<i64>, i64);

fn decode_input(row: SessionInputRow) -> Result<SessionInput, DbError> {
    let (id, session_id, prompt, delivery, admitted_seq, promoted_seq, time_created) = row;
    Ok(SessionInput {
        id,
        session_id,
        prompt: serde_json::from_str(&prompt)?,
        delivery,
        admitted_seq,
        promoted_seq,
        time_created,
    })
}

/// Repository over `session_input`, backed by the shared pool.
pub struct SessionInputRepo {
    pool: SqlitePool,
}

impl SessionInputRepo {
    /// Wrap the shared pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert a steering input.
    pub async fn insert(&self, input: &SessionInput) -> Result<(), DbError> {
        let prompt = serde_json::to_string(&input.prompt)?;
        sqlx::query(
            "INSERT INTO session_input \
             (id, session_id, prompt, delivery, admitted_seq, promoted_seq, time_created) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&input.id)
        .bind(&input.session_id)
        .bind(&prompt)
        .bind(&input.delivery)
        .bind(input.admitted_seq)
        .bind(input.promoted_seq)
        .bind(input.time_created)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch a single input by id.
    pub async fn get(&self, id: &str) -> Result<Option<SessionInput>, DbError> {
        let row: Option<SessionInputRow> = sqlx::query_as(&format!(
            "SELECT {SESSION_INPUT_COLS} FROM session_input WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(decode_input).transpose()
    }

    /// List the *pending* inputs for a session (`promoted_seq IS NULL`), oldest first.
    pub async fn list_pending(&self, session_id: &str) -> Result<Vec<SessionInput>, DbError> {
        let rows: Vec<SessionInputRow> = sqlx::query_as(&format!(
            "SELECT {SESSION_INPUT_COLS} FROM session_input \
             WHERE session_id = ? AND promoted_seq IS NULL ORDER BY admitted_seq"
        ))
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(decode_input).collect()
    }
}

/// A per-session context epoch (`session_context_epoch`): the baseline the runner projects from, and
/// the agent/snapshot it was captured under (used to detect a mid-run agent/model change).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SessionContextEpoch {
    /// Owning session (primary key).
    pub session_id: String,
    /// Opaque baseline marker.
    pub baseline: String,
    /// Agent id this epoch was captured under (defaults to `build`).
    pub agent: String,
    /// System-context snapshot (JSON `snapshot` column).
    pub snapshot: Value,
    /// Event seq the baseline was taken at.
    pub baseline_seq: i64,
    /// Event seq a replacement epoch starts at; `None` while current.
    pub replacement_seq: Option<i64>,
    /// Monotonic revision counter.
    pub revision: i64,
}

type ContextEpochRow = (String, String, String, String, i64, Option<i64>, i64);

const CONTEXT_EPOCH_COLS: &str =
    "session_id, baseline, agent, snapshot, baseline_seq, replacement_seq, revision";

fn decode_epoch(row: ContextEpochRow) -> Result<SessionContextEpoch, DbError> {
    let (session_id, baseline, agent, snapshot, baseline_seq, replacement_seq, revision) = row;
    Ok(SessionContextEpoch {
        session_id,
        baseline,
        agent,
        snapshot: serde_json::from_str(&snapshot)?,
        baseline_seq,
        replacement_seq,
        revision,
    })
}

/// Repository over `session_context_epoch`, backed by the shared pool.
pub struct SessionContextEpochRepo {
    pool: SqlitePool,
}

impl SessionContextEpochRepo {
    /// Wrap the shared pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Insert or replace the epoch for a session (`session_id` is the primary key).
    pub async fn upsert(&self, epoch: &SessionContextEpoch) -> Result<(), DbError> {
        let snapshot = serde_json::to_string(&epoch.snapshot)?;
        sqlx::query(
            "INSERT INTO session_context_epoch \
             (session_id, baseline, agent, snapshot, baseline_seq, replacement_seq, revision) \
             VALUES (?, ?, ?, ?, ?, ?, ?) \
             ON CONFLICT(session_id) DO UPDATE SET \
               baseline = excluded.baseline, agent = excluded.agent, snapshot = excluded.snapshot, \
               baseline_seq = excluded.baseline_seq, replacement_seq = excluded.replacement_seq, \
               revision = excluded.revision",
        )
        .bind(&epoch.session_id)
        .bind(&epoch.baseline)
        .bind(&epoch.agent)
        .bind(&snapshot)
        .bind(epoch.baseline_seq)
        .bind(epoch.replacement_seq)
        .bind(epoch.revision)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Fetch the epoch for a session.
    pub async fn get(&self, session_id: &str) -> Result<Option<SessionContextEpoch>, DbError> {
        let row: Option<ContextEpochRow> = sqlx::query_as(&format!(
            "SELECT {CONTEXT_EPOCH_COLS} FROM session_context_epoch WHERE session_id = ?"
        ))
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(decode_epoch).transpose()
    }
}

/// A row of the `session` **projection table** — the materialized session state the V2 read routes
/// serve (the `SessionProjector` folds session events into this table on the write path; reads are a
/// plain `SELECT`). Only the columns needed to build the `v2.session.get` response are read, so this
/// stays compatible with the (wider) TS-owned `session` table. Mirrors `session/info.ts` `fromRow`.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionRecord {
    /// Session id (`ses_…`).
    pub id: String,
    /// Owning project id.
    pub project_id: String,
    /// Parent session id, if a child session.
    pub parent_id: Option<String>,
    /// Agent id, if set.
    pub agent: Option<String>,
    /// Model JSON (`{ id, providerID, variant? }`), if set.
    pub model: Option<Value>,
    /// Accumulated cost.
    pub cost: f64,
    /// Input tokens.
    pub tokens_input: i64,
    /// Output tokens.
    pub tokens_output: i64,
    /// Reasoning tokens.
    pub tokens_reasoning: i64,
    /// Cache-read tokens.
    pub tokens_cache_read: i64,
    /// Cache-write tokens.
    pub tokens_cache_write: i64,
    /// Session title.
    pub title: String,
    /// Absolute working directory.
    pub directory: String,
    /// Workspace id, if any.
    pub workspace_id: Option<String>,
    /// Sub-path within the workspace, if any.
    pub path: Option<String>,
    /// Creation time (ms).
    pub time_created: i64,
    /// Last-updated time (ms).
    pub time_updated: i64,
    /// Archival time (ms), if archived.
    pub time_archived: Option<i64>,
}

/// Columns read for [`SessionRecord`] (explicit, so the wider TS `session` table is fine).
const SESSION_RECORD_COLS: &str = "id, project_id, parent_id, agent, model, cost, tokens_input, \
     tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, title, directory, \
     workspace_id, path, time_created, time_updated, time_archived";

/// Pagination direction within a keyset cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ListDirection {
    /// The page after the anchor (default).
    #[default]
    Next,
    /// The page before the anchor.
    Previous,
}

/// A keyset pagination anchor: the `(time_created, id)` position to page from, plus the direction.
/// Mirrors the TS `ListAnchor` (`session.ts`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListAnchor {
    /// Anchor session id (the `id` tiebreaker).
    pub id: String,
    /// Anchor `time_created` (ms).
    pub time: i64,
    /// Page before or after the anchor.
    pub direction: ListDirection,
}

/// Filters/ordering for [`SessionStore::list`]. All filters are optional (`None` = no filter).
#[derive(Debug, Clone, Default)]
pub struct SessionListQuery {
    /// Max rows to return (`None` = no limit).
    pub limit: Option<i64>,
    /// Order by `time_created` (then `id`) descending — most-recent first — when `true`; ascending
    /// otherwise. (The requested order; for a `Previous` anchor the query is flipped internally.)
    pub descending: bool,
    /// ASCII-case-insensitive substring match on `title`.
    pub search: Option<String>,
    /// Restrict to a project id.
    pub project: Option<String>,
    /// Restrict to a workspace id.
    pub workspace: Option<String>,
    /// Restrict to a directory.
    pub directory: Option<String>,
    /// Keyset anchor for cursor pagination (`None` = first page).
    pub anchor: Option<ListAnchor>,
}

impl SessionListQuery {
    /// Whether this query pages *before* its anchor.
    fn is_previous(&self) -> bool {
        matches!(
            self.anchor.as_ref().map(|a| a.direction),
            Some(ListDirection::Previous)
        )
    }

    /// The effective sort order applied to the SQL/in-memory query: a `Previous` anchor flips the
    /// requested order (then results are reversed back), so the page is the rows immediately before
    /// the anchor in the requested order.
    fn effective_descending(&self) -> bool {
        if self.is_previous() {
            !self.descending
        } else {
            self.descending
        }
    }
}

/// The full V1 session row (`packages/core/src/v1/session.ts` `Session`) — every column the V1
/// `Session` wire type needs (superset of [`SessionRecord`]). Read by the mutation routes
/// (`session.update`/`share`/`revert`/…), which return the whole session. JSON columns
/// (`model`/`metadata`/`permission`/`revert`/`summary_diffs`) stay as parsed `Value`s; the server maps
/// them into the typed wire shape.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionV1Record {
    /// Session id.
    pub id: String,
    /// URL-safe slug.
    pub slug: String,
    /// Owning project id.
    pub project_id: String,
    /// Workspace id, if any.
    pub workspace_id: Option<String>,
    /// Working directory.
    pub directory: String,
    /// Session path, if any.
    pub path: Option<String>,
    /// Parent session id, if a child.
    pub parent_id: Option<String>,
    /// Change-summary counts `(additions, deletions, files)`, if computed.
    pub summary: Option<(i64, i64, i64)>,
    /// Per-file diffs JSON, if computed.
    pub summary_diffs: Option<Value>,
    /// Accumulated cost.
    pub cost: f64,
    /// Token usage `(input, output, reasoning, cache_read, cache_write)`.
    pub tokens: (i64, i64, i64, i64, i64),
    /// Share URL, if shared.
    pub share_url: Option<String>,
    /// Title.
    pub title: String,
    /// Active agent, if set.
    pub agent: Option<String>,
    /// Model JSON (`{ id, providerID, variant? }`), if set.
    pub model: Option<Value>,
    /// App/schema version that wrote the row.
    pub version: String,
    /// Free-form metadata JSON, if set.
    pub metadata: Option<Value>,
    /// Creation time (ms).
    pub time_created: i64,
    /// Last-updated time (ms).
    pub time_updated: i64,
    /// Compaction-in-progress time (ms), if any.
    pub time_compacting: Option<i64>,
    /// Archival time (ms), if archived.
    pub time_archived: Option<i64>,
    /// Permission ruleset JSON (`[PermissionRule]`), if set.
    pub permission: Option<Value>,
    /// Revert pointer JSON, if reverted.
    pub revert: Option<Value>,
}

/// Read-only store over the `session` projection table.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Fetch a session by id, or `None` if it doesn't exist.
    async fn get(&self, id: &str) -> Result<Option<SessionRecord>, DbError>;

    /// List sessions matching `query`, ordered by `time_created` then `id` per `query.descending`.
    async fn list(&self, query: &SessionListQuery) -> Result<Vec<SessionRecord>, DbError>;

    /// Fetch the full V1 session row (every `Session` column), or `None`. Backs the mutation routes
    /// that return the whole session.
    async fn get_full(&self, id: &str) -> Result<Option<SessionV1Record>, DbError>;

    /// Insert a new session row (all `Session` columns). Errors on a duplicate id or a project-FK
    /// violation. Backs `session.create`.
    async fn create(&self, record: &SessionV1Record) -> Result<(), DbError>;

    /// List sessions as full V1 records (every `Session` column), applying the same filters/order as
    /// [`list`](Self::list). Default impl = `list` then `get_full` per id, so both stores share it.
    /// Backs the V1 `session.list`.
    async fn list_full(&self, query: &SessionListQuery) -> Result<Vec<SessionV1Record>, DbError> {
        let mut out = Vec::new();
        for r in self.list(query).await? {
            if let Some(full) = self.get_full(&r.id).await? {
                out.push(full);
            }
        }
        Ok(out)
    }

    /// Apply a partial update to a session's mutable fields (any of `title`/`metadata`/`permission`;
    /// `None` leaves a field unchanged) and bump `time_updated`. Returns whether the session existed.
    async fn update(
        &self,
        id: &str,
        title: Option<&str>,
        metadata: Option<&Value>,
        permission: Option<&Value>,
    ) -> Result<bool, DbError>;

    /// Set (`Some`) or clear (`None`) a session's `revert` pointer and bump `time_updated`. Returns
    /// whether the session existed. Backs `session.revert`/`session.unrevert`.
    async fn set_revert(&self, id: &str, revert: Option<&Value>) -> Result<bool, DbError>;
}

/// Columns read for [`SessionV1Record`].
const SESSION_V1_COLS: &str = "id, slug, project_id, workspace_id, directory, path, parent_id, \
     summary_additions, summary_deletions, summary_files, summary_diffs, cost, tokens_input, \
     tokens_output, tokens_reasoning, tokens_cache_read, tokens_cache_write, share_url, title, agent, \
     model, version, metadata, time_created, time_updated, time_compacting, time_archived, \
     permission, revert";

fn json_col(row: &SqliteRow, name: &str) -> Result<Option<Value>, DbError> {
    let raw: Option<String> = row.try_get(name)?;
    Ok(raw.map(|s| serde_json::from_str(&s)).transpose()?)
}

fn v1_record_from_row(row: &SqliteRow) -> Result<SessionV1Record, DbError> {
    let summary_additions: Option<i64> = row.try_get("summary_additions")?;
    let summary = summary_additions.map(|additions| {
        (
            additions,
            row.try_get("summary_deletions").unwrap_or(0),
            row.try_get("summary_files").unwrap_or(0),
        )
    });
    Ok(SessionV1Record {
        id: row.try_get("id")?,
        slug: row.try_get("slug")?,
        project_id: row.try_get("project_id")?,
        workspace_id: row.try_get("workspace_id")?,
        directory: row.try_get("directory")?,
        path: row.try_get("path")?,
        parent_id: row.try_get("parent_id")?,
        summary,
        summary_diffs: json_col(row, "summary_diffs")?,
        cost: row.try_get("cost")?,
        tokens: (
            row.try_get("tokens_input")?,
            row.try_get("tokens_output")?,
            row.try_get("tokens_reasoning")?,
            row.try_get("tokens_cache_read")?,
            row.try_get("tokens_cache_write")?,
        ),
        share_url: row.try_get("share_url")?,
        title: row.try_get("title")?,
        agent: row.try_get("agent")?,
        model: json_col(row, "model")?,
        version: row.try_get("version")?,
        metadata: json_col(row, "metadata")?,
        time_created: row.try_get("time_created")?,
        time_updated: row.try_get("time_updated")?,
        time_compacting: row.try_get("time_compacting")?,
        time_archived: row.try_get("time_archived")?,
        permission: json_col(row, "permission")?,
        revert: json_col(row, "revert")?,
    })
}

/// Wall-clock milliseconds since the epoch.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Build a best-effort [`SessionV1Record`] from a (V2) [`SessionRecord`] — used by the in-memory test
/// double, which only stores the V2 columns. V1-only fields it doesn't track get placeholders
/// (`slug = id`, `version = "0.0.0"`, the rest empty); the Sqlx store reads the real columns.
fn v1_from_session_record(r: &SessionRecord) -> SessionV1Record {
    SessionV1Record {
        id: r.id.clone(),
        slug: r.id.clone(),
        project_id: r.project_id.clone(),
        workspace_id: r.workspace_id.clone(),
        directory: r.directory.clone(),
        path: r.path.clone(),
        parent_id: r.parent_id.clone(),
        summary: None,
        summary_diffs: None,
        cost: r.cost,
        tokens: (
            r.tokens_input,
            r.tokens_output,
            r.tokens_reasoning,
            r.tokens_cache_read,
            r.tokens_cache_write,
        ),
        share_url: None,
        title: r.title.clone(),
        agent: r.agent.clone(),
        model: r.model.clone(),
        version: "0.0.0".to_string(),
        metadata: None,
        time_created: r.time_created,
        time_updated: r.time_updated,
        time_compacting: None,
        time_archived: r.time_archived,
        permission: None,
        revert: None,
    }
}

/// Build a [`SessionRecord`] from a row. Read via `Row::try_get` (rather than a tuple) — the row has
/// >16 columns, and `model` needs JSON parsing from its TEXT column.
fn record_from_row(row: &SqliteRow) -> Result<SessionRecord, DbError> {
    let model: Option<String> = row.try_get("model")?;
    let model = model.map(|s| serde_json::from_str(&s)).transpose()?;
    Ok(SessionRecord {
        id: row.try_get("id")?,
        project_id: row.try_get("project_id")?,
        parent_id: row.try_get("parent_id")?,
        agent: row.try_get("agent")?,
        model,
        cost: row.try_get("cost")?,
        tokens_input: row.try_get("tokens_input")?,
        tokens_output: row.try_get("tokens_output")?,
        tokens_reasoning: row.try_get("tokens_reasoning")?,
        tokens_cache_read: row.try_get("tokens_cache_read")?,
        tokens_cache_write: row.try_get("tokens_cache_write")?,
        title: row.try_get("title")?,
        directory: row.try_get("directory")?,
        workspace_id: row.try_get("workspace_id")?,
        path: row.try_get("path")?,
        time_created: row.try_get("time_created")?,
        time_updated: row.try_get("time_updated")?,
        time_archived: row.try_get("time_archived")?,
    })
}

/// SQLite-backed [`SessionStore`] over the shared pool.
pub struct SqlxSessionStore {
    pool: SqlitePool,
}

impl SqlxSessionStore {
    /// Wrap the shared pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SessionStore for SqlxSessionStore {
    async fn get(&self, id: &str) -> Result<Option<SessionRecord>, DbError> {
        let Some(row) = sqlx::query(&format!(
            "SELECT {SESSION_RECORD_COLS} FROM session WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        record_from_row(&row).map(Some)
    }

    async fn list(&self, query: &SessionListQuery) -> Result<Vec<SessionRecord>, DbError> {
        let descending = query.effective_descending();
        // Build the filter clause dynamically, then bind in the same order.
        let mut sql = format!("SELECT {SESSION_RECORD_COLS} FROM session");
        let mut conds: Vec<&str> = Vec::new();
        if query.directory.is_some() {
            conds.push("directory = ?");
        }
        if query.project.is_some() {
            conds.push("project_id = ?");
        }
        if query.workspace.is_some() {
            conds.push("workspace_id = ?");
        }
        if query.search.is_some() {
            conds.push("title LIKE ?");
        }
        if query.anchor.is_some() {
            // Keyset boundary on `(time_created, id)` relative to the anchor.
            conds.push(if descending {
                "(time_created < ? OR (time_created = ? AND id < ?))"
            } else {
                "(time_created > ? OR (time_created = ? AND id > ?))"
            });
        }
        if !conds.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conds.join(" AND "));
        }
        sql.push_str(if descending {
            " ORDER BY time_created DESC, id DESC"
        } else {
            " ORDER BY time_created ASC, id ASC"
        });
        if query.limit.is_some() {
            sql.push_str(" LIMIT ?");
        }

        let mut q = sqlx::query(&sql);
        if let Some(d) = &query.directory {
            q = q.bind(d);
        }
        if let Some(p) = &query.project {
            q = q.bind(p);
        }
        if let Some(w) = &query.workspace {
            q = q.bind(w);
        }
        if let Some(s) = &query.search {
            q = q.bind(format!("%{s}%"));
        }
        if let Some(a) = &query.anchor {
            q = q.bind(a.time).bind(a.time).bind(&a.id);
        }
        if let Some(l) = query.limit {
            q = q.bind(l);
        }

        let rows = q.fetch_all(&self.pool).await?;
        let mut records: Vec<SessionRecord> =
            rows.iter().map(record_from_row).collect::<Result<_, _>>()?;
        if query.is_previous() {
            records.reverse();
        }
        Ok(records)
    }

    async fn get_full(&self, id: &str) -> Result<Option<SessionV1Record>, DbError> {
        let Some(row) = sqlx::query(&format!(
            "SELECT {SESSION_V1_COLS} FROM session WHERE id = ?"
        ))
        .bind(id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        v1_record_from_row(&row).map(Some)
    }

    async fn create(&self, r: &SessionV1Record) -> Result<(), DbError> {
        let (sa, sd, sf) = match r.summary {
            Some((a, d, f)) => (Some(a), Some(d), Some(f)),
            None => (None, None, None),
        };
        let placeholders = ["?"; 29].join(", ");
        sqlx::query(&format!(
            "INSERT INTO session ({SESSION_V1_COLS}) VALUES ({placeholders})"
        ))
        .bind(r.id.as_str())
        .bind(r.slug.as_str())
        .bind(r.project_id.as_str())
        .bind(r.workspace_id.as_deref())
        .bind(r.directory.as_str())
        .bind(r.path.as_deref())
        .bind(r.parent_id.as_deref())
        .bind(sa)
        .bind(sd)
        .bind(sf)
        .bind(r.summary_diffs.as_ref().map(|v| v.to_string()))
        .bind(r.cost)
        .bind(r.tokens.0)
        .bind(r.tokens.1)
        .bind(r.tokens.2)
        .bind(r.tokens.3)
        .bind(r.tokens.4)
        .bind(r.share_url.as_deref())
        .bind(r.title.as_str())
        .bind(r.agent.as_deref())
        .bind(r.model.as_ref().map(|v| v.to_string()))
        .bind(r.version.as_str())
        .bind(r.metadata.as_ref().map(|v| v.to_string()))
        .bind(r.time_created)
        .bind(r.time_updated)
        .bind(r.time_compacting)
        .bind(r.time_archived)
        .bind(r.permission.as_ref().map(|v| v.to_string()))
        .bind(r.revert.as_ref().map(|v| v.to_string()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn update(
        &self,
        id: &str,
        title: Option<&str>,
        metadata: Option<&Value>,
        permission: Option<&Value>,
    ) -> Result<bool, DbError> {
        let mut sets: Vec<&str> = Vec::new();
        if title.is_some() {
            sets.push("title = ?");
        }
        if metadata.is_some() {
            sets.push("metadata = ?");
        }
        if permission.is_some() {
            sets.push("permission = ?");
        }
        // Nothing to change: just report whether the session exists.
        if sets.is_empty() {
            let found = sqlx::query("SELECT 1 FROM session WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?
                .is_some();
            return Ok(found);
        }
        sets.push("time_updated = ?");
        let sql = format!("UPDATE session SET {} WHERE id = ?", sets.join(", "));
        let mut q = sqlx::query(&sql);
        if let Some(t) = title {
            q = q.bind(t.to_string());
        }
        if let Some(m) = metadata {
            q = q.bind(m.to_string());
        }
        if let Some(p) = permission {
            q = q.bind(p.to_string());
        }
        q = q.bind(now_ms()).bind(id);
        Ok(q.execute(&self.pool).await?.rows_affected() > 0)
    }

    async fn set_revert(&self, id: &str, revert: Option<&Value>) -> Result<bool, DbError> {
        let revert_json = revert.map(|v| v.to_string());
        let affected = sqlx::query("UPDATE session SET revert = ?, time_updated = ? WHERE id = ?")
            .bind(revert_json)
            .bind(now_ms())
            .bind(id)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(affected > 0)
    }
}

/// In-memory [`SessionStore`] — test double / the backing for `AppContext::in_memory()`.
#[derive(Default)]
pub struct MemorySessionStore {
    rows: Mutex<HashMap<String, SessionRecord>>,
    /// Revert pointers set via [`SessionStore::set_revert`] (the V2 `SessionRecord` doesn't hold one),
    /// so the double can round-trip `revert`/`unrevert` through `get_full`.
    reverts: Mutex<HashMap<String, Value>>,
    /// Full V1 rows written via [`SessionStore::create`], so the double round-trips every `Session`
    /// column through `get_full` (the V2 `SessionRecord` in `rows` drops slug/version/metadata/…).
    full_rows: Mutex<HashMap<String, SessionV1Record>>,
}

/// Project a full V1 record down to the V2 [`SessionRecord`] kept in the memory double's `rows`
/// (so `get`/`list` see a created session).
fn v2_from_v1_record(r: &SessionV1Record) -> SessionRecord {
    SessionRecord {
        id: r.id.clone(),
        project_id: r.project_id.clone(),
        parent_id: r.parent_id.clone(),
        agent: r.agent.clone(),
        model: r.model.clone(),
        cost: r.cost,
        tokens_input: r.tokens.0,
        tokens_output: r.tokens.1,
        tokens_reasoning: r.tokens.2,
        tokens_cache_read: r.tokens.3,
        tokens_cache_write: r.tokens.4,
        title: r.title.clone(),
        directory: r.directory.clone(),
        workspace_id: r.workspace_id.clone(),
        path: r.path.clone(),
        time_created: r.time_created,
        time_updated: r.time_updated,
        time_archived: r.time_archived,
    }
}

impl MemorySessionStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert (or replace) a session record.
    pub fn insert(&self, record: SessionRecord) {
        self.rows
            .lock()
            .expect("session store mutex poisoned")
            .insert(record.id.clone(), record);
    }
}

#[async_trait]
impl SessionStore for MemorySessionStore {
    async fn get(&self, id: &str) -> Result<Option<SessionRecord>, DbError> {
        Ok(self
            .rows
            .lock()
            .expect("session store mutex poisoned")
            .get(id)
            .cloned())
    }

    async fn list(&self, query: &SessionListQuery) -> Result<Vec<SessionRecord>, DbError> {
        let descending = query.effective_descending();
        let search = query.search.as_ref().map(|s| s.to_lowercase());
        let mut rows: Vec<SessionRecord> = self
            .rows
            .lock()
            .expect("session store mutex poisoned")
            .values()
            .filter(|r| query.directory.as_ref().is_none_or(|d| &r.directory == d))
            .filter(|r| query.project.as_ref().is_none_or(|p| &r.project_id == p))
            .filter(|r| {
                query
                    .workspace
                    .as_ref()
                    .is_none_or(|w| r.workspace_id.as_deref() == Some(w.as_str()))
            })
            .filter(|r| {
                search
                    .as_ref()
                    .is_none_or(|s| r.title.to_lowercase().contains(s))
            })
            .cloned()
            .collect();
        // Keyset boundary relative to the anchor, in the effective order.
        if let Some(a) = &query.anchor {
            rows.retain(|r| {
                if descending {
                    (r.time_created, r.id.as_str()) < (a.time, a.id.as_str())
                } else {
                    (r.time_created, r.id.as_str()) > (a.time, a.id.as_str())
                }
            });
        }
        rows.sort_by(|a, b| (a.time_created, &a.id).cmp(&(b.time_created, &b.id)));
        if descending {
            rows.reverse();
        }
        if let Some(limit) = query.limit {
            rows.truncate(limit.max(0) as usize);
        }
        if query.is_previous() {
            rows.reverse();
        }
        Ok(rows)
    }

    async fn create(&self, record: &SessionV1Record) -> Result<(), DbError> {
        self.full_rows
            .lock()
            .expect("session store mutex poisoned")
            .insert(record.id.clone(), record.clone());
        self.rows
            .lock()
            .expect("session store mutex poisoned")
            .insert(record.id.clone(), v2_from_v1_record(record));
        Ok(())
    }

    async fn get_full(&self, id: &str) -> Result<Option<SessionV1Record>, DbError> {
        // A `create`d session round-trips fully; otherwise reconstruct from the V2 row + revert map.
        if let Some(record) = self
            .full_rows
            .lock()
            .expect("session store mutex poisoned")
            .get(id)
            .cloned()
        {
            return Ok(Some(record));
        }
        let Some(mut record) = self
            .rows
            .lock()
            .expect("session store mutex poisoned")
            .get(id)
            .map(v1_from_session_record)
        else {
            return Ok(None);
        };
        record.revert = self
            .reverts
            .lock()
            .expect("session store mutex poisoned")
            .get(id)
            .cloned();
        Ok(Some(record))
    }

    async fn update(
        &self,
        id: &str,
        title: Option<&str>,
        _metadata: Option<&Value>,
        _permission: Option<&Value>,
    ) -> Result<bool, DbError> {
        // The memory double stores only the V2 columns, so it applies `title` (what tests assert);
        // `metadata`/`permission` are persisted by the Sqlx store in production.
        let mut rows = self.rows.lock().expect("session store mutex poisoned");
        match rows.get_mut(id) {
            Some(record) => {
                if let Some(t) = title {
                    record.title = t.to_string();
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn set_revert(&self, id: &str, revert: Option<&Value>) -> Result<bool, DbError> {
        if !self
            .rows
            .lock()
            .expect("session store mutex poisoned")
            .contains_key(id)
        {
            return Ok(false);
        }
        let mut reverts = self.reverts.lock().expect("session store mutex poisoned");
        match revert {
            Some(v) => {
                reverts.insert(id.to_string(), v.clone());
            }
            None => {
                reverts.remove(id);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use serde_json::json;

    /// A `Database` with a minimal `session` parent table plus the two session-store tables created.
    async fn session_db() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("session.db"))
            .await
            .unwrap();
        sqlx::query("CREATE TABLE session (id text PRIMARY KEY)")
            .execute(db.pool())
            .await
            .unwrap();
        for ddl in [SESSION_INPUT_DDL, SESSION_CONTEXT_EPOCH_DDL] {
            for stmt in ddl.split(';').map(str::trim).filter(|s| !s.is_empty()) {
                sqlx::query(stmt).execute(db.pool()).await.unwrap();
            }
        }
        sqlx::query("INSERT INTO session (id) VALUES ('ses_1')")
            .execute(db.pool())
            .await
            .unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn session_input_roundtrips_and_filters_pending() {
        let (_dir, db) = session_db().await;
        let repo = db.session_input();

        let pending = SessionInput {
            id: "msg_a".into(),
            session_id: "ses_1".into(),
            prompt: json!({ "text": "hi" }),
            delivery: "steer".into(),
            admitted_seq: 1,
            promoted_seq: None,
            time_created: 100,
        };
        let promoted = SessionInput {
            id: "msg_b".into(),
            session_id: "ses_1".into(),
            prompt: json!({ "text": "bye" }),
            delivery: "steer".into(),
            admitted_seq: 2,
            promoted_seq: Some(5),
            time_created: 200,
        };
        repo.insert(&pending).await.unwrap();
        repo.insert(&promoted).await.unwrap();

        assert_eq!(repo.get("msg_a").await.unwrap().as_ref(), Some(&pending));
        assert_eq!(repo.get("missing").await.unwrap(), None);

        // Only the pending (promoted_seq IS NULL) input is listed.
        let pendings = repo.list_pending("ses_1").await.unwrap();
        assert_eq!(pendings, vec![pending]);
    }

    #[tokio::test]
    async fn session_input_admitted_seq_is_unique_per_session() {
        let (_dir, db) = session_db().await;
        let repo = db.session_input();
        let row = |id: &str| SessionInput {
            id: id.into(),
            session_id: "ses_1".into(),
            prompt: json!({}),
            delivery: "steer".into(),
            admitted_seq: 1,
            promoted_seq: None,
            time_created: 1,
        };
        repo.insert(&row("msg_a")).await.unwrap();
        // Same (session_id, admitted_seq) violates the unique index.
        assert!(repo.insert(&row("msg_b")).await.is_err());
    }

    #[tokio::test]
    async fn context_epoch_upserts() {
        let (_dir, db) = session_db().await;
        let repo = db.session_context_epoch();

        let mut epoch = SessionContextEpoch {
            session_id: "ses_1".into(),
            baseline: "base".into(),
            agent: "build".into(),
            snapshot: json!({ "files": [] }),
            baseline_seq: 0,
            replacement_seq: None,
            revision: 0,
        };
        repo.upsert(&epoch).await.unwrap();
        assert_eq!(repo.get("ses_1").await.unwrap().as_ref(), Some(&epoch));

        // Upsert replaces the row in place (PK = session_id).
        epoch.agent = "plan".into();
        epoch.revision = 1;
        epoch.replacement_seq = Some(7);
        repo.upsert(&epoch).await.unwrap();
        let got = repo.get("ses_1").await.unwrap().unwrap();
        assert_eq!(got, epoch);
        assert_eq!(got.agent, "plan");
        assert_eq!(got.revision, 1);
    }

    /// The `session` projection table as TS writes it (subset of columns the V2 read serves).
    const SESSION_PROJECTION_DDL: &str = "CREATE TABLE session (\
        id text PRIMARY KEY, project_id text NOT NULL, parent_id text, agent text, model text, \
        cost real NOT NULL DEFAULT 0, tokens_input integer NOT NULL DEFAULT 0, \
        tokens_output integer NOT NULL DEFAULT 0, tokens_reasoning integer NOT NULL DEFAULT 0, \
        tokens_cache_read integer NOT NULL DEFAULT 0, tokens_cache_write integer NOT NULL DEFAULT 0, \
        title text NOT NULL, directory text NOT NULL, workspace_id text, path text, \
        time_created integer NOT NULL, time_updated integer NOT NULL, time_archived integer)";

    #[tokio::test]
    async fn sqlx_session_store_reads_a_ts_written_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("sessions.db"))
            .await
            .unwrap();
        sqlx::query(SESSION_PROJECTION_DDL)
            .execute(db.pool())
            .await
            .unwrap();
        // A row shaped exactly as the TS server would persist it (model as JSON; workspace/path NULL).
        sqlx::query(
            "INSERT INTO session \
             (id, project_id, parent_id, agent, model, cost, tokens_input, tokens_output, \
              tokens_reasoning, tokens_cache_read, tokens_cache_write, title, directory, \
              workspace_id, path, time_created, time_updated, time_archived) \
             VALUES ('ses_1', 'prj_1', NULL, 'build', \
                     '{\"id\":\"claude\",\"providerID\":\"anthropic\",\"variant\":\"default\"}', \
                     1.5, 2, 3, 4, 5, 6, 'Hello', '/repo', NULL, NULL, 100, 200, NULL)",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let store = SqlxSessionStore::new(db.pool().clone());
        let got = store.get("ses_1").await.unwrap().unwrap();
        assert_eq!(got.id, "ses_1");
        assert_eq!(got.project_id, "prj_1");
        assert_eq!(got.parent_id, None);
        assert_eq!(got.agent.as_deref(), Some("build"));
        assert_eq!(got.model.as_ref().unwrap()["providerID"], "anthropic");
        assert_eq!(got.cost, 1.5);
        assert_eq!(got.tokens_input, 2);
        assert_eq!(got.tokens_cache_write, 6);
        assert_eq!(got.title, "Hello");
        assert_eq!(got.directory, "/repo");
        assert_eq!(got.workspace_id, None);
        assert_eq!(got.time_created, 100);
        assert_eq!(got.time_archived, None);

        assert!(store.get("ses_missing").await.unwrap().is_none());
    }

    /// Full session table (the V1 superset), for `get_full`/`update`.
    const SESSION_V1_DDL: &str = "CREATE TABLE session (\
        id text PRIMARY KEY, slug text NOT NULL, project_id text NOT NULL, workspace_id text, \
        directory text NOT NULL, path text, parent_id text, summary_additions integer, \
        summary_deletions integer, summary_files integer, summary_diffs text, \
        cost real NOT NULL DEFAULT 0, tokens_input integer NOT NULL DEFAULT 0, \
        tokens_output integer NOT NULL DEFAULT 0, tokens_reasoning integer NOT NULL DEFAULT 0, \
        tokens_cache_read integer NOT NULL DEFAULT 0, tokens_cache_write integer NOT NULL DEFAULT 0, \
        share_url text, title text NOT NULL, agent text, model text, version text NOT NULL, \
        metadata text, time_created integer NOT NULL, time_updated integer NOT NULL, \
        time_compacting integer, time_archived integer, permission text, revert text)";

    #[tokio::test]
    async fn sqlx_session_store_get_full_and_update() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("v1.db")).await.unwrap();
        sqlx::query(SESSION_V1_DDL)
            .execute(db.pool())
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO session (id, slug, project_id, directory, title, version, time_created, \
             time_updated, cost) VALUES ('ses_1','my-chat','prj_1','/repo','Hello','1.2.3',100,200,0)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let store = SqlxSessionStore::new(db.pool().clone());

        let full = store.get_full("ses_1").await.unwrap().unwrap();
        assert_eq!(full.slug, "my-chat");
        assert_eq!(full.version, "1.2.3");
        assert_eq!(full.title, "Hello");

        // Update the title (+ a permission ruleset JSON) and read it back.
        assert!(store
            .update(
                "ses_1",
                Some("Renamed"),
                None,
                Some(
                    &serde_json::json!([{ "permission": "bash", "pattern": "*", "action": "ask" }])
                ),
            )
            .await
            .unwrap());
        let updated = store.get_full("ses_1").await.unwrap().unwrap();
        assert_eq!(updated.title, "Renamed");
        assert!(updated.time_updated >= 200);
        assert_eq!(updated.permission.unwrap()[0]["action"], "ask");

        // A missing session: update reports false, get_full is None.
        assert!(!store.update("ses_x", Some("x"), None, None).await.unwrap());
        assert!(store.get_full("ses_x").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sqlx_session_store_create_inserts_full_row() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("create.db"))
            .await
            .unwrap();
        sqlx::query(SESSION_V1_DDL)
            .execute(db.pool())
            .await
            .unwrap();
        let store = SqlxSessionStore::new(db.pool().clone());
        let record = SessionV1Record {
            id: "ses_new".into(),
            slug: "fresh-chat".into(),
            project_id: "prj_1".into(),
            workspace_id: Some("wrk_1".into()),
            directory: "/repo".into(),
            path: None,
            parent_id: None,
            summary: None,
            summary_diffs: None,
            cost: 0.0,
            tokens: (0, 0, 0, 0, 0),
            share_url: None,
            title: "Fresh".into(),
            agent: Some("build".into()),
            model: Some(json!({ "id": "claude-x", "providerID": "anthropic" })),
            version: "9.9.9".into(),
            metadata: Some(json!({ "k": "v" })),
            time_created: 1000,
            time_updated: 1000,
            time_compacting: None,
            time_archived: None,
            permission: Some(json!([{ "permission": "bash", "pattern": "*", "action": "ask" }])),
            revert: None,
        };
        store.create(&record).await.unwrap();
        let back = store.get_full("ses_new").await.unwrap().unwrap();
        assert_eq!(back.slug, "fresh-chat");
        assert_eq!(back.title, "Fresh");
        assert_eq!(back.version, "9.9.9");
        assert_eq!(back.workspace_id.as_deref(), Some("wrk_1"));
        assert_eq!(back.agent.as_deref(), Some("build"));
        assert_eq!(back.model.unwrap()["providerID"], "anthropic");
        assert_eq!(back.metadata.unwrap()["k"], "v");
        assert_eq!(back.permission.unwrap()[0]["action"], "ask");
        // The V2 read path sees it too.
        assert!(store.get("ses_new").await.unwrap().is_some());
        // A duplicate id is an error.
        assert!(store.create(&record).await.is_err());
    }

    #[tokio::test]
    async fn memory_session_store_create_roundtrips() {
        let store = MemorySessionStore::new();
        let record = SessionV1Record {
            id: "ses_mem".into(),
            slug: "mem-chat".into(),
            project_id: "prj_1".into(),
            workspace_id: None,
            directory: "/repo".into(),
            path: None,
            parent_id: None,
            summary: None,
            summary_diffs: None,
            cost: 0.0,
            tokens: (0, 0, 0, 0, 0),
            share_url: None,
            title: "Mem".into(),
            agent: None,
            model: None,
            version: "1.0.0".into(),
            metadata: Some(json!({ "x": 1 })),
            time_created: 5,
            time_updated: 5,
            time_compacting: None,
            time_archived: None,
            permission: None,
            revert: None,
        };
        store.create(&record).await.unwrap();
        // get_full round-trips the V1-only fields (slug/version/metadata) the V2 row would drop.
        let back = store.get_full("ses_mem").await.unwrap().unwrap();
        assert_eq!(back.slug, "mem-chat");
        assert_eq!(back.version, "1.0.0");
        assert_eq!(back.metadata.unwrap()["x"], 1);
        // get/list see it via the projected V2 row.
        assert!(store.get("ses_mem").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn memory_session_store_update_title_and_get_full() {
        let store = MemorySessionStore::new();
        store.insert(SessionRecord {
            id: "ses_1".into(),
            project_id: "prj_1".into(),
            parent_id: None,
            agent: Some("build".into()),
            model: None,
            cost: 0.0,
            tokens_input: 0,
            tokens_output: 0,
            tokens_reasoning: 0,
            tokens_cache_read: 0,
            tokens_cache_write: 0,
            title: "Hello".into(),
            directory: "/repo".into(),
            workspace_id: None,
            path: None,
            time_created: 100,
            time_updated: 200,
            time_archived: None,
        });
        let full = store.get_full("ses_1").await.unwrap().unwrap();
        assert_eq!(full.title, "Hello");
        assert_eq!(full.slug, "ses_1"); // placeholder for the memory double
        assert!(store
            .update("ses_1", Some("Renamed"), None, None)
            .await
            .unwrap());
        assert_eq!(
            store.get_full("ses_1").await.unwrap().unwrap().title,
            "Renamed"
        );
        assert!(!store.update("ses_x", Some("x"), None, None).await.unwrap());
    }

    #[tokio::test]
    async fn memory_session_store_roundtrips() {
        let store = MemorySessionStore::new();
        assert!(store.get("ses_1").await.unwrap().is_none());
        let rec = SessionRecord {
            id: "ses_1".into(),
            project_id: "prj_1".into(),
            parent_id: None,
            agent: None,
            model: None,
            cost: 0.0,
            tokens_input: 0,
            tokens_output: 0,
            tokens_reasoning: 0,
            tokens_cache_read: 0,
            tokens_cache_write: 0,
            title: "t".into(),
            directory: "/d".into(),
            workspace_id: None,
            path: None,
            time_created: 1,
            time_updated: 2,
            time_archived: None,
        };
        store.insert(rec.clone());
        assert_eq!(store.get("ses_1").await.unwrap().as_ref(), Some(&rec));
    }

    #[tokio::test]
    async fn sqlx_session_store_lists_with_order_limit_search_and_filter() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("sessions.db"))
            .await
            .unwrap();
        sqlx::query(SESSION_PROJECTION_DDL)
            .execute(db.pool())
            .await
            .unwrap();
        for (id, project, title, created) in [
            ("ses_a", "prj_1", "Alpha", 100),
            ("ses_b", "prj_1", "Beta", 300),
            ("ses_c", "prj_2", "Gamma alpha", 200),
        ] {
            sqlx::query(
                "INSERT INTO session (id, project_id, title, directory, time_created, time_updated) \
                 VALUES (?, ?, ?, '/repo', ?, ?)",
            )
            .bind(id)
            .bind(project)
            .bind(title)
            .bind(created)
            .bind(created)
            .execute(db.pool())
            .await
            .unwrap();
        }
        let store = SqlxSessionStore::new(db.pool().clone());
        let ids = |rs: Vec<SessionRecord>| rs.into_iter().map(|r| r.id).collect::<Vec<_>>();

        // Default descending by time_created.
        let desc = SessionListQuery {
            descending: true,
            ..Default::default()
        };
        assert_eq!(
            ids(store.list(&desc).await.unwrap()),
            ["ses_b", "ses_c", "ses_a"]
        );

        // Ascending.
        let asc = SessionListQuery::default();
        assert_eq!(
            ids(store.list(&asc).await.unwrap()),
            ["ses_a", "ses_c", "ses_b"]
        );

        // Limit (applied after ordering).
        let limited = SessionListQuery {
            descending: true,
            limit: Some(2),
            ..Default::default()
        };
        assert_eq!(ids(store.list(&limited).await.unwrap()), ["ses_b", "ses_c"]);

        // Project filter.
        let by_project = SessionListQuery {
            project: Some("prj_1".into()),
            ..Default::default()
        };
        assert_eq!(
            ids(store.list(&by_project).await.unwrap()),
            ["ses_a", "ses_b"]
        );

        // Case-insensitive title search ("alpha" matches "Alpha" and "Gamma alpha").
        let search = SessionListQuery {
            search: Some("alpha".into()),
            ..Default::default()
        };
        assert_eq!(ids(store.list(&search).await.unwrap()), ["ses_a", "ses_c"]);
    }

    #[tokio::test]
    async fn memory_session_store_lists_with_order_and_filter() {
        let store = MemorySessionStore::new();
        let rec = |id: &str, project: &str, title: &str, created: i64| SessionRecord {
            id: id.into(),
            project_id: project.into(),
            parent_id: None,
            agent: None,
            model: None,
            cost: 0.0,
            tokens_input: 0,
            tokens_output: 0,
            tokens_reasoning: 0,
            tokens_cache_read: 0,
            tokens_cache_write: 0,
            title: title.into(),
            directory: "/d".into(),
            workspace_id: None,
            path: None,
            time_created: created,
            time_updated: created,
            time_archived: None,
        };
        store.insert(rec("ses_a", "prj_1", "Alpha", 100));
        store.insert(rec("ses_b", "prj_1", "Beta", 300));
        store.insert(rec("ses_c", "prj_2", "Gamma alpha", 200));
        let ids = |rs: Vec<SessionRecord>| rs.into_iter().map(|r| r.id).collect::<Vec<_>>();

        let desc = SessionListQuery {
            descending: true,
            ..Default::default()
        };
        assert_eq!(
            ids(store.list(&desc).await.unwrap()),
            ["ses_b", "ses_c", "ses_a"]
        );

        let by_project = SessionListQuery {
            project: Some("prj_1".into()),
            limit: Some(1),
            descending: true,
            ..Default::default()
        };
        assert_eq!(ids(store.list(&by_project).await.unwrap()), ["ses_b"]);

        let search = SessionListQuery {
            search: Some("ALPHA".into()),
            ..Default::default()
        };
        assert_eq!(ids(store.list(&search).await.unwrap()), ["ses_a", "ses_c"]);
    }

    #[tokio::test]
    async fn sqlx_session_store_keyset_pagination() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("sessions.db"))
            .await
            .unwrap();
        sqlx::query(SESSION_PROJECTION_DDL)
            .execute(db.pool())
            .await
            .unwrap();
        for i in 1..=5 {
            sqlx::query(
                "INSERT INTO session (id, project_id, title, directory, time_created, time_updated) \
                 VALUES (?, 'prj_1', 'S', '/r', ?, ?)",
            )
            .bind(format!("ses_{i}"))
            .bind(i * 100)
            .bind(i * 100)
            .execute(db.pool())
            .await
            .unwrap();
        }
        let store = SqlxSessionStore::new(db.pool().clone());
        let ids = |rs: &[SessionRecord]| rs.iter().map(|r| r.id.clone()).collect::<Vec<_>>();

        // Page 1: newest first, two per page.
        let page1 = store
            .list(&SessionListQuery {
                descending: true,
                limit: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(ids(&page1), ["ses_5", "ses_4"]);

        // Page 2: anchored "next" from the last row of page 1.
        let last = page1.last().unwrap();
        let page2 = store
            .list(&SessionListQuery {
                descending: true,
                limit: Some(2),
                anchor: Some(ListAnchor {
                    id: last.id.clone(),
                    time: last.time_created,
                    direction: ListDirection::Next,
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(ids(&page2), ["ses_3", "ses_2"]);

        // "previous" from the first row of page 2 returns page 1's rows in the requested (desc) order.
        let first2 = page2.first().unwrap();
        let prev = store
            .list(&SessionListQuery {
                descending: true,
                limit: Some(2),
                anchor: Some(ListAnchor {
                    id: first2.id.clone(),
                    time: first2.time_created,
                    direction: ListDirection::Previous,
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(ids(&prev), ["ses_5", "ses_4"]);
    }

    #[tokio::test]
    async fn memory_session_store_keyset_matches_sqlx() {
        let store = MemorySessionStore::new();
        for i in 1..=5 {
            let mut r = SessionRecord {
                id: format!("ses_{i}"),
                project_id: "prj_1".into(),
                parent_id: None,
                agent: None,
                model: None,
                cost: 0.0,
                tokens_input: 0,
                tokens_output: 0,
                tokens_reasoning: 0,
                tokens_cache_read: 0,
                tokens_cache_write: 0,
                title: "S".into(),
                directory: "/r".into(),
                workspace_id: None,
                path: None,
                time_created: 0,
                time_updated: 0,
                time_archived: None,
            };
            r.time_created = i * 100;
            store.insert(r);
        }
        let ids = |rs: &[SessionRecord]| rs.iter().map(|r| r.id.clone()).collect::<Vec<_>>();
        let page1 = store
            .list(&SessionListQuery {
                descending: true,
                limit: Some(2),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(ids(&page1), ["ses_5", "ses_4"]);
        let last = page1.last().unwrap();
        let page2 = store
            .list(&SessionListQuery {
                descending: true,
                limit: Some(2),
                anchor: Some(ListAnchor {
                    id: last.id.clone(),
                    time: last.time_created,
                    direction: ListDirection::Next,
                }),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(ids(&page2), ["ses_3", "ses_2"]);
    }
}
