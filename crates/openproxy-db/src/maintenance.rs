//! Database maintenance, integrity checking, and diagnostics.

use openproxy_types::Result;
use rusqlite::Connection;

/// Run a PRAGMA integrity check on the connection.
pub fn integrity_check(conn: &Connection) -> String {
    conn.query_row("PRAGMA integrity_check;", [], |r| r.get::<_, String>(0))
        .unwrap_or_else(|e| format!("error: {e}"))
}

/// Checkpoint the WAL log.
pub fn checkpoint_wal(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "wal_checkpoint", "TRUNCATE")
        .map_err(crate::error::map_db_error)
}

/// Execute incremental vacuum.
pub fn incremental_vacuum(conn: &Connection, pages: i64) -> Result<()> {
    let _ = conn.pragma_update(None, "auto_vacuum", "INCREMENTAL");
    conn.pragma_update(None, "incremental_vacuum", pages)
        .map_err(crate::error::map_db_error)
}

/// Execute a full VACUUM.
pub fn vacuum(conn: &Connection) -> Result<()> {
    let _ = conn.pragma_update(None, "temp_store", "FILE");
    let res = conn.execute_batch("VACUUM;");
    let _ = conn.pragma_update(None, "temp_store", "MEMORY");
    res.map_err(crate::error::map_db_error)
}

/// List all non-internal SQLite user table names.
pub fn list_user_tables(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(crate::error::map_db_error)?;
    rows.map(|r| r.map_err(crate::error::map_db_error))
        .collect()
}

macro_rules! define_db_tables {
    ($(($variant:ident, $str:literal, $count_sql:literal)),* $(,)?) => {
        /// Known database tables in OpenProxy.
        #[repr(u8)]
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum DbTable {
            $($variant),*
        }

        const TABLE_STRINGS: &[&str] = &[
            $($str),*
        ];

        const COUNT_SQLS: &[&str] = &[
            $($count_sql),*
        ];

        impl DbTable {
            pub const ALL: &'static [DbTable] = &[
                $(DbTable::$variant),*
            ];

            #[inline]
            pub const fn as_str(&self) -> &'static str {
                TABLE_STRINGS[*self as usize]
            }

            #[inline]
            pub fn parse(s: &str) -> Option<Self> {
                Self::ALL.iter().copied().find(|t| t.as_str() == s)
            }

            #[inline]
            pub const fn count_sql(&self) -> &'static str {
                COUNT_SQLS[*self as usize]
            }
        }
    };
}

define_db_tables!(
    (Providers, "providers", "SELECT COUNT(*) FROM \"providers\""),
    (Accounts, "accounts", "SELECT COUNT(*) FROM \"accounts\""),
    (Models, "models", "SELECT COUNT(*) FROM \"models\""),
    (Combos, "combos", "SELECT COUNT(*) FROM \"combos\""),
    (ComboTargets, "combo_targets", "SELECT COUNT(*) FROM \"combo_targets\""),
    (Usage, "usage", "SELECT COUNT(*) FROM \"usage\""),
    (ApiKeys, "api_keys", "SELECT COUNT(*) FROM \"api_keys\""),
    (TargetCooldowns, "target_cooldowns", "SELECT COUNT(*) FROM \"target_cooldowns\""),
    (AppConfig, "app_config", "SELECT COUNT(*) FROM \"app_config\""),
    (OauthDeviceTickets, "oauth_device_tickets", "SELECT COUNT(*) FROM \"oauth_device_tickets\""),
    (ModelCapabilitiesSync, "model_capabilities_sync", "SELECT COUNT(*) FROM \"model_capabilities_sync\""),
    (Notifications, "notifications", "SELECT COUNT(*) FROM \"notifications\""),
    (FreeProxies, "free_proxies", "SELECT COUNT(*) FROM \"free_proxies\""),
    (SmartWarmupHistory, "smart_warmup_history", "SELECT COUNT(*) FROM \"smart_warmup_history\""),
    (ProxySources, "proxy_sources", "SELECT COUNT(*) FROM \"proxy_sources\""),
    (ProviderProxyCooldowns, "provider_proxy_cooldowns", "SELECT COUNT(*) FROM \"provider_proxy_cooldowns\""),
    (SchemaMigrations, "schema_migrations", "SELECT COUNT(*) FROM \"schema_migrations\""),
);

impl std::fmt::Display for DbTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Count rows in a specific table with zero runtime query allocations.
pub fn count_table_rows(conn: &Connection, table: DbTable) -> rusqlite::Result<i64> {
    conn.query_row(table.count_sql(), [], |r| r.get(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_db_table_roundtrip() {
        for &table in DbTable::ALL {
            let s = table.as_str();
            assert_eq!(DbTable::parse(s), Some(table));
            assert_eq!(table.to_string(), s);
            assert!(table.count_sql().starts_with("SELECT COUNT(*) FROM"));
        }
        assert_eq!(DbTable::parse("unknown_table"), None);
    }

    #[test]
    fn test_count_table_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE providers (id TEXT PRIMARY KEY)", [])
            .unwrap();
        conn.execute("INSERT INTO providers VALUES ('openai')", [])
            .unwrap();
        conn.execute("INSERT INTO providers VALUES ('anthropic')", [])
            .unwrap();

        let count = count_table_rows(&conn, DbTable::Providers).unwrap();
        assert_eq!(count, 2);
    }
}
