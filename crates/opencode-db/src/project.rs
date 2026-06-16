//! Read store over the `project` **projection table** — the second entity (after sessions) to prove
//! the `AppContext → Store → projection table` pattern generalizes. Mirrors the TS `project` schema
//! (`packages/core/src/project/sql.ts`): the `icon_*` columns fold into one `icon` object and the
//! `sandboxes`/`commands` columns are JSON. TS owns the schema; reads here are a plain `SELECT`.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::DbError;

/// `project` projection-table DDL (the columns this store reads). For tests / a future Rust-applies
/// path; TS owns the real (possibly wider) table.
pub const PROJECT_DDL: &str = "\
CREATE TABLE IF NOT EXISTS project (\
  id text PRIMARY KEY,\
  worktree text NOT NULL,\
  vcs text,\
  name text,\
  icon_url text,\
  icon_url_override text,\
  icon_color text,\
  time_created integer NOT NULL,\
  time_updated integer NOT NULL,\
  time_initialized integer,\
  sandboxes text NOT NULL DEFAULT '[]',\
  commands text\
);";

/// A row of the `project` projection table.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectRecord {
    /// Project id.
    pub id: String,
    /// Absolute worktree path.
    pub worktree: String,
    /// Version-control system (e.g. `"git"`), if known.
    pub vcs: Option<String>,
    /// Display name, if set.
    pub name: Option<String>,
    /// Icon URL, if set.
    pub icon_url: Option<String>,
    /// Icon URL override, if set.
    pub icon_url_override: Option<String>,
    /// Icon color, if set.
    pub icon_color: Option<String>,
    /// Creation time (ms).
    pub time_created: i64,
    /// Last-updated time (ms).
    pub time_updated: i64,
    /// Initialization time (ms), if initialized.
    pub time_initialized: Option<i64>,
    /// Sandbox worktree paths (JSON array column).
    pub sandboxes: Vec<String>,
    /// Project commands JSON (`{ start? }`), if set.
    pub commands: Option<Value>,
}

const PROJECT_COLS: &str = "id, worktree, vcs, name, icon_url, icon_url_override, icon_color, \
     time_created, time_updated, time_initialized, sandboxes, commands";

fn record_from_row(row: &SqliteRow) -> Result<ProjectRecord, DbError> {
    let sandboxes: String = row.try_get("sandboxes")?;
    let sandboxes: Vec<String> = serde_json::from_str(&sandboxes)?;
    let commands: Option<String> = row.try_get("commands")?;
    let commands = commands.map(|s| serde_json::from_str(&s)).transpose()?;
    Ok(ProjectRecord {
        id: row.try_get("id")?,
        worktree: row.try_get("worktree")?,
        vcs: row.try_get("vcs")?,
        name: row.try_get("name")?,
        icon_url: row.try_get("icon_url")?,
        icon_url_override: row.try_get("icon_url_override")?,
        icon_color: row.try_get("icon_color")?,
        time_created: row.try_get("time_created")?,
        time_updated: row.try_get("time_updated")?,
        time_initialized: row.try_get("time_initialized")?,
        sandboxes,
        commands,
    })
}

/// Read-only store over the `project` projection table.
#[async_trait]
pub trait ProjectStore: Send + Sync {
    /// List all projects, most-recently-created first.
    async fn list(&self) -> Result<Vec<ProjectRecord>, DbError>;

    /// Fetch the project whose `worktree` equals `worktree` (a non-PK lookup), or `None`.
    async fn get_by_worktree(&self, worktree: &str) -> Result<Option<ProjectRecord>, DbError>;
}

/// SQLite-backed [`ProjectStore`] over the shared pool.
pub struct SqlxProjectStore {
    pool: SqlitePool,
}

