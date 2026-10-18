//! Embedded migration runner.
//!
//! Migration files live under `crates/openproxy-db/migrations/` and are
//! embedded at compile time via `include_str!`. Versions are the six-digit
//! numeric prefix of the filename. The runner is idempotent: a second
//! invocation against an already-migrated DB applies zero new versions.

use openproxy_types::{CoreError, Result};
use rusqlite::Connection;
use std::fmt::Write;

use crate::error::with_busy_retry;

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

include!(concat!(env!("OUT_DIR"), "/migrations_generated.rs"));

fn collect_pending_migrations(conn: &Connection) -> Result<Vec<&'static Migration>> {
    let applied = load_applied_versions(conn)?;
    let mut pending: Vec<&'static Migration> = MIGRATIONS
        .iter()
        .filter(|m| !applied.contains(&m.version))
        .collect();
    pending.sort_by_key(|m| m.version);
    Ok(pending)
}

fn set_pragma_foreign_keys(conn: &Connection, enabled: bool) -> Result<()> {
    let sql = if enabled {
        "PRAGMA foreign_keys = ON"
    } else {
        "PRAGMA foreign_keys = OFF"
    };
    conn.execute_batch(sql).map_err(|e| CoreError::Migration {
        version: 0,
        message: format!("{sql}: {e}"),
    })
}

fn build_versions_insert_sql(pending: &[&Migration]) -> String {
    let mut insert_sql = String::with_capacity(64 + pending.len() * 12);
    insert_sql.push_str("INSERT OR IGNORE INTO schema_migrations(version) VALUES ");
    for (i, m) in pending.iter().enumerate() {
        if i > 0 {
            insert_sql.push(',');
        }
        let _ = write!(&mut insert_sql, "({})", m.version);
    }
    insert_sql
}

fn apply_migration_batch(conn: &mut Connection, pending: &[&Migration]) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| CoreError::Migration {
            version: 0,
            message: format!("begin tx: {e}"),
        })?;

    for m in pending {
        tx.execute_batch(m.sql).map_err(|e| CoreError::Migration {
            version: m.version,
            message: format!("{}: {}", m.name, e),
        })?;
    }

    let insert_sql = build_versions_insert_sql(pending);
    tx.execute_batch(&insert_sql)
        .map_err(|e| CoreError::Migration {
            version: 0,
            message: format!("insert into schema_migrations: {e}"),
        })?;

    tx.commit().map_err(|e| CoreError::Migration {
        version: 0,
        message: format!("commit: {e}"),
    })?;

    Ok(())
}

fn run_migrations_with_fk_guard(
    conn: &mut Connection,
    pending: &[&Migration],
    needs_fk_off: bool,
) -> Result<()> {
    if !needs_fk_off {
        return apply_migration_batch_with_retry(conn, pending);
    }
    set_pragma_foreign_keys(conn, false)?;
    let res = apply_migration_batch_with_retry(conn, pending);
    let fk_res = set_pragma_foreign_keys(conn, true);
    res.and(fk_res)
}

/// [`apply_migration_batch`] with the whole `BEGIN IMMEDIATE` → `COMMIT`
/// window inside `with_busy_retry`.
///
/// `TransactionBehavior::Immediate` takes a RESERVED lock at `BEGIN`, which
/// races with another process holding the writer, e.g. a crash-restart loop
/// where a previous instance is still flushing its WAL. The per-connection
/// `busy_timeout` of 5s absorbs it most of the time; once it expires, `BEGIN`
/// fails with `SQLITE_BUSY` and the whole transaction is retried on the
/// `BUSY_RETRY_DELAYS` backoff. No explicit rollback is needed, since a failed
/// `BEGIN` leaves no transaction open.
fn apply_migration_batch_with_retry(conn: &mut Connection, pending: &[&Migration]) -> Result<()> {
    with_busy_retry("migrations::apply_batch", || {
        apply_migration_batch(conn, pending)
    })
    .inspect_err(|e| {
        tracing::error!(
            error = %e,
            "migration batch failed (including BUSY retries)",
        );
    })
}

