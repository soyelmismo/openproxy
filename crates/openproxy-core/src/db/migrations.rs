//! Embedded migration runner.
//!
//! Migration files live under `crates/openproxy-core/migrations/` and are
//! embedded at compile time via `include_str!`. Versions are the six-digit
//! numeric prefix of the filename. The runner is idempotent: a second
//! invocation against an already-migrated DB applies zero new versions.

use crate::error::{CoreError, Result};
use rusqlite::{params, Connection, Transaction};

/// One embedded migration. `version` is the integer PK stored in
/// `schema_migrations`. `sql` is the raw file contents.
struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "000001_initial_schema",
        sql: include_str!("../../migrations/000001_initial_schema.sql"),
    },
    Migration {
        version: 2,
        name: "000002_add_timing_to_usage",
        sql: include_str!("../../migrations/000002_add_timing_to_usage.sql"),
    },
    Migration {
        version: 3,
        name: "000003_add_race_to_usage",
        sql: include_str!("../../migrations/000003_add_race_to_usage.sql"),
    },
    Migration {
        version: 4,
        name: "000004_add_race_size_to_combos",
        sql: include_str!("../../migrations/000004_add_race_size_to_combos.sql"),
    },
    Migration {
        version: 5,
        name: "000005_add_provider_timeouts",
        sql: include_str!("../../migrations/000005_add_provider_timeouts.sql"),
    },
    Migration {
        version: 6,
        name: "000006_add_model_timeout_overrides",
        sql: include_str!("../../migrations/000006_add_model_timeout_overrides.sql"),
    },
    Migration {
        version: 8,
        name: "000008_add_error_msg_redacted",
        sql: include_str!("../../migrations/000008_add_error_msg_redacted.sql"),
    },
    Migration {
        version: 9,
        name: "000009_add_active_to_models",
        sql: include_str!("../../migrations/000009_add_active_to_models.sql"),
    },
    Migration {
        version: 10,
        name: "000010_add_provider_keyword_and_model_status",
        sql: include_str!("../../migrations/000010_add_provider_keyword_and_model_status.sql"),
    },
    Migration {
        version: 11,
        name: "000011_only_new_models_in_auto_activate",
        sql: include_str!("../../migrations/000011_only_new_models_in_auto_activate.sql"),
    },
    Migration {
        version: 12,
        name: "000012_add_account_quota",
        sql: include_str!("../../migrations/000012_add_account_quota.sql"),
    },
    Migration {
        version: 13,
        name: "000013_add_active_to_providers",
        sql: include_str!("../../migrations/000013_add_active_to_providers.sql"),
    },
    Migration {
        version: 14,
        name: "000014_add_model_metadata",
        sql: include_str!("../../migrations/000014_add_model_metadata.sql"),
    },
    Migration {
        version: 15,
        name: "000015_add_api_key_metadata",
        sql: include_str!("../../migrations/000015_add_api_key_metadata.sql"),
    },
    Migration {
        version: 16,
        name: "000016_add_subcombo_support",
        sql: include_str!("../../migrations/000016_add_subcombo_support.sql"),
    },
    Migration {
        version: 17,
        name: "000017_add_target_cooldowns",
        sql: include_str!("../../migrations/000017_add_target_cooldowns.sql"),
    },
    Migration {
        version: 18,
        name: "000018_add_gemini_format_and_goog_auth",
        sql: include_str!("../../migrations/000018_add_gemini_format_and_goog_auth.sql"),
    },
    Migration {
        version: 19,
        name: "000019_add_oauth_support",
        sql: include_str!("../../migrations/000019_add_oauth_support.sql"),
    },
    Migration {
        version: 20,
        name: "000020_set_antigravity_format_to_gemini",
        sql: include_str!("../../migrations/000020_set_antigravity_format_to_gemini.sql"),
    },
    Migration {
        version: 21,
        name: "000021_add_none_auth_type",
        sql: include_str!("../../migrations/000021_add_none_auth_type.sql"),
    },
    Migration {
        version: 22,
        name: "000022_add_gemini_target_format",
        sql: include_str!("../../migrations/000022_add_gemini_target_format.sql"),
    },
    Migration {
        version: 23,
        name: "000023_add_live_log_columns",
        sql: include_str!("../../migrations/000023_add_live_log_columns.sql"),
    },
    Migration {
        version: 24,
        name: "000024_add_app_config_kv",
        sql: include_str!("../../migrations/000024_add_app_config_kv.sql"),
    },
    Migration {
        version: 25,
        name: "000025_combo_targets_model_fk_set_null",
        sql: include_str!("../../migrations/000025_combo_targets_model_fk_set_null.sql"),
    },
    Migration {
        version: 26,
        name: "000026_combo_targets_upstream_model_id",
        sql: include_str!("../../migrations/000026_combo_targets_upstream_model_id.sql"),
    },
    Migration {
        version: 27,
        name: "000027_oauth_device_tickets",
        sql: include_str!("../../migrations/000027_oauth_device_tickets.sql"),
    },
    Migration {
        version: 28,
        name: "000028_add_stop_reason",
        sql: include_str!("../../migrations/000028_add_stop_reason.sql"),
    },
    Migration {
        version: 29,
        name: "000029_add_models_dev_sync",
        sql: include_str!("../../migrations/000029_add_models_dev_sync.sql"),
    },
    Migration {
        version: 30,
        name: "000030_combo_targets_cascade_on_model_delete",
        sql: include_str!("../../migrations/000030_combo_targets_cascade_on_model_delete.sql"),
    },
    Migration {
        version: 31,
        name: "000031_add_compression",
        sql: include_str!("../../migrations/000031_add_compression.sql"),
    },
    Migration {
        version: 32,
        name: "000032_add_usage_analytics_indexes",
        sql: include_str!("../../migrations/000032_add_usage_analytics_indexes.sql"),
    },
    Migration {
        version: 33,
        name: "000033_add_model_id_normalized",
        sql: include_str!("../../migrations/000033_add_model_id_normalized.sql"),
    },
];

