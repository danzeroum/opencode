//! Read store over the `todo` table — a session's todo list (`packages/core/src/session/sql.ts`,
//! `TodoTable`). TS owns the schema + writes; reads here are a plain `SELECT` ordered by `position`.
//! Rows are returned with their `position` so the server can order; the wire `Todo` keeps only
//! `content`/`status`/`priority`.

use std::sync::Mutex;

use async_trait::async_trait;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::DbError;

/// `todo` table DDL (the columns this store reads). For tests / a future Rust-applies path; TS owns the
/// real table (composite PK `(session_id, position)`).
pub const TODO_DDL: &str = "\
CREATE TABLE IF NOT EXISTS todo (\
  session_id text NOT NULL,\
  content text NOT NULL,\
  status text NOT NULL,\
  priority text NOT NULL,\
  position integer NOT NULL,\
  time_created integer NOT NULL,\
  time_updated integer NOT NULL,\
  PRIMARY KEY (session_id, position)\
);";

/// A row of the `todo` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoRecord {
    /// Brief description of the task.
    pub content: String,
    /// Current status (`pending` | `in_progress` | `completed` | `cancelled`).
    pub status: String,
    /// Priority (`high` | `medium` | `low`).
    pub priority: String,
    /// Stable ordering position within the session.
    pub position: i64,
}

const TODO_COLS: &str = "content, status, priority, position";

fn record_from_row(row: &SqliteRow) -> Result<TodoRecord, DbError> {
    Ok(TodoRecord {
        content: row.try_get("content")?,
        status: row.try_get("status")?,
        priority: row.try_get("priority")?,
        position: row.try_get("position")?,
    })
}

/// Read-only store over the `todo` table.
#[async_trait]
pub trait TodoStore: Send + Sync {
    /// A session's todos, ordered by `position`.
    async fn list(&self, session_id: &str) -> Result<Vec<TodoRecord>, DbError>;
}

/// SQLite-backed [`TodoStore`] over the shared pool.
pub struct SqlxTodoStore {
    pool: SqlitePool,
}

impl SqlxTodoStore {
    /// Wrap the shared pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl TodoStore for SqlxTodoStore {
    async fn list(&self, session_id: &str) -> Result<Vec<TodoRecord>, DbError> {
        let rows = sqlx::query(&format!(
            "SELECT {TODO_COLS} FROM todo WHERE session_id = ? ORDER BY position"
        ))
        .bind(session_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(record_from_row).collect()
    }
}

/// In-memory [`TodoStore`] — test double / the backing for `AppServices::default()`.
#[derive(Default)]
pub struct MemoryTodoStore {
    rows: Mutex<Vec<(String, TodoRecord)>>,
}

impl MemoryTodoStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a todo for `session_id`.
    pub fn insert(&self, session_id: &str, record: TodoRecord) {
        self.rows
            .lock()
            .expect("todo store mutex poisoned")
            .push((session_id.to_string(), record));
    }
}

#[async_trait]
impl TodoStore for MemoryTodoStore {
    async fn list(&self, session_id: &str) -> Result<Vec<TodoRecord>, DbError> {
        let mut rows: Vec<TodoRecord> = self
            .rows
            .lock()
            .expect("todo store mutex poisoned")
            .iter()
            .filter(|(sid, _)| sid == session_id)
            .map(|(_, r)| r.clone())
            .collect();
        rows.sort_by_key(|r| r.position);
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    #[tokio::test]
    async fn sqlx_lists_todos_ordered_by_position_per_session() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("todo.db")).await.unwrap();
        sqlx::query(TODO_DDL).execute(db.pool()).await.unwrap();
        for (sid, content, status, prio, pos) in [
            ("ses_1", "second", "pending", "low", 1),
            ("ses_1", "first", "completed", "high", 0),
            ("ses_2", "other", "pending", "medium", 0),
        ] {
            sqlx::query(
                "INSERT INTO todo (session_id, content, status, priority, position, time_created, time_updated) \
                 VALUES (?, ?, ?, ?, ?, 1, 1)",
            )
            .bind(sid)
            .bind(content)
            .bind(status)
            .bind(prio)
            .bind(pos)
            .execute(db.pool())
            .await
            .unwrap();
        }
        let store = SqlxTodoStore::new(db.pool().clone());
        let todos = store.list("ses_1").await.unwrap();
        assert_eq!(
            todos.iter().map(|t| t.content.as_str()).collect::<Vec<_>>(),
            ["first", "second"]
        );
        assert_eq!(todos[0].status, "completed");
        assert_eq!(store.list("ses_2").await.unwrap().len(), 1);
        assert!(store.list("ses_none").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn memory_orders_by_position() {
        let store = MemoryTodoStore::new();
        store.insert(
            "ses_1",
            TodoRecord {
                content: "b".into(),
                status: "pending".into(),
                priority: "low".into(),
                position: 1,
            },
        );
        store.insert(
            "ses_1",
            TodoRecord {
                content: "a".into(),
                status: "pending".into(),
                priority: "high".into(),
                position: 0,
            },
        );
        let todos = store.list("ses_1").await.unwrap();
        assert_eq!(
            todos.iter().map(|t| t.content.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }
}
