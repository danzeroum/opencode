//! Read store over the `session_message` table — the V2 session **timeline** rows that back
//! `v2.session.messages`. Each row is a `SessionMessage` with its discriminator (`type`) and `id`
//! lifted into columns and the rest of the encoding kept as JSON `data`; the canonical order is `seq`.
//!
//! TS owns the schema + writes (the projector populates `session_message`); reads here are a plain
//! `SELECT` with the same seq-windowed pagination as `V2Session.messages`
//! (`packages/core/src/session.ts`). Reconstructing a typed `SessionMessage` from a row (`{ ...data,
//! id, type }`) is the server's job — this crate stays proto-free, returning raw rows.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::DbError;

/// `session_message` table DDL (the columns this store reads). For tests / a future Rust-applies path;
/// TS owns the real (wider, indexed) table.
pub const SESSION_MESSAGE_DDL: &str = "\
CREATE TABLE IF NOT EXISTS session_message (\
  id text PRIMARY KEY,\
  session_id text NOT NULL,\
  type text NOT NULL,\
  seq integer NOT NULL,\
  time_created integer NOT NULL,\
  time_updated integer NOT NULL,\
  data text NOT NULL\
);";

/// A row of the `session_message` timeline table.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionMessageRow {
    /// Message id (`msg_…`).
    pub id: String,
    /// Owning session id (`ses_…`).
    pub session_id: String,
    /// The timeline discriminator (the `SessionMessage` `type`, e.g. `"user"` / `"assistant"`).
    pub kind: String,
    /// Monotonic per-session sequence — the canonical timeline order.
    pub seq: i64,
    /// The `SessionMessage` encoding minus its `type` + `id` (those are columns); merge them back to
    /// reconstruct the wire object.
    pub data: Value,
}

/// Sort order over `seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageOrder {
    /// Oldest first.
    Asc,
    /// Newest first.
    Desc,
}

const SESSION_MESSAGE_COLS: &str = "id, session_id, type, seq, data";

fn record_from_row(row: &SqliteRow) -> Result<SessionMessageRow, DbError> {
    let data: String = row.try_get("data")?;
    let data: Value = serde_json::from_str(&data)?;
    Ok(SessionMessageRow {
        id: row.try_get("id")?,
        session_id: row.try_get("session_id")?,
        kind: row.try_get("type")?,
        seq: row.try_get("seq")?,
        data,
    })
}

/// Read-only store over the `session_message` timeline table.
#[async_trait]
pub trait SessionMessageStore: Send + Sync {
    /// The `seq` of a specific message in a session (the cursor *anchor* lookup), or `None` if no such
    /// row — mirrors the anchor `select` in `V2Session.messages`.
    async fn seq_of(&self, session_id: &str, id: &str) -> Result<Option<i64>, DbError>;

    /// Timeline rows for a session, optionally bounded by an exclusive `seq` window
    /// (`after_seq` → `seq > after_seq`; `before_seq` → `seq < before_seq`), ordered by `seq`, capped at
    /// `limit`. The seq-window + order mirror `V2Session.messages`' cursor paging.
    async fn list(
        &self,
        session_id: &str,
        after_seq: Option<i64>,
        before_seq: Option<i64>,
        order: MessageOrder,
        limit: Option<i64>,
    ) -> Result<Vec<SessionMessageRow>, DbError>;

    /// Append a timeline row, assigning the next per-session `seq` (max + 1). Returns the assigned seq.
    /// `kind` is the `SessionMessage` `type`; `data` is the encoding minus `id` + `type`. The native
    /// write path (the runner projecting a finished turn) uses this; TS still owns its own projector.
    async fn append(
        &self,
        session_id: &str,
        id: &str,
        kind: &str,
        data: Value,
    ) -> Result<i64, DbError>;
}

/// Wall-clock milliseconds since the epoch (the `time_created`/`time_updated` columns).
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// SQLite-backed [`SessionMessageStore`] over the shared pool.
pub struct SqlxSessionMessageStore {
    pool: SqlitePool,
}

impl SqlxSessionMessageStore {
    /// Wrap the shared pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl SessionMessageStore for SqlxSessionMessageStore {
    async fn seq_of(&self, session_id: &str, id: &str) -> Result<Option<i64>, DbError> {
        let row =
            sqlx::query("SELECT seq FROM session_message WHERE session_id = ? AND id = ? LIMIT 1")
                .bind(session_id)
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        row.as_ref()
            .map(|r| r.try_get("seq"))
            .transpose()
            .map_err(Into::into)
    }

