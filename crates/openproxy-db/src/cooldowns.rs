use openproxy_types::ids::{ComboId, ComboTargetId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cooldown {
    pub combo_target_id: ComboTargetId,
    pub cooldown_until: String,
    pub reason: Option<String>,
    pub failure_count: u32,
    pub updated_at: String,
}

pub fn list_for_combo(
    conn: &rusqlite::Connection,
    combo_id: ComboId,
) -> openproxy_types::error::Result<Vec<Cooldown>> {
    let mut stmt = conn.prepare(
        "SELECT tc.combo_target_id, tc.cooldown_until, tc.reason, tc.failure_count, tc.updated_at
         FROM target_cooldowns tc
         INNER JOIN combo_targets ct ON ct.id = tc.combo_target_id
         WHERE ct.combo_id = ?1",
    ).map_err(crate::error::map_db_error)?;

    let rows = stmt
        .query_map(rusqlite::params![combo_id.0], |row| {
            Ok(Cooldown {
                combo_target_id: ComboTargetId(row.get(0)?),
                cooldown_until: row.get(1)?,
                reason: row.get(2)?,
                failure_count: row.get(3)?,
                updated_at: row.get(4)?,
            })
        })
        .map_err(crate::error::map_db_error)?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(crate::error::map_db_error)?);
    }
    Ok(out)
}

pub fn index_for_combo(
    conn: &rusqlite::Connection,
    combo_id: ComboId,
) -> openproxy_types::error::Result<std::collections::HashMap<i64, Cooldown>> {
    let list = list_for_combo(conn, combo_id)?;
    let mut map = std::collections::HashMap::new();
    for c in list {
        map.insert(c.combo_target_id.0, c);
    }
    Ok(map)
}

pub fn get_for_target(
    conn: &rusqlite::Connection,
    target_id: ComboTargetId,
) -> openproxy_types::error::Result<Option<Cooldown>> {
    use rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT combo_target_id, cooldown_until, reason, failure_count, updated_at
         FROM target_cooldowns
         WHERE combo_target_id = ?1",
        rusqlite::params![target_id.0],
        |row| {
            Ok(Cooldown {
                combo_target_id: ComboTargetId(row.get(0)?),
                cooldown_until: row.get(1)?,
                reason: row.get(2)?,
                failure_count: row.get(3)?,
                updated_at: row.get(4)?,
            })
        },
    )
    .optional()
    .map_err(crate::error::map_db_error)
}

pub fn clear_cooldown(
    conn: &rusqlite::Connection,
    target_id: ComboTargetId,
) -> openproxy_types::error::Result<()> {
    conn.execute(
        "DELETE FROM target_cooldowns WHERE combo_target_id = ?1",
        rusqlite::params![target_id.0],
    )
    .map(|_| ())
    .map_err(crate::error::map_db_error)
}

use std::sync::{LazyLock, RwLock};
use std::time::Instant;

static PROVIDER_PROXY_COOLDOWNS: LazyLock<RwLock<std::collections::HashMap<(String, String), Instant>>> =
    LazyLock::new(|| RwLock::new(std::collections::HashMap::new()));

pub fn add_provider_proxy_cooldown(provider_id: &str, proxy_id: &str, duration: std::time::Duration) {
    if let Ok(mut map) = PROVIDER_PROXY_COOLDOWNS.write() {
        map.insert((provider_id.to_string(), proxy_id.to_string()), Instant::now() + duration);
    }
}

pub fn is_provider_proxy_in_cooldown(provider_id: &str, proxy_id: &str) -> bool {
    if let Ok(map) = PROVIDER_PROXY_COOLDOWNS.read() {
        if let Some(until) = map.get(&(provider_id.to_string(), proxy_id.to_string())) {
            return Instant::now() < *until;
        }
    }
    false
}

