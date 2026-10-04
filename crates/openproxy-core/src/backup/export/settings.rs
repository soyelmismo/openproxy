//! Proxy sources, API keys, and app configuration backup export helpers.

use openproxy_db::error::map_db_error;
use openproxy_types::Result;
use openproxy_types::backup::{BackupApiKey, BackupAppConfig, BackupProxySource};
use rusqlite::Connection;

pub(super) fn export_proxy_sources(conn: &Connection) -> Result<Vec<BackupProxySource>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, url, priority, active, is_builtin \
             FROM proxy_sources ORDER BY priority DESC, id ASC",
        )
        .map_err(map_db_error)?;

    let proxy_sources = stmt
        .query_map([], |row| {
            Ok(BackupProxySource {
                id: row.get(0)?,
                name: row.get(1)?,
                url: row.get(2)?,
                priority: row.get(3)?,
                active: row.get::<_, i64>(4)? != 0,
                is_builtin: row.get::<_, i64>(5)? != 0,
            })
        })
        .map_err(map_db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_db_error)?;

    Ok(proxy_sources)
}

pub(super) fn export_api_keys(conn: &Connection) -> Result<Vec<BackupApiKey>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, key_hash, key_prefix, label, scopes_json, \
             allowed_models_json, allowed_combos_json, is_active, revoked_at, \
             expires_at, created_by, blacklisted_providers_json, \
             blacklisted_models_json FROM api_keys ORDER BY id",
        )
        .map_err(map_db_error)?;

    let api_keys = stmt
        .query_map([], |row| {
            Ok(BackupApiKey {
                id: row.get(0)?,
                key_hash: row.get(1)?,
                key_prefix: row.get(2)?,
                label: row.get(3)?,
                scopes_json: row.get(4)?,
                allowed_models_json: row.get(5)?,
                allowed_combos_json: row.get(6)?,
                is_active: row.get::<_, i64>(7)? != 0,
                revoked_at: row.get(8)?,
                expires_at: row.get(9)?,
                created_by: row.get(10)?,
                blacklisted_providers_json: row.get(11)?,
                blacklisted_models_json: row.get(12)?,
            })
        })
        .map_err(map_db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_db_error)?;

    Ok(api_keys)
}

pub(super) fn export_app_config(conn: &Connection) -> Result<Vec<BackupAppConfig>> {
    let mut stmt = conn
        .prepare("SELECT key, value, updated_at FROM app_config ORDER BY key")
        .map_err(map_db_error)?;

    let app_config = stmt
        .query_map([], |row| {
            Ok(BackupAppConfig {
                key: row.get(0)?,
                value: row.get(1)?,
                updated_at: row.get(2)?,
            })
        })
        .map_err(map_db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_db_error)?;

    Ok(app_config)
}