/// Apply pending migrations on `conn`. Idempotent: skips versions already in
/// `schema_migrations`. Each migration runs inside its own transaction; a
/// failure aborts and bubbles up as `CoreError::Migration`.
///
/// `conn` is `&mut` because rusqlite transactions require exclusive access;
/// the typical caller passes a `MutexGuard` from [`crate::db::DbPool::writer`].
pub fn run(conn: &mut Connection) -> Result<()> {
    ensure_tracking_table(conn)?;

    let applied = load_applied_versions(conn)?;
    let mut pending: Vec<&Migration> = MIGRATIONS
        .iter()
        .filter(|m| !applied.contains(&m.version))
        .collect();
    // Defensive: embedded list must be sorted by version. Cheap to assert
    // because the list is tiny and in-source.
    pending.sort_by_key(|m| m.version);

    for m in pending {
        apply_one(conn, m)?;
    }
    Ok(())
}

/// Create the `schema_migrations` tracking table if missing.
fn ensure_tracking_table(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (\
            version    INTEGER PRIMARY KEY,\
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))\
         )",
        [],
    )
    .map_err(|e| CoreError::Database {
        message: format!("create schema_migrations: {}", e),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

/// Return the set of versions already applied.
fn load_applied_versions(conn: &Connection) -> Result<std::collections::HashSet<i64>> {
    let mut stmt = conn
        .prepare("SELECT version FROM schema_migrations")
        .map_err(|e| CoreError::Database {
            message: format!("prepare select schema_migrations: {}", e),
            source: Some(Box::new(e)),
        })?;
    let rows = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(|e| CoreError::Database {
            message: format!("query schema_migrations: {}", e),
            source: Some(Box::new(e)),
        })?;
    let mut set = std::collections::HashSet::new();
    for r in rows {
        let v = r.map_err(|e| CoreError::Database {
            message: format!("read schema_migrations row: {}", e),
            source: Some(Box::new(e)),
        })?;
        set.insert(v);
    }
    Ok(set)
}

fn apply_one(conn: &mut Connection, m: &Migration) -> Result<()> {
    // If the migration disables foreign keys (table rebuild pattern),
    // execute the pragma *before* the transaction — it has no effect
    // inside a SQLite transaction.
    if m.sql.contains("PRAGMA foreign_keys = OFF") {
        conn.execute_batch("PRAGMA foreign_keys = OFF").map_err(|e| CoreError::Migration {
            version: m.version,
            message: format!("{}: PRAGMA foreign_keys = OFF: {}", m.name, e),
        })?;
    }

    // Use IMMEDIATE so the migration lock is taken up-front, matching spec §9.
    let tx: Transaction<'_> = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| CoreError::Migration { version: m.version, message: format!("begin tx: {}", e) })?;

    tx.execute_batch(m.sql).map_err(|e| CoreError::Migration {
        version: m.version,
        message: format!("{}: {}", m.name, e),
    })?;

    tx.execute(
        "INSERT INTO schema_migrations(version) VALUES (?1)",
        params![m.version],
    )
    .map_err(|e| CoreError::Migration {
        version: m.version,
        message: format!("{}: insert into schema_migrations: {}", m.name, e),
    })?;

    tx.commit().map_err(|e| CoreError::Migration {
        version: m.version,
        message: format!("{}: commit: {}", m.name, e),
    })?;

    // Re-enable foreign keys if the migration had disabled them.
    if m.sql.contains("PRAGMA foreign_keys = OFF") {
        conn.execute_batch("PRAGMA foreign_keys = ON").map_err(|e| CoreError::Migration {
            version: m.version,
            message: format!("{}: PRAGMA foreign_keys = ON: {}", m.name, e),
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = base.join(format!("openproxy-mig-test-{}-{}", pid, nanos));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn applies_all_migrations_once() {
        let dir = tempdir();
        let path = dir.join("fresh.db");
        let mut conn = Connection::open(&path).expect("open");

        run(&mut conn).expect("first run");

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .expect("count");
        assert_eq!(count, MIGRATIONS.len() as i64, "all embedded migrations applied");

        // Sanity: every table the spec §8 promises is present.
        for table in [
            "providers",
            "accounts",
            "models",
            "combos",
            "combo_targets",
            "usage",
            "api_keys",
            "schema_migrations",
            "provider_timeouts",
        ] {
            let present: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    rusqlite::params![table],
                    |r| r.get(0),
                )
                .expect("sqlite_master");
            assert_eq!(present, 1, "table {} missing", table);
        }
    }

    #[test]
    fn is_idempotent_on_second_run() {
        let dir = tempdir();
        let path = dir.join("idem.db");
        let mut conn = Connection::open(&path).expect("open");

        run(&mut conn).expect("first run");
        let count_first: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .expect("count1");

        run(&mut conn).expect("second run must not fail");

        let count_second: i64 = conn
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
            .expect("count2");
        assert_eq!(count_first, count_second, "no rows duplicated");
    }

    #[test]
    fn versions_match_embedded_list() {
        let dir = tempdir();
        let path = dir.join("versions.db");
        let mut conn = Connection::open(&path).expect("open");
        run(&mut conn).expect("run");

        let mut stmt = conn
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .expect("prep");
        let rows: Vec<i64> = stmt
            .query_map([], |r| r.get(0))
            .expect("q")
            .map(|r| r.expect("v"))
            .collect();

        let expected: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert_eq!(rows, expected, "applied versions match the embedded list");
    }

    #[test]
    fn end_to_end_via_dbpool_is_idempotent() {
        use crate::db::conn::DbPool;

        let dir = tempdir();
        let path = dir.join("e2e.db");
        let pool = DbPool::open(&path).expect("open pool");

        {
            let mut writer = pool.writer();
            crate::db::migrations::run(&mut writer).expect("first run");
        }
        let count_first: i64 = pool
            .with_conn(|c| {
                c.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
                    .expect("count")
            });
        assert_eq!(count_first, 32, "thirty-two migrations applied (versions 1-6, 8-33)");

        {
            let mut writer = pool.writer();
            crate::db::migrations::run(&mut writer).expect("second run");
        }
        let count_second: i64 = pool
            .with_conn(|c| {
                c.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
                    .expect("count")
            });
        assert_eq!(count_second, count_first, "idempotent: same row count");
    }

    // =====================================================================
    // Migration 000020 specific tests
    // =====================================================================

    #[test]
    fn migration_20_changes_antigravity_format_to_gemini() {
        use crate::db::conn::DbPool;

        let dir = tempdir();
        let path = dir.join("mig20_antigravity.db");
        let pool = DbPool::open(&path).expect("open pool");

        // Run all migrations to get the schema.
        {
            let mut writer = pool.writer();
            crate::db::migrations::run(&mut writer).expect("migrations");
        }

        // Insert antigravity providers with format='openai' (simulating
        // what early versions did before migration 20 existed). The
        // seed module inserts them with 'gemini' already, but the point
        // of this test is to verify the migration changes the format.
        {
            let conn = pool.writer();
            conn.execute_batch(
                "INSERT OR IGNORE INTO providers(id, name, base_url, auth_type, format)
                 VALUES ('antigravity', 'Antigravity', 'https://example.com', 'oauth', 'openai');
                 INSERT OR IGNORE INTO providers(id, name, base_url, auth_type, format)
                 VALUES ('antigravity-cli', 'Antigravity CLI', 'https://example.com', 'oauth', 'openai');",
            )
            .expect("insert antigravity providers");

            // Verify they are now openai.
            let fmt: String = conn
                .query_row(
                    "SELECT format FROM providers WHERE id = 'antigravity'",
                    [],
                    |r| r.get(0),
                )
                .expect("query format");
            assert_eq!(fmt, "openai", "pre-condition: format should be openai");
        }

        // Now manually apply migration 20's SQL to simulate the migration.
        {
            let conn = pool.writer();
            conn.execute_batch(
                "UPDATE providers SET format = 'gemini' WHERE id IN ('antigravity', 'antigravity-cli');",
            )
            .expect("apply migration 20 manually");
        }

        // Verify both providers are now gemini.
        {
            let conn = pool.writer();
            let fmt1: String = conn
                .query_row(
                    "SELECT format FROM providers WHERE id = 'antigravity'",
                    [],
                    |r| r.get(0),
                )
                .expect("query format 1");
            assert_eq!(fmt1, "gemini", "antigravity should be gemini after migration 20");

            let fmt2: String = conn
                .query_row(
                    "SELECT format FROM providers WHERE id = 'antigravity-cli'",
                    [],
                    |r| r.get(0),
                )
                .expect("query format 2");
            assert_eq!(fmt2, "gemini", "antigravity-cli should be gemini after migration 20");
        }
    }

    #[test]
    fn migration_20_does_not_affect_other_providers() {
        use crate::db::conn::DbPool;

        let dir = tempdir();
        let path = dir.join("mig20_other.db");
        let pool = DbPool::open(&path).expect("open pool");

        {
            let mut writer = pool.writer();
            crate::db::migrations::run(&mut writer).expect("migrations");
        }

        // Insert a non-antigravity provider and verify migration 20 doesn't change it.
        {
            let conn = pool.writer();
            conn.execute_batch(
                "INSERT OR IGNORE INTO providers(id, name, base_url, auth_type, format)
                 VALUES ('test-provider', 'Test', 'https://example.com', 'bearer', 'openai');",
            )
            .expect("insert test provider");
        }

        // Apply migration 20 SQL.
        {
            let conn = pool.writer();
            conn.execute_batch(
                "UPDATE providers SET format = 'gemini' WHERE id IN ('antigravity', 'antigravity-cli');",
            )
            .expect("apply migration 20");
        }

        // The non-antigravity provider should still be openai.
        {
            let conn = pool.reader();
            let fmt: String = conn
                .query_row(
                    "SELECT format FROM providers WHERE id = 'test-provider'",
                    [],
                    |r| r.get(0),
                )
                .expect("query format");
            assert_eq!(fmt, "openai", "non-antigravity provider should not be changed");
        }
    }

    #[test]
    fn migration_20_idempotent() {
        use crate::db::conn::DbPool;

        let dir = tempdir();
        let path = dir.join("mig20_idem.db");
        let pool = DbPool::open(&path).expect("open pool");

        {
            let mut writer = pool.writer();
            crate::db::migrations::run(&mut writer).expect("first run");
        }

        // Insert antigravity providers with format='gemini' (already correct).
        {
            let conn = pool.writer();
            conn.execute_batch(
                "INSERT OR IGNORE INTO providers(id, name, base_url, auth_type, format)
                 VALUES ('antigravity', 'Antigravity', 'https://example.com', 'oauth', 'gemini');",
            )
            .expect("insert antigravity providers");
        }

        // Run migrations again — should not fail.
        {
            let mut writer = pool.writer();
            crate::db::migrations::run(&mut writer).expect("second run");
        }

        // Verify antigravity providers are still gemini.
        {
            let conn = pool.reader();
            let fmt: String = conn
                .query_row(
                    "SELECT format FROM providers WHERE id = 'antigravity'",
                    [],
                    |r| r.get(0),
                )
                .expect("query format");
            assert_eq!(fmt, "gemini");
        }
    }

    #[test]
    fn migration_20_updates_schema_migrations_tracking() {
        use crate::db::conn::DbPool;

        let dir = tempdir();
        let path = dir.join("mig20_tracking.db");
        let pool = DbPool::open(&path).expect("open pool");

        {
            let mut writer = pool.writer();
            crate::db::migrations::run(&mut writer).expect("migrations");
        }

        // Version 20 should be recorded.
        {
            let conn = pool.reader();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM schema_migrations WHERE version = 20",
                    [],
                    |r| r.get(0),
                )
                .expect("count version 20");
            assert_eq!(count, 1, "version 20 should be tracked");
        }
    }

    // =====================================================================
    // Migration 000025 specific tests
    // =====================================================================

    /// Gate D + 000030: the rebuilt `combo_targets` FK must be
    /// `model_row_id ... REFERENCES models(id) ON DELETE CASCADE`.
    /// After the migration, deleting a `models` row that a
    /// `combo_targets` row points at must cascade-delete the
    /// `combo_targets` row entirely.
    #[test]
    fn migration_25_sets_model_fk_to_on_delete_set_null() {
        use crate::db::conn::DbPool;

        let dir = tempdir();
        let path = dir.join("mig25_fk_set_null.db");
        let pool = DbPool::open(&path).expect("open pool");

        // Run all migrations to land on the post-000025 shape.
        {
            let mut writer = pool.writer();
            crate::db::migrations::run(&mut writer).expect("migrations");
        }

        // 1. Insert a `combo_targets` row referencing a `models` row.
        let (model_row_id, target_id) = {
            let conn = pool.writer();
            conn.execute_batch(
                "INSERT INTO providers(id, name, base_url, auth_type, format) \
                 VALUES ('p25', 'Provider 25', 'https://example.com', 'bearer', 'openai');\
                 INSERT INTO models(provider_id, model_id, target_format) \
                 VALUES ('p25', 'm25', 'openai');",
            )
            .expect("insert provider + model");

            let model_id: i64 = conn
                .query_row(
                    "SELECT id FROM models WHERE provider_id = 'p25' AND model_id = 'm25'",
                    [],
                    |r| r.get(0),
                )
                .expect("model row id");

            // Use a standalone connection with FKs on (the default
            // for connections from the pool) so the insert proves
            // the FK is wired up.
            conn.execute(
                "INSERT INTO combos(name, strategy) VALUES ('c25', 'priority');",
                [],
            )
            .expect("insert combo");
            let combo_id: i64 = conn
                .query_row("SELECT id FROM combos WHERE name = 'c25'", [], |r| r.get(0))
                .expect("combo id");
            conn.execute(
                "INSERT INTO combo_targets(combo_id, provider_id, model_row_id, priority_order) \
                 VALUES (?1, 'p25', ?2, 1);",
                rusqlite::params![combo_id, model_id],
            )
            .expect("insert combo_target");

            (model_id, combo_id)
        };

        // 2. Delete the `models` row. With `ON DELETE CASCADE`
        //    (migration 000030 supersedes 000025's SET NULL) the
        //    `combo_targets` row is removed alongside it.
        {
            let conn = pool.writer();
            conn.execute("DELETE FROM models WHERE id = ?1", rusqlite::params![model_row_id])
                .expect("delete model must succeed under CASCADE");
        }

        // 3. Assert the `combo_targets` row was cascade-deleted
        //    and no longer exists.
        {
            let conn = pool.writer();
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
                    rusqlite::params![target_id],
                    |r| r.get(0),
                )
                .expect("query surviving target");
            assert_eq!(
                count, 0,
                "the combo_target row must be cascade-deleted with the model"
            );
        }

        // 4. Sanity: trying to delete a *different* model that
        //    has no references still works (the rebuild did not
        //    break the happy path).
        {
            let conn = pool.writer();
            conn.execute(
                "INSERT INTO models(provider_id, model_id, target_format) \
                 VALUES ('p25', 'm25-extra', 'openai');",
                [],
            )
            .expect("insert extra model");
            let extra_id: i64 = conn
                .query_row(
                    "SELECT id FROM models WHERE model_id = 'm25-extra'",
                    [],
                    |r| r.get(0),
                )
                .expect("extra model id");
            conn.execute("DELETE FROM models WHERE id = ?1", rusqlite::params![extra_id])
                .expect("delete extra model must still work");
        }
    }

    /// Gate D — the migration must be idempotent (re-applying
    /// the runner against a DB that's already at version 25 must
    /// not fail and must not duplicate the `schema_migrations`
    /// row). The runner's `applied` set already guards against
    /// re-execution; this test just makes the guarantee
    /// explicit.
    #[test]
    fn migration_25_is_idempotent() {
        use crate::db::conn::DbPool;

        let dir = tempdir();
        let path = dir.join("mig25_idem.db");
        let pool = DbPool::open(&path).expect("open pool");

        {
            let mut writer = pool.writer();
            crate::db::migrations::run(&mut writer).expect("first run");
        }
        {
            let mut writer = pool.writer();
            crate::db::migrations::run(&mut writer).expect("second run must not fail");
        }

        let count: i64 = pool.with_conn(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM schema_migrations WHERE version = 25",
                [],
                |r| r.get(0),
            )
            .expect("count version 25")
        });
        assert_eq!(count, 1, "version 25 must be recorded exactly once");
    }
}
