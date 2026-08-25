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

/// Known database tables in OpenProxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbTable {
    Providers,
    Accounts,
    Models,
    Combos,
    ComboTargets,
    Usage,
    ApiKeys,
    TargetCooldowns,
    AppConfig,
    OauthDeviceTickets,
    ModelCapabilitiesSync,
    Notifications,
    FreeProxies,
    SmartWarmupHistory,
    ProxySources,
    ProviderProxyCooldowns,
    SchemaMigrations,
}

impl DbTable {
    pub const ALL: &'static [DbTable] = &[
        DbTable::Providers,
        DbTable::Accounts,
        DbTable::Models,
        DbTable::Combos,
        DbTable::ComboTargets,
        DbTable::Usage,
        DbTable::ApiKeys,
        DbTable::TargetCooldowns,
        DbTable::AppConfig,
        DbTable::OauthDeviceTickets,
        DbTable::ModelCapabilitiesSync,
        DbTable::Notifications,
        DbTable::FreeProxies,
        DbTable::SmartWarmupHistory,
        DbTable::ProxySources,
        DbTable::ProviderProxyCooldowns,
        DbTable::SchemaMigrations,
    ];

    #[inline]
    pub const fn as_str(&self) -> &'static str {
        match self {
            DbTable::Providers => "providers",
            DbTable::Accounts => "accounts",
            DbTable::Models => "models",
            DbTable::Combos => "combos",
            DbTable::ComboTargets => "combo_targets",
            DbTable::Usage => "usage",
            DbTable::ApiKeys => "api_keys",
            DbTable::TargetCooldowns => "target_cooldowns",
            DbTable::AppConfig => "app_config",
            DbTable::OauthDeviceTickets => "oauth_device_tickets",
            DbTable::ModelCapabilitiesSync => "model_capabilities_sync",
            DbTable::Notifications => "notifications",
            DbTable::FreeProxies => "free_proxies",
            DbTable::SmartWarmupHistory => "smart_warmup_history",
            DbTable::ProxySources => "proxy_sources",
            DbTable::ProviderProxyCooldowns => "provider_proxy_cooldowns",
            DbTable::SchemaMigrations => "schema_migrations",
        }
    }

    #[inline]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "providers" => Some(DbTable::Providers),
            "accounts" => Some(DbTable::Accounts),
            "models" => Some(DbTable::Models),
            "combos" => Some(DbTable::Combos),
            "combo_targets" => Some(DbTable::ComboTargets),
            "usage" => Some(DbTable::Usage),
            "api_keys" => Some(DbTable::ApiKeys),
            "target_cooldowns" => Some(DbTable::TargetCooldowns),
            "app_config" => Some(DbTable::AppConfig),
            "oauth_device_tickets" => Some(DbTable::OauthDeviceTickets),
            "model_capabilities_sync" => Some(DbTable::ModelCapabilitiesSync),
            "notifications" => Some(DbTable::Notifications),
            "free_proxies" => Some(DbTable::FreeProxies),
            "smart_warmup_history" => Some(DbTable::SmartWarmupHistory),
            "proxy_sources" => Some(DbTable::ProxySources),
            "provider_proxy_cooldowns" => Some(DbTable::ProviderProxyCooldowns),
            "schema_migrations" => Some(DbTable::SchemaMigrations),
            _ => None,
        }
    }

    #[inline]
    pub const fn count_sql(&self) -> &'static str {
        match self {
            DbTable::Providers => "SELECT COUNT(*) FROM \"providers\"",
            DbTable::Accounts => "SELECT COUNT(*) FROM \"accounts\"",
            DbTable::Models => "SELECT COUNT(*) FROM \"models\"",
            DbTable::Combos => "SELECT COUNT(*) FROM \"combos\"",
            DbTable::ComboTargets => "SELECT COUNT(*) FROM \"combo_targets\"",
            DbTable::Usage => "SELECT COUNT(*) FROM \"usage\"",
            DbTable::ApiKeys => "SELECT COUNT(*) FROM \"api_keys\"",
            DbTable::TargetCooldowns => "SELECT COUNT(*) FROM \"target_cooldowns\"",
            DbTable::AppConfig => "SELECT COUNT(*) FROM \"app_config\"",
            DbTable::OauthDeviceTickets => "SELECT COUNT(*) FROM \"oauth_device_tickets\"",
            DbTable::ModelCapabilitiesSync => "SELECT COUNT(*) FROM \"model_capabilities_sync\"",
            DbTable::Notifications => "SELECT COUNT(*) FROM \"notifications\"",
            DbTable::FreeProxies => "SELECT COUNT(*) FROM \"free_proxies\"",
            DbTable::SmartWarmupHistory => "SELECT COUNT(*) FROM \"smart_warmup_history\"",
            DbTable::ProxySources => "SELECT COUNT(*) FROM \"proxy_sources\"",
            DbTable::ProviderProxyCooldowns => "SELECT COUNT(*) FROM \"provider_proxy_cooldowns\"",
            DbTable::SchemaMigrations => "SELECT COUNT(*) FROM \"schema_migrations\"",
        }
    }
}

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
