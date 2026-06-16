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

/// Read-only store over the `session` projection table.
#[async_trait]
pub trait SessionStore: Send + Sync {
    /// Fetch a session by id, or `None` if it doesn't exist.
    async fn get(&self, id: &str) -> Result<Option<SessionRecord>, DbError>;

    /// List sessions matching `query`, ordered by `time_created` then `id` per `query.descending`.
    async fn list(&self, query: &SessionListQuery) -> Result<Vec<SessionRecord>, DbError>;
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
}

/// In-memory [`SessionStore`] — test double / the backing for `AppContext::in_memory()`.
#[derive(Default)]
pub struct MemorySessionStore {
    rows: Mutex<HashMap<String, SessionRecord>>,
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