impl SqlxProjectStore {
    /// Wrap the shared pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ProjectStore for SqlxProjectStore {
    async fn list(&self) -> Result<Vec<ProjectRecord>, DbError> {
        let rows = sqlx::query(&format!(
            "SELECT {PROJECT_COLS} FROM project ORDER BY time_created DESC, id"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(record_from_row).collect()
    }

    async fn get_by_worktree(&self, worktree: &str) -> Result<Option<ProjectRecord>, DbError> {
        let row = sqlx::query(&format!(
            "SELECT {PROJECT_COLS} FROM project WHERE worktree = ? LIMIT 1"
        ))
        .bind(worktree)
        .fetch_optional(&self.pool)
        .await?;
        row.as_ref().map(record_from_row).transpose()
    }
}

/// In-memory [`ProjectStore`] — test double / the backing for `AppServices::default()`.
#[derive(Default)]
pub struct MemoryProjectStore {
    rows: Mutex<Vec<ProjectRecord>>,
}

impl MemoryProjectStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a project record.
    pub fn insert(&self, record: ProjectRecord) {
        self.rows
            .lock()
            .expect("project store mutex poisoned")
            .push(record);
    }
}

#[async_trait]
impl ProjectStore for MemoryProjectStore {
    async fn list(&self) -> Result<Vec<ProjectRecord>, DbError> {
        let mut rows = self
            .rows
            .lock()
            .expect("project store mutex poisoned")
            .clone();
        // Most-recent first, id as a stable tiebreaker (matches the SQL ordering).
        rows.sort_by(|a, b| (b.time_created, b.id.as_str()).cmp(&(a.time_created, a.id.as_str())));
        Ok(rows)
    }

    async fn get_by_worktree(&self, worktree: &str) -> Result<Option<ProjectRecord>, DbError> {
        Ok(self
            .rows
            .lock()
            .expect("project store mutex poisoned")
            .iter()
            .find(|r| r.worktree == worktree)
            .cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;
    use serde_json::json;

    #[tokio::test]
    async fn sqlx_project_store_reads_ts_written_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("projects.db"))
            .await
            .unwrap();
        sqlx::query(PROJECT_DDL).execute(db.pool()).await.unwrap();
        // Two projects (newest by time_created should sort first).
        sqlx::query(
            "INSERT INTO project (id, worktree, vcs, name, icon_url, time_created, time_updated, \
             time_initialized, sandboxes, commands) \
             VALUES ('prj_a', '/a', 'git', 'Alpha', 'http://i', 100, 100, 150, \
                     '[\"/a/sb\"]', '{\"start\":\"bun dev\"}')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('prj_b', '/b', 300, 300, '[]')",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let store = SqlxProjectStore::new(db.pool().clone());
        let projects = store.list().await.unwrap();
        assert_eq!(projects.len(), 2);
        // Newest first.
        assert_eq!(projects[0].id, "prj_b");
        assert_eq!(projects[1].id, "prj_a");
        let a = &projects[1];
        assert_eq!(a.worktree, "/a");
        assert_eq!(a.vcs.as_deref(), Some("git"));
        assert_eq!(a.icon_url.as_deref(), Some("http://i"));
        assert_eq!(a.time_initialized, Some(150));
        assert_eq!(a.sandboxes, vec!["/a/sb".to_string()]);
        assert_eq!(a.commands, Some(json!({ "start": "bun dev" })));
        // prj_b: minimal row.
        assert_eq!(projects[0].sandboxes, Vec::<String>::new());
        assert_eq!(projects[0].commands, None);
        assert_eq!(projects[0].vcs, None);
    }

    #[tokio::test]
    async fn memory_project_store_lists_newest_first() {
        let store = MemoryProjectStore::new();
        let rec = |id: &str, created: i64| ProjectRecord {
            id: id.into(),
            worktree: "/w".into(),
            vcs: None,
            name: None,
            icon_url: None,
            icon_url_override: None,
            icon_color: None,
            time_created: created,
            time_updated: created,
            time_initialized: None,
            sandboxes: vec![],
            commands: None,
        };
        store.insert(rec("prj_old", 100));
        store.insert(rec("prj_new", 200));
        let ids: Vec<String> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, ["prj_new", "prj_old"]);
    }

    #[tokio::test]
    async fn sqlx_project_store_get_by_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("projects.db"))
            .await
            .unwrap();
        sqlx::query(PROJECT_DDL).execute(db.pool()).await.unwrap();
        sqlx::query(
            "INSERT INTO project (id, worktree, time_created, time_updated, sandboxes) \
             VALUES ('prj_a', '/repo/a', 1, 1, '[]')",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let store = SqlxProjectStore::new(db.pool().clone());
        assert_eq!(
            store.get_by_worktree("/repo/a").await.unwrap().unwrap().id,
            "prj_a"
        );
        assert!(store.get_by_worktree("/nope").await.unwrap().is_none());
    }
}
