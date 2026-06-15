//! Migration-journal **verification** (read-only).
//!
//! During the TS↔Rust coexistence the TypeScript server owns schema evolution ("TS migrates, Rust
//! verifies"): it applies migrations and records them in the `migration(id, time_completed)` journal
//! (`packages/core/src/database/migration.ts`). Rust never applies migrations; instead it reads the
//! journal on boot and compares it against [`EXPECTED_MIGRATIONS`] — the ordered list of migration
//! ids this binary was compiled against (mirrors `database/migration.gen.ts`).
//!
//! The journal read mirrors the TS `applyOnly` bridge: if the `migration` table is empty/absent but a
//! legacy Drizzle journal (`__drizzle_migrations`) exists, its migration names count as applied — but
//! **read-only**, we never write the bridge rows (that remains TS's job).

use std::collections::BTreeSet;

use sqlx::SqlitePool;

use crate::DbError;

/// The ordered migration ids this build expects to be applied. Mirrors the `migrations` array in
/// `packages/core/src/database/migration.gen.ts`; keep in sync when TS adds a migration. The list is
/// chronological (ids are timestamp-prefixed), which is also lexicographic order.
pub const EXPECTED_MIGRATIONS: &[&str] = &[
    "20260127222353_familiar_lady_ursula",
    "20260211171708_add_project_commands",
    "20260213144116_wakeful_the_professor",
    "20260225215848_workspace",
    "20260227213759_add_session_workspace_id",
    "20260228203230_blue_harpoon",
    "20260303231226_add_workspace_fields",
    "20260309230000_move_org_to_state",
    "20260312043431_session_message_cursor",
    "20260323234822_events",
    "20260410174513_workspace-name",
    "20260413175956_chief_energizer",
    "20260423070820_add_icon_url_override",
    "20260427172553_slow_nightmare",
    "20260428004200_add_session_path",
    "20260501142318_next_venus",
    "20260504145000_add_sync_owner",
    "20260507164347_add_workspace_time",
    "20260510033149_session_usage",
    "20260511000411_data_migration_state",
    "20260511173437_session-metadata",
    "20260601010001_normalize_storage_paths",
    "20260601202201_amazing_prowler",
    "20260602002951_lowly_union_jack",
    "20260602182828_add_project_directories",
    "20260603001617_session_message_projection_indexes",
    "20260603040000_session_message_projection_order",
    "20260603141458_session_input_inbox",
    "20260603160727_jittery_ezekiel_stane",
    "20260604172448_event_sourced_session_input",
    "20260605003541_add_session_context_snapshot",
    "20260605042240_add_context_epoch_agent",
    "20260611035744_credential",
    "20260611192811_lush_chimera",
    "20260612174303_project_dir_strategy",
];

/// The outcome of verifying the migration journal against [`EXPECTED_MIGRATIONS`].
#[derive(Debug, Clone)]
pub struct MigrationReport {
    /// Migration ids the database reports as applied (after the read-only Drizzle bridge).
    pub applied: BTreeSet<String>,
    /// Expected migrations missing from the database — the database is **behind** this build.
    pub missing: Vec<String>,
    /// Applied migrations this build does not know about — the database is **ahead** of this build.
    pub unknown: Vec<String>,
    /// Whether the database carries a migration journal at all (a `migration` or
    /// `__drizzle_migrations` table). `false` means an uninitialized / fresh database.
    pub journal_present: bool,
}

impl MigrationReport {
    /// The database is missing migrations this build requires (it is older than this build).
    pub fn is_behind(&self) -> bool {
        !self.missing.is_empty()
    }

    /// The database has migrations newer than this build understands.
    pub fn is_ahead(&self) -> bool {
        !self.unknown.is_empty()
    }

    /// The journal exactly matches the migrations this build expects.
    pub fn in_sync(&self) -> bool {
        self.missing.is_empty() && self.unknown.is_empty()
    }
}

/// Whether a table exists in the SQLite catalog.
async fn table_exists(pool: &SqlitePool, name: &str) -> Result<bool, DbError> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    Ok(row.is_some())
}