    async fn list(
        &self,
        session_id: &str,
        after_seq: Option<i64>,
        before_seq: Option<i64>,
        order: MessageOrder,
        limit: Option<i64>,
    ) -> Result<Vec<SessionMessageRow>, DbError> {
        let mut sql =
            format!("SELECT {SESSION_MESSAGE_COLS} FROM session_message WHERE session_id = ?");
        if after_seq.is_some() {
            sql.push_str(" AND seq > ?");
        }
        if before_seq.is_some() {
            sql.push_str(" AND seq < ?");
        }
        sql.push_str(match order {
            MessageOrder::Asc => " ORDER BY seq ASC",
            MessageOrder::Desc => " ORDER BY seq DESC",
        });
        if limit.is_some() {
            sql.push_str(" LIMIT ?");
        }
        let mut query = sqlx::query(&sql).bind(session_id);
        if let Some(a) = after_seq {
            query = query.bind(a);
        }
        if let Some(b) = before_seq {
            query = query.bind(b);
        }
        if let Some(l) = limit {
            query = query.bind(l);
        }
        let rows = query.fetch_all(&self.pool).await?;
        rows.iter().map(record_from_row).collect()
    }

    async fn append(
        &self,
        session_id: &str,
        id: &str,
        kind: &str,
        data: Value,
    ) -> Result<i64, DbError> {
        // One drain task writes a session at a time, but use a transaction so the seq read + insert is
        // atomic against any concurrent writer (the unique `(session_id, seq)` index is the backstop).
        let mut tx = self.pool.begin().await?;
        let next: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM session_message WHERE session_id = ?",
        )
        .bind(session_id)
        .fetch_one(&mut *tx)
        .await?;
        let now = now_ms();
        sqlx::query(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(session_id)
        .bind(kind)
        .bind(next)
        .bind(now)
        .bind(now)
        .bind(data.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(next)
    }
}

/// In-memory [`SessionMessageStore`] — test double / the backing for `AppServices::default()`.
#[derive(Default)]
pub struct MemorySessionMessageStore {
    rows: Mutex<Vec<SessionMessageRow>>,
}

impl MemorySessionMessageStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a timeline row.
    pub fn insert(&self, row: SessionMessageRow) {
        self.rows
            .lock()
            .expect("session_message store mutex poisoned")
            .push(row);
    }
}

#[async_trait]
impl SessionMessageStore for MemorySessionMessageStore {
    async fn seq_of(&self, session_id: &str, id: &str) -> Result<Option<i64>, DbError> {
        Ok(self
            .rows
            .lock()
            .expect("session_message store mutex poisoned")
            .iter()
            .find(|r| r.session_id == session_id && r.id == id)
            .map(|r| r.seq))
    }

    async fn list(
        &self,
        session_id: &str,
        after_seq: Option<i64>,
        before_seq: Option<i64>,
        order: MessageOrder,
        limit: Option<i64>,
    ) -> Result<Vec<SessionMessageRow>, DbError> {
        let mut rows: Vec<SessionMessageRow> = self
            .rows
            .lock()
            .expect("session_message store mutex poisoned")
            .iter()
            .filter(|r| r.session_id == session_id)
            .filter(|r| after_seq.is_none_or(|a| r.seq > a))
            .filter(|r| before_seq.is_none_or(|b| r.seq < b))
            .cloned()
            .collect();
        match order {
            MessageOrder::Asc => rows.sort_by_key(|r| r.seq),
            MessageOrder::Desc => rows.sort_by(|a, b| b.seq.cmp(&a.seq)),
        }
        if let Some(l) = limit {
            rows.truncate(l.max(0) as usize);
        }
        Ok(rows)
    }

