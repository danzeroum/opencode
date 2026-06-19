//! Read store over the `credential` table — stored provider credentials (`packages/core/src/credential`).
//! TS owns the schema + writes (the auth flows); reads here are a plain `SELECT`. Rows with no
//! `integration_id` are skipped (they can't enable a provider), mirroring the TS `Credential.all()`.

use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

use crate::DbError;

/// `credential` table DDL (the columns this store reads). For tests / a future Rust-applies path; TS
/// owns the real (wider) table.
pub const CREDENTIAL_DDL: &str = "\
CREATE TABLE IF NOT EXISTS credential (\
  id text PRIMARY KEY,\
  integration_id text,\
  label text NOT NULL,\
  value text NOT NULL,\
  connector_id text,\
  method_id text,\
  active integer,\
  time_created integer NOT NULL,\
  time_updated integer NOT NULL\
);";

/// A row of the `credential` table (only those bound to an integration/provider).
#[derive(Debug, Clone, PartialEq)]
pub struct CredentialRecord {
    /// Credential id (`cred_…`).
    pub id: String,
    /// The integration/provider id this credential belongs to.
    pub integration_id: String,
    /// Human label (e.g. `"default"`).
    pub label: String,
    /// The secret value (`{ type: "key" | "oauth", … }`), kept as JSON — the catalog only needs its
    /// presence + id; the value is for the execution path.
    pub value: Value,
}

const CREDENTIAL_COLS: &str = "id, integration_id, label, value";

fn record_from_row(row: &SqliteRow) -> Result<CredentialRecord, DbError> {
    let value: String = row.try_get("value")?;
    let value: Value = serde_json::from_str(&value)?;
    Ok(CredentialRecord {
        id: row.try_get("id")?,
        integration_id: row.try_get("integration_id")?,
        label: row.try_get("label")?,
        value,
    })
}

/// Read-only store over the `credential` table.
#[async_trait]
pub trait CredentialStore: Send + Sync {
    /// Every stored credential bound to an integration, oldest-created first (matches the TS
    /// `Credential.all()`, which skips rows with no `integration_id`).
    async fn all(&self) -> Result<Vec<CredentialRecord>, DbError>;
}

/// SQLite-backed [`CredentialStore`] over the shared pool.
pub struct SqlxCredentialStore {
    pool: SqlitePool,
}

impl SqlxCredentialStore {
    /// Wrap the shared pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl CredentialStore for SqlxCredentialStore {
    async fn all(&self) -> Result<Vec<CredentialRecord>, DbError> {
        let rows = sqlx::query(&format!(
            "SELECT {CREDENTIAL_COLS} FROM credential WHERE integration_id IS NOT NULL \
             ORDER BY time_created, id"
        ))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(record_from_row).collect()
    }
}

/// In-memory [`CredentialStore`] — test double / the backing for `AppServices::default()`.
#[derive(Default)]
pub struct MemoryCredentialStore {
    rows: Mutex<Vec<CredentialRecord>>,
}

impl MemoryCredentialStore {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert a credential record.
    pub fn insert(&self, record: CredentialRecord) {
        self.rows
            .lock()
            .expect("credential store mutex poisoned")
            .push(record);
    }
}

#[async_trait]
impl CredentialStore for MemoryCredentialStore {
    async fn all(&self) -> Result<Vec<CredentialRecord>, DbError> {
        Ok(self
            .rows
            .lock()
            .expect("credential store mutex poisoned")
            .clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    #[tokio::test]
    async fn sqlx_credential_store_reads_rows_and_skips_null_integration() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("creds.db"))
            .await
            .unwrap();
        sqlx::query(CREDENTIAL_DDL)
            .execute(db.pool())
            .await
            .unwrap();
        // A provider-bound key credential...
        sqlx::query(
            "INSERT INTO credential (id, integration_id, label, value, time_created, time_updated) \
             VALUES ('cred_a', 'anthropic', 'default', '{\"type\":\"key\",\"key\":\"sk-x\"}', 100, 100)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        // ...and a dangling credential with no integration (must be skipped).
        sqlx::query(
            "INSERT INTO credential (id, integration_id, label, value, time_created, time_updated) \
             VALUES ('cred_b', NULL, 'orphan', '{\"type\":\"key\",\"key\":\"sk-y\"}', 200, 200)",
        )
        .execute(db.pool())
        .await
        .unwrap();

        let store = SqlxCredentialStore::new(db.pool().clone());
        let creds = store.all().await.unwrap();
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].id, "cred_a");
        assert_eq!(creds[0].integration_id, "anthropic");
        assert_eq!(creds[0].label, "default");
        assert_eq!(creds[0].value["type"], "key");
        assert_eq!(creds[0].value["key"], "sk-x");
    }

    #[tokio::test]
    async fn memory_credential_store_round_trips() {
        let store = MemoryCredentialStore::new();
        store.insert(CredentialRecord {
            id: "cred_a".into(),
            integration_id: "anthropic".into(),
            label: "default".into(),
            value: serde_json::json!({ "type": "key", "key": "sk-x" }),
        });
        let creds = store.all().await.unwrap();
        assert_eq!(creds.len(), 1);
        assert_eq!(creds[0].integration_id, "anthropic");
    }
}