/// Read the migration journal (read-only) and diff it against [`EXPECTED_MIGRATIONS`].
pub async fn verify(pool: &SqlitePool) -> Result<MigrationReport, DbError> {
    let mut applied: BTreeSet<String> = BTreeSet::new();

    let has_migration_table = table_exists(pool, "migration").await?;
    if has_migration_table {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT id FROM migration")
            .fetch_all(pool)
            .await?;
        applied.extend(rows.into_iter().map(|(id,)| id));
    }

    // Mirror the TS bridge: only consult the legacy Drizzle journal when our journal is empty, and
    // do it **read-only** (TS, not Rust, persists the bridge rows).
    let has_drizzle = if applied.is_empty() {
        table_exists(pool, "__drizzle_migrations").await?
    } else {
        false
    };
    if has_drizzle {
        let rows: Vec<(Option<String>,)> =
            sqlx::query_as("SELECT name FROM __drizzle_migrations WHERE name IS NOT NULL")
                .fetch_all(pool)
                .await?;
        applied.extend(rows.into_iter().filter_map(|(name,)| name));
    }

    let expected: BTreeSet<&str> = EXPECTED_MIGRATIONS.iter().copied().collect();
    let missing = EXPECTED_MIGRATIONS
        .iter()
        .filter(|id| !applied.contains(**id))
        .map(|id| id.to_string())
        .collect();
    let unknown = applied
        .iter()
        .filter(|id| !expected.contains(id.as_str()))
        .cloned()
        .collect();

    Ok(MigrationReport {
        applied,
        missing,
        unknown,
        journal_present: has_migration_table || has_drizzle,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    #[test]
    fn expected_migrations_are_unique_and_chronological() {
        let set: BTreeSet<&&str> = EXPECTED_MIGRATIONS.iter().collect();
        assert_eq!(
            set.len(),
            EXPECTED_MIGRATIONS.len(),
            "duplicate migration id"
        );
        let mut sorted = EXPECTED_MIGRATIONS.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            sorted, EXPECTED_MIGRATIONS,
            "migration ids must be listed in chronological (sorted) order"
        );
    }

    /// A `Database` over a fresh temp file. Returns the `TempDir` guard too — the caller must keep it
    /// bound so the SQLite file (and its `-wal`/`-shm`) outlive the pool.
    async fn temp_db() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::connect(dir.path().join("verify.db"))
            .await
            .unwrap();
        (dir, db)
    }

    #[tokio::test]
    async fn fresh_database_has_no_journal_and_is_behind() {
        let (_dir, db) = temp_db().await;
        let report = db.verify_migrations().await.unwrap();
        assert!(!report.journal_present, "fresh DB has no journal");
        assert!(report.is_behind(), "every expected migration is missing");
        assert_eq!(report.missing.len(), EXPECTED_MIGRATIONS.len());
        assert!(!report.is_ahead());
    }

    #[tokio::test]
    async fn fully_migrated_database_is_in_sync() {
        let (_dir, db) = temp_db().await;
        sqlx::query(
            "CREATE TABLE migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        for id in EXPECTED_MIGRATIONS {
            sqlx::query("INSERT INTO migration (id, time_completed) VALUES (?, 0)")
                .bind(id)
                .execute(db.pool())
                .await
                .unwrap();
        }
        let report = db.verify_migrations().await.unwrap();
        assert!(report.journal_present);
        assert!(
            report.in_sync(),
            "missing={:?} unknown={:?}",
            report.missing,
            report.unknown
        );
    }

    #[tokio::test]
    async fn database_ahead_reports_unknown_not_missing() {
        let (_dir, db) = temp_db().await;
        sqlx::query(
            "CREATE TABLE migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        for id in EXPECTED_MIGRATIONS {
            sqlx::query("INSERT INTO migration (id, time_completed) VALUES (?, 0)")
                .bind(id)
                .execute(db.pool())
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO migration (id, time_completed) VALUES ('29990101000000_from_the_future', 0)")
            .execute(db.pool())
            .await
            .unwrap();
        let report = db.verify_migrations().await.unwrap();
        assert!(!report.is_behind());
        assert!(report.is_ahead());
        assert_eq!(
            report.unknown,
            vec!["29990101000000_from_the_future".to_string()]
        );
    }

    #[tokio::test]
    async fn database_behind_reports_missing() {
        let (_dir, db) = temp_db().await;
        sqlx::query(
            "CREATE TABLE migration (id TEXT PRIMARY KEY, time_completed INTEGER NOT NULL)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        // Apply all but the last expected migration.
        for id in &EXPECTED_MIGRATIONS[..EXPECTED_MIGRATIONS.len() - 1] {
            sqlx::query("INSERT INTO migration (id, time_completed) VALUES (?, 0)")
                .bind(id)
                .execute(db.pool())
                .await
                .unwrap();
        }
        let report = db.verify_migrations().await.unwrap();
        assert!(report.journal_present);
        assert!(report.is_behind());
        assert_eq!(
            report.missing,
            vec![EXPECTED_MIGRATIONS[EXPECTED_MIGRATIONS.len() - 1].to_string()]
        );
    }

    #[tokio::test]
    async fn drizzle_journal_is_bridged_read_only() {
        let (_dir, db) = temp_db().await;
        sqlx::query(
            "CREATE TABLE __drizzle_migrations (id INTEGER PRIMARY KEY, hash text NOT NULL, created_at numeric, name text, applied_at TEXT)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        sqlx::query("INSERT INTO __drizzle_migrations (hash, name) VALUES ('h', ?)")
            .bind(EXPECTED_MIGRATIONS[0])
            .execute(db.pool())
            .await
            .unwrap();

        let report = db.verify_migrations().await.unwrap();
        assert!(report.journal_present, "drizzle table counts as a journal");
        assert!(report.applied.contains(EXPECTED_MIGRATIONS[0]));
        // Bridge is read-only: we must not have created/populated a `migration` table.
        assert!(
            !table_exists(db.pool(), "migration").await.unwrap(),
            "verify must not write the `migration` journal"
        );
    }
}