    async fn append(
        &self,
        session_id: &str,
        id: &str,
        kind: &str,
        data: Value,
    ) -> Result<i64, DbError> {
        let mut rows = self
            .rows
            .lock()
            .expect("session_message store mutex poisoned");
        let next = rows
            .iter()
            .filter(|r| r.session_id == session_id)
            .map(|r| r.seq)
            .max()
            .unwrap_or(0)
            + 1;
        rows.push(SessionMessageRow {
            id: id.to_string(),
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            seq: next,
            data,
        });
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use serde_json::json;

    async fn seeded_db() -> Database {
        let dir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        let db = Database::connect(dir.path().join("msgs.db")).await.unwrap();
        sqlx::query(SESSION_MESSAGE_DDL)
            .execute(db.pool())
            .await
            .unwrap();
        for (id, seq, kind) in [
            ("msg_a", 1, "user"),
            ("msg_b", 2, "assistant"),
            ("msg_c", 3, "user"),
        ] {
            sqlx::query(
                "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) \
                 VALUES (?, 'ses_1', ?, ?, ?, ?, ?)",
            )
            .bind(id)
            .bind(kind)
            .bind(seq)
            .bind(seq * 10)
            .bind(seq * 10)
            .bind(json!({ "time": { "created": seq * 10 }, "text": id }).to_string())
            .execute(db.pool())
            .await
            .unwrap();
        }
        // A row in another session (must never leak across sessions).
        sqlx::query(
            "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) \
             VALUES ('msg_other', 'ses_2', 'user', 1, 5, 5, '{}')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        db
    }

    #[tokio::test]
    async fn sqlx_lists_a_session_ordered_by_seq_and_isolates_sessions() {
        let db = seeded_db().await;
        let store = SqlxSessionMessageStore::new(db.pool().clone());
        let asc = store
            .list("ses_1", None, None, MessageOrder::Asc, None)
            .await
            .unwrap();
        assert_eq!(
            asc.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["msg_a", "msg_b", "msg_c"]
        );
        assert_eq!(asc[0].kind, "user");
        assert_eq!(asc[0].data["text"], "msg_a");
        // Desc + limit (the default newest-first page).
        let desc = store
            .list("ses_1", None, None, MessageOrder::Desc, Some(2))
            .await
            .unwrap();
        assert_eq!(
            desc.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["msg_c", "msg_b"]
        );
    }

    #[tokio::test]
    async fn sqlx_seq_window_pages_with_anchor() {
        let db = seeded_db().await;
        let store = SqlxSessionMessageStore::new(db.pool().clone());
        let anchor = store.seq_of("ses_1", "msg_b").await.unwrap().unwrap();
        assert_eq!(anchor, 2);
        // Newer than msg_b, ascending.
        let after = store
            .list("ses_1", Some(anchor), None, MessageOrder::Asc, None)
            .await
            .unwrap();
        assert_eq!(
            after.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["msg_c"]
        );
        // Older than msg_b, descending.
        let before = store
            .list("ses_1", None, Some(anchor), MessageOrder::Desc, None)
            .await
            .unwrap();
        assert_eq!(
            before.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["msg_a"]
        );
        // Unknown anchor → None.
        assert!(store.seq_of("ses_1", "msg_zzz").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn memory_matches_sqlx_ordering_and_window() {
        let store = MemorySessionMessageStore::new();
        for (id, seq) in [("msg_a", 1i64), ("msg_b", 2), ("msg_c", 3)] {
            store.insert(SessionMessageRow {
                id: id.into(),
                session_id: "ses_1".into(),
                kind: "user".into(),
                seq,
                data: json!({ "text": id }),
            });
        }
        let desc = store
            .list("ses_1", None, None, MessageOrder::Desc, Some(2))
            .await
            .unwrap();
        assert_eq!(
            desc.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["msg_c", "msg_b"]
        );
        let after = store
            .list("ses_1", Some(1), None, MessageOrder::Asc, None)
            .await
            .unwrap();
        assert_eq!(
            after.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["msg_b", "msg_c"]
        );
        assert_eq!(store.seq_of("ses_1", "msg_b").await.unwrap(), Some(2));
    }

    #[tokio::test]
    async fn sqlx_append_assigns_monotonic_seq_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("append.db"))
            .await
            .unwrap();
        sqlx::query(SESSION_MESSAGE_DDL)
            .execute(db.pool())
            .await
            .unwrap();
        let store = SqlxSessionMessageStore::new(db.pool().clone());
        let s1 = store
            .append("ses_1", "msg_1", "user", json!({ "text": "hi" }))
            .await
            .unwrap();
        let s2 = store
            .append("ses_1", "msg_2", "assistant", json!({ "agent": "build" }))
            .await
            .unwrap();
        assert_eq!((s1, s2), (1, 2));
        // A different session starts its own sequence at 1.
        assert_eq!(
            store
                .append("ses_2", "msg_3", "user", json!({}))
                .await
                .unwrap(),
            1
        );
        // The appended rows read back in order, reconstructable by the server.
        let rows = store
            .list("ses_1", None, None, MessageOrder::Asc, None)
            .await
            .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(),
            ["msg_1", "msg_2"]
        );
        assert_eq!(rows[0].kind, "user");
        assert_eq!(rows[0].data["text"], "hi");
    }

    #[tokio::test]
    async fn memory_append_matches_sqlx_seq_behavior() {
        let store = MemorySessionMessageStore::new();
        assert_eq!(
            store
                .append("ses_1", "m1", "user", json!({}))
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .append("ses_1", "m2", "assistant", json!({}))
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            store
                .append("ses_2", "m3", "user", json!({}))
                .await
                .unwrap(),
            1
        );
    }
}