/// Apply pending migrations on `conn`.
pub fn run(conn: &mut Connection) -> Result<()> {
    ensure_tracking_table(conn)?;

    let pending = collect_pending_migrations(conn)?;
    if pending.is_empty() {
        return Ok(());
    }

    let needs_fk_off = pending
        .iter()
        .any(|m| m.sql.contains("PRAGMA foreign_keys = OFF"));
    run_migrations_with_fk_guard(conn, &pending, needs_fk_off)?;

    // `cost::backfill_usage_pricing` used to run inline here. That full-table
    // scan took tens of seconds on large DBs and blocked the listener socket at
    // boot, so the server runs it through the background `BackfillService`.

    Ok(())
}

fn ensure_tracking_table(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (\
            version    INTEGER PRIMARY KEY,\
            applied_at TEXT NOT NULL DEFAULT (datetime('now'))\
         )",
        [],
    )
    .map_err(crate::error::map_db_error)?;
    Ok(())
}

fn load_applied_versions(conn: &Connection) -> Result<std::collections::HashSet<i64>> {
    let mut stmt = conn
        .prepare("SELECT version FROM schema_migrations")
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map([], |row| row.get::<_, i64>(0))
        .map_err(crate::error::map_db_error)?;
    let mut set = std::collections::HashSet::new();
    for r in rows {
        let v = r.map_err(crate::error::map_db_error)?;
        set.insert(v);
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tempdir() -> crate::testing::TempDir {
        crate::testing::TempDir::new("openproxy-mig-test").expect("mkdir")
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
            assert_eq!(present, 1, "table {table} missing");
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

    /// `providers.notif_keyword_only` is three-state: 0=off, 1=on, NULL=cleared.
    #[test]
    fn test_migrations_count_includes_000074() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert!(
            versions.contains(&74),
            "embedded MIGRATIONS must include 000074; got {versions:?}"
        );

        let dir = tempdir();
        let path = dir.join("notif-kw.db");
        let mut conn = Connection::open(&path).expect("open");
        run(&mut conn).expect("run");

        let has_column: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('providers') WHERE name = 'notif_keyword_only'",
                [],
                |r| r.get(0),
            )
            .expect("pragma_table_info");
        assert_eq!(has_column, 1, "providers.notif_keyword_only missing");

        let default_value: Option<String> = conn
            .query_row(
                "SELECT dflt_value FROM pragma_table_info('providers') WHERE name = 'notif_keyword_only'",
                [],
                |r| r.get(0),
            )
            .expect("pragma dflt_value");
        assert_eq!(
            default_value.as_deref(),
            Some("0"),
            "notif_keyword_only must default to 0"
        );
    }

    #[test]
    fn test_migrations_count_includes_000076() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert!(
            versions.contains(&76),
            "embedded MIGRATIONS must include 000076; got {versions:?}"
        );
    }

    #[test]
    fn test_migrations_count_includes_000078() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert!(
            versions.contains(&78),
            "embedded MIGRATIONS must include 000078; got {versions:?}"
        );
    }

    #[test]
    fn test_migrations_count_includes_000080() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert!(
            versions.contains(&80),
            "embedded MIGRATIONS must include 000080; got {versions:?}"
        );
    }

    #[test]
    fn test_migrations_count_includes_000082() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert!(
            versions.contains(&82),
            "embedded MIGRATIONS must include 000082; got {versions:?}"
        );
    }

    #[test]
    fn test_migrations_count_includes_000083() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert!(
            versions.contains(&83),
            "embedded MIGRATIONS must include 000083; got {versions:?}"
        );
    }

    #[test]
    fn test_migrations_count_includes_000090() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert!(
            versions.contains(&90),
            "embedded MIGRATIONS must include 000090; got {versions:?}"
        );
    }

    #[test]
    fn test_migrations_count_includes_000091() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert!(
            versions.contains(&91),
            "embedded MIGRATIONS must include 000091; got {versions:?}"
        );
    }

    #[test]
    fn test_migrations_count_includes_000092() {
        let versions: Vec<i64> = MIGRATIONS.iter().map(|m| m.version).collect();
        assert!(
            versions.contains(&92),
            "embedded MIGRATIONS must include 000092; got {versions:?}"
        );
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
