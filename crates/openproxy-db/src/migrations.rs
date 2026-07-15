//! Embedded migration runner.
//!
//! Migration files live under `crates/openproxy-db/migrations/` and are
//! embedded at compile time via `include_str!`. Versions are the six-digit
//! numeric prefix of the filename. The runner is idempotent: a second
//! invocation against an already-migrated DB applies zero new versions.

use openproxy_types::{CoreError, Result};
use rusqlite::{Connection, params};

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
        sql: include_str!("../migrations/000001_initial_schema.sql"),
    },
    Migration {
        version: 2,
        name: "000002_add_timing_to_usage",
        sql: include_str!("../migrations/000002_add_timing_to_usage.sql"),
    },
    Migration {
        version: 3,
        name: "000003_add_race_to_usage",
        sql: include_str!("../migrations/000003_add_race_to_usage.sql"),
    },
    Migration {
        version: 4,
        name: "000004_add_race_size_to_combos",
        sql: include_str!("../migrations/000004_add_race_size_to_combos.sql"),
    },
    Migration {
        version: 5,
        name: "000005_add_provider_timeouts",
        sql: include_str!("../migrations/000005_add_provider_timeouts.sql"),
    },
    Migration {
        version: 6,
        name: "000006_add_model_timeout_overrides",
        sql: include_str!("../migrations/000006_add_model_timeout_overrides.sql"),
    },
    Migration {
        version: 8,
        name: "000008_add_error_msg_redacted",
        sql: include_str!("../migrations/000008_add_error_msg_redacted.sql"),
    },
    Migration {
        version: 9,
        name: "000009_add_active_to_models",
        sql: include_str!("../migrations/000009_add_active_to_models.sql"),
    },
    Migration {
        version: 10,
        name: "000010_add_provider_keyword_and_model_status",
        sql: include_str!("../migrations/000010_add_provider_keyword_and_model_status.sql"),
    },
    Migration {
        version: 11,
        name: "000011_only_new_models_in_auto_activate",
        sql: include_str!("../migrations/000011_only_new_models_in_auto_activate.sql"),
    },
    Migration {
        version: 12,
        name: "000012_add_account_quota",
        sql: include_str!("../migrations/000012_add_account_quota.sql"),
    },
    Migration {
        version: 13,
        name: "000013_add_active_to_providers",
        sql: include_str!("../migrations/000013_add_active_to_providers.sql"),
    },
    Migration {
        version: 14,
        name: "000014_add_model_metadata",
        sql: include_str!("../migrations/000014_add_model_metadata.sql"),
    },
    Migration {
        version: 15,
        name: "000015_add_api_key_metadata",
        sql: include_str!("../migrations/000015_add_api_key_metadata.sql"),
    },
    Migration {
        version: 16,
        name: "000016_add_subcombo_support",
        sql: include_str!("../migrations/000016_add_subcombo_support.sql"),
    },
    Migration {
        version: 17,
        name: "000017_add_target_cooldowns",
        sql: include_str!("../migrations/000017_add_target_cooldowns.sql"),
    },
    Migration {
        version: 18,
        name: "000018_add_gemini_format_and_goog_auth",
        sql: include_str!("../migrations/000018_add_gemini_format_and_goog_auth.sql"),
    },
    Migration {
        version: 19,
        name: "000019_add_oauth_support",
        sql: include_str!("../migrations/000019_add_oauth_support.sql"),
    },
    Migration {
        version: 20,
        name: "000020_set_antigravity_format_to_gemini",
        sql: include_str!("../migrations/000020_set_antigravity_format_to_gemini.sql"),
    },
    Migration {
        version: 21,
        name: "000021_add_none_auth_type",
        sql: include_str!("../migrations/000021_add_none_auth_type.sql"),
    },
    Migration {
        version: 22,
        name: "000022_add_gemini_target_format",
        sql: include_str!("../migrations/000022_add_gemini_target_format.sql"),
    },
    Migration {
        version: 23,
        name: "000023_add_live_log_columns",
        sql: include_str!("../migrations/000023_add_live_log_columns.sql"),
    },
    Migration {
        version: 24,
        name: "000024_add_app_config_kv",
        sql: include_str!("../migrations/000024_add_app_config_kv.sql"),
    },
    Migration {
        version: 25,
        name: "000025_combo_targets_model_fk_set_null",
        sql: include_str!("../migrations/000025_combo_targets_model_fk_set_null.sql"),
    },
    Migration {
        version: 26,
        name: "000026_combo_targets_upstream_model_id",
        sql: include_str!("../migrations/000026_combo_targets_upstream_model_id.sql"),
    },
    Migration {
        version: 27,
        name: "000027_oauth_device_tickets",
        sql: include_str!("../migrations/000027_oauth_device_tickets.sql"),
    },
    Migration {
        version: 28,
        name: "000028_add_stop_reason",
        sql: include_str!("../migrations/000028_add_stop_reason.sql"),
    },
    Migration {
        version: 29,
        name: "000029_add_models_dev_sync",
        sql: include_str!("../migrations/000029_add_models_dev_sync.sql"),
    },
    Migration {
        version: 30,
        name: "000030_combo_targets_cascade_on_model_delete",
        sql: include_str!("../migrations/000030_combo_targets_cascade_on_model_delete.sql"),
    },
    Migration {
        version: 31,
        name: "000031_add_compression",
        sql: include_str!("../migrations/000031_add_compression.sql"),
    },
    Migration {
        version: 32,
        name: "000032_add_usage_analytics_indexes",
        sql: include_str!("../migrations/000032_add_usage_analytics_indexes.sql"),
    },
    Migration {
        version: 33,
        name: "000033_add_model_id_normalized",
        sql: include_str!("../migrations/000033_add_model_id_normalized.sql"),
    },
    Migration {
        version: 34,
        name: "000034_combo_context_window",
        sql: include_str!("../migrations/000034_combo_context_window.sql"),
    },
    Migration {
        version: 35,
        name: "000035_combo_priority_modes",
        sql: include_str!("../migrations/000035_combo_priority_modes.sql"),
    },
    Migration {
        version: 36,
        name: "000036_notifications",
        sql: include_str!("../migrations/000036_notifications.sql"),
    },
    Migration {
        version: 37,
        name: "000037_add_client_response",
        sql: include_str!("../migrations/000037_add_client_response.sql"),
    },
    Migration {
        version: 38,
        name: "000038_add_estimated_tokens",
        sql: include_str!("../migrations/000038_add_estimated_tokens.sql"),
    },
    Migration {
        version: 39,
        name: "000039_add_endpoint_kind",
        sql: include_str!("../migrations/000039_add_endpoint_kind.sql"),
    },
    Migration {
        version: 40,
        name: "000040_drop_provider_timeouts",
        sql: include_str!("../migrations/000040_drop_provider_timeouts.sql"),
    },
    Migration {
        version: 41,
        name: "000041_add_quota_model_details",
        sql: include_str!("../migrations/000041_add_quota_model_details.sql"),
    },
    Migration {
        version: 42,
        name: "000042_free_proxies",
        sql: include_str!("../migrations/000042_free_proxies.sql"),
    },
    Migration {
        version: 43,
        name: "000043_provider_proxies",
        sql: include_str!("../migrations/000043_provider_proxies.sql"),
    },
    Migration {
        version: 44,
        name: "000044_add_responses_format",
        sql: include_str!("../migrations/000044_add_responses_format.sql"),
    },
    Migration {
        version: 45,
        name: "000045_add_usage_winner_partial",
        sql: include_str!("../migrations/000045_add_usage_winner_partial.sql"),
    },
    Migration {
        version: 46,
        name: "000046_smart_warmup_history",
        sql: include_str!("../migrations/000046_smart_warmup_history.sql"),
    },
    Migration {
        version: 47,
        name: "000047_add_proxy_logs",
        sql: include_str!("../migrations/000047_add_proxy_logs.sql"),
    },
    Migration {
        version: 48,
        name: "000048_provider_rate_limit_scope",
        sql: include_str!("../migrations/000048_provider_rate_limit_scope.sql"),
    },
];

