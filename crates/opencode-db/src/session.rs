//! Session-store repositories for the two event-sourced tables the session runner depends on:
//! `session_input` (the event-sourced steering inbox) and `session_context_epoch` (per-session
//! context baseline that detects mid-run agent/model changes).
//!
//! The DDL mirrors the final TypeScript schema (`packages/core/src/session/sql.ts` plus migrations
//! `…_event_sourced_session_input` and `…_add_context_epoch_agent`). TS owns schema creation during
//! the migration, so these tables are **not** auto-created by [`crate::Database::connect`]; the
//! [`SESSION_INPUT_DDL`]/[`SESSION_CONTEXT_EPOCH_DDL`] constants exist for tests and for a future
//! Rust-applies path. The repositories assume the tables already exist (created by TS).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;

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
}