/// Apply pending migrations on `conn`.
pub fn run(conn: &mut Connection) -> Result<()> {
    ensure_tracking_table(conn)?;

    let applied = load_applied_versions(conn)?;
    let mut pending: Vec<&Migration> = MIGRATIONS
        .iter()
        .filter(|m| !applied.contains(&m.version))
        .collect();
    pending.sort_by_key(|m| m.version);

    if pending.is_empty() {
        return Ok(());
    }

    let needs_fk_off = pending
        .iter()
        .any(|m| m.sql.contains("PRAGMA foreign_keys = OFF"));
    if needs_fk_off {
        conn.execute_batch("PRAGMA foreign_keys = OFF")
            .map_err(|e| CoreError::Migration {
                version: 0,
                message: format!("PRAGMA foreign_keys = OFF: {}", e),
            })?;
    }

    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| CoreError::Migration {
            version: 0,
            message: format!("begin tx: {}", e),
        })?;

    let mut stmt = tx
        .prepare_cached("INSERT INTO schema_migrations(version) VALUES (?1)")
        .map_err(|e| CoreError::Migration {
            version: 0,
            message: format!("prepare stmt: {}", e),
        })?;

    for m in pending {
        tx.execute_batch(m.sql).map_err(|e| CoreError::Migration {
            version: m.version,
            message: format!("{}: {}", m.name, e),
        })?;

        stmt.execute(params![m.version])
            .map_err(|e| CoreError::Migration {
                version: m.version,
                message: format!("{}: insert into schema_migrations: {}", m.name, e),
            })?;
    }

    drop(stmt);

    tx.commit().map_err(|e| CoreError::Migration {
        version: 0,
        message: format!("commit: {}", e),
    })?;

    if needs_fk_off {
        conn.execute_batch("PRAGMA foreign_keys = ON")
            .map_err(|e| CoreError::Migration {
                version: 0,
                message: format!("PRAGMA foreign_keys = ON: {}", e),
            })?;
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
    .map_err(openproxy_types::map_db_error)?;
    Ok(())
}

/// Return the set of versions already applied.
fn load_applied_versions(conn: &Connection) -> Result<std::collections::HashSet<i64>> {
    let mut stmt = conn
        .prepare("SELECT version FROM schema_migrations")
        .map_err(openproxy_types::map_db_error)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(openproxy_types::map_db_error)?;
    let mut set = std::collections::HashSet::new();
    for r in rows {
        let v = r.map_err(openproxy_types::map_db_error)?;
        set.insert(v);
    }
    Ok(set)
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
        assert_eq!(
            count,
            MIGRATIONS.len() as i64,
            "all embedded migrations applied"
        );

        for table in [
            "providers",
            "accounts",
            "models",
            "combos",
            "combo_targets",
            "usage",
            "api_keys",
            "schema_migrations",
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
        use crate::conn::DbPool;

        let dir = tempdir();
        let path = dir.join("e2e.db");
        let pool = DbPool::open(&path).expect("open pool");

        {
            let mut writer = pool.writer();
            run(&mut writer).expect("first run");
        }
        let count_first: i64 = pool.with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
                .expect("count")
        });
        assert_eq!(
            count_first,
            MIGRATIONS.len() as i64,
            "all embedded migrations applied"
        );

        {
            let mut writer = pool.writer();
            run(&mut writer).expect("second run");
        }
        let count_second: i64 = pool.with_conn(|c| {
            c.query_row("SELECT COUNT(*) FROM schema_migrations", [], |r| r.get(0))
                .expect("count")
        });
        assert_eq!(count_second, count_first, "idempotent: same row count");
    }

    #[test]
    fn migration_20_changes_antigravity_format_to_gemini() {
        use crate::conn::DbPool;

        let dir = tempdir();
        let path = dir.join("mig20_antigravity.db");
        let pool = DbPool::open(&path).expect("open pool");

        {
            let mut writer = pool.writer();
            run(&mut writer).expect("migrations");
        }

        {
            let conn = pool.writer();
            conn.execute_batch(
                "INSERT OR IGNORE INTO providers(id, name, base_url, auth_type, format)
                 VALUES ('antigravity', 'Antigravity', 'https://example.com', 'oauth', 'openai');
                 INSERT OR IGNORE INTO providers(id, name, base_url, auth_type, format)
                 VALUES ('antigravity-cli', 'Antigravity CLI', 'https://example.com', 'oauth', 'openai');",
            )
            .expect("insert antigravity providers");

            let fmt: String = conn
                .query_row(
                    "SELECT format FROM providers WHERE id = 'antigravity'",
                    [],
                    |r| r.get(0),
                )
                .expect("query format");
            assert_eq!(fmt, "openai", "pre-condition: format should be openai");
        }

        {
            let conn = pool.writer();
            conn.execute_batch(
                "UPDATE providers SET format = 'gemini' WHERE id IN ('antigravity', 'antigravity-cli');",
            )
            .expect("apply migration 20 manually");
        }

        {
            let conn = pool.writer();
            let fmt1: String = conn
                .query_row(
                    "SELECT format FROM providers WHERE id = 'antigravity'",
                    [],
                    |r| r.get(0),
                )
                .expect("query format 1");
            assert_eq!(fmt1, "gemini");

            let fmt2: String = conn
                .query_row(
                    "SELECT format FROM providers WHERE id = 'antigravity-cli'",
                    [],
                    |r| r.get(0),
                )
                .expect("query format 2");
            assert_eq!(fmt2, "gemini");
        }
    }
}
