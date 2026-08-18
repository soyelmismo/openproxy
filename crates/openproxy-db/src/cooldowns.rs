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

crate::def_table_select!(
    target_cooldown_select,
    "target_cooldowns tc",
    "tc.combo_target_id, tc.cooldown_until, tc.reason, tc.failure_count, tc.updated_at"
);

crate::def_table_select!(
    provider_proxy_cooldown_select,
    "provider_proxy_cooldowns",
    "cooldown_until"
);

crate::def_table_select!(
    target_cooldown_failure_count_select,
    "target_cooldowns",
    "failure_count"
);

impl crate::crud::FromRow for Cooldown {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        crate::map_row_struct!(row, Cooldown {
            combo_target_id: @id(0, ComboTargetId),
            cooldown_until: 1,
            reason: 2,
            failure_count: @u32(3),
            updated_at: 4,
        })
    }
}

pub fn list_for_combo(
    conn: &rusqlite::Connection,
    combo_id: ComboId,
) -> openproxy_types::error::Result<Vec<Cooldown>> {
    crate::db_query_all!(
        conn,
        target_cooldown_select!(
            "INNER JOIN combo_targets ct ON ct.id = tc.combo_target_id WHERE ct.combo_id = ?1"
        ),
        rusqlite::params![combo_id.0],
        format!("list cooldowns for combo {}", combo_id.0)
    )
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
    crate::db_query_one!(
        conn,
        target_cooldown_select!("WHERE tc.combo_target_id = ?1"),
        rusqlite::params![target_id.0],
        format!("get cooldown for target {}", target_id.0)
    )
}

pub fn clear_cooldown(
    conn: &rusqlite::Connection,
    target_id: ComboTargetId,
) -> openproxy_types::error::Result<()> {
    crate::db_execute!(
        conn,
        "DELETE FROM target_cooldowns WHERE combo_target_id = ?1",
        rusqlite::params![target_id.0],
        format!("clear cooldown for target {}", target_id.0)
    )?;
    Ok(())
}

pub fn prune_expired(conn: &rusqlite::Connection) -> openproxy_types::error::Result<usize> {
    let now = chrono::Utc::now().to_rfc3339();
    let n = conn
        .execute(
            "DELETE FROM target_cooldowns WHERE datetime(cooldown_until) <= datetime(?1)",
            rusqlite::params![now],
        )
        .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?;
    Ok(n)
}

use rusqlite::OptionalExtension;

pub fn add_provider_proxy_cooldown(
    conn: &rusqlite::Connection,
    provider_id: &str,
    proxy_id: &str,
    duration: std::time::Duration,
) -> openproxy_types::error::Result<()> {
    let until = chrono::Utc::now()
        + chrono::Duration::from_std(duration).unwrap_or_else(|_| chrono::Duration::seconds(900));
    let until_str = until.to_rfc3339();
    conn.execute(
        "INSERT INTO provider_proxy_cooldowns (provider_id, proxy_id, cooldown_until) \
         VALUES (?1, ?2, ?3) \
         ON CONFLICT(provider_id, proxy_id) DO UPDATE SET \
            cooldown_until = excluded.cooldown_until, \
            created_at = datetime('now')",
        rusqlite::params![provider_id, proxy_id, until_str],
    )
    .map_err(crate::error::map_db_error)?;
    Ok(())
}

pub fn is_provider_proxy_in_cooldown(
    conn: &rusqlite::Connection,
    provider_id: &str,
    proxy_id: &str,
) -> bool {
    let Ok(Some(until_str)) = conn
        .query_row(
            provider_proxy_cooldown_select!("WHERE provider_id = ?1 AND proxy_id = ?2"),
            rusqlite::params![provider_id, proxy_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
    else {
        return false;
    };

    let now = chrono::Utc::now();
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&until_str) {
        return now < dt.with_timezone(&chrono::Utc);
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(&until_str, "%Y-%m-%d %H:%M:%S") {
        let dt = chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(naive, chrono::Utc);
        return now < dt;
    }
    false
}

pub fn record_cooldown(
    conn: &rusqlite::Connection,
    target_id: ComboTargetId,
    reason: &str,
    mode: openproxy_types::CooldownMode,
    base_secs: u64,
    max_secs: u64,
    factor: u32,
) -> openproxy_types::error::Result<()> {
    if mode == openproxy_types::CooldownMode::None || base_secs == 0 {
        return Ok(());
    }

    let current_count: u32 = conn
        .query_row(
            target_cooldown_failure_count_select!("WHERE combo_target_id = ?1"),
            rusqlite::params![target_id.0],
            |row| row.get(0),
        )
        .unwrap_or(0);

    let new_count = current_count + 1;

    let cooldown_secs = match mode {
        openproxy_types::CooldownMode::None => return Ok(()),
        openproxy_types::CooldownMode::Flat => base_secs,
        openproxy_types::CooldownMode::Exponential => {
            let mut exp_secs =
                base_secs.saturating_mul(u64::from(factor).saturating_pow(current_count));
            if exp_secs > max_secs {
                exp_secs = max_secs;
            }
            exp_secs
        }
    };

    let cooldown_until = chrono::Utc::now() + chrono::Duration::seconds(cooldown_secs as i64);
    let cooldown_until_str = cooldown_until.to_rfc3339();
    conn.execute(
        "INSERT INTO target_cooldowns (combo_target_id, cooldown_until, reason, failure_count, updated_at) \
         VALUES (?1, ?2, ?3, ?4, datetime('now')) \
         ON CONFLICT(combo_target_id) DO UPDATE SET \
             cooldown_until = excluded.cooldown_until, \
             reason = excluded.reason, \
             failure_count = excluded.failure_count, \
             updated_at = excluded.updated_at",
        rusqlite::params![target_id.0, cooldown_until_str, reason, new_count],
    )
    .map(|_| ())
    .map_err(crate::error::map_db_error_ctx("record_cooldown"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fresh_db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::migrations::run(&mut conn).unwrap();
        conn
    }

    #[test]
    fn test_provider_proxy_cooldown_persistence_and_isolation() {
        let conn = fresh_db();
        conn.execute(
            "INSERT INTO providers (id, name, base_url, auth_type, format) \
             VALUES ('opencode-zen', 'Zen', 'https://example.com', 'none', 'openai'), \
                    ('cline', 'Cline', 'https://example.com', 'none', 'openai')",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO free_proxies (id, source, host, port, type, status) \
             VALUES ('proxy-1', 'test', '1.1.1.1', 8080, 'http', 'alive')",
            [],
        )
        .unwrap();

        assert!(!is_provider_proxy_in_cooldown(
            &conn,
            "opencode-zen",
            "proxy-1"
        ));
        assert!(!is_provider_proxy_in_cooldown(&conn, "cline", "proxy-1"));

        // Put proxy-1 in cooldown for opencode-zen only
        add_provider_proxy_cooldown(
            &conn,
            "opencode-zen",
            "proxy-1",
            std::time::Duration::from_mins(15),
        )
        .unwrap();

        // opencode-zen is in cooldown, cline is NOT
        assert!(is_provider_proxy_in_cooldown(
            &conn,
            "opencode-zen",
            "proxy-1"
        ));
        assert!(!is_provider_proxy_in_cooldown(&conn, "cline", "proxy-1"));

        // Cascade delete on proxy removal
        conn.execute("DELETE FROM free_proxies WHERE id = 'proxy-1'", [])
            .unwrap();

        assert!(!is_provider_proxy_in_cooldown(
            &conn,
            "opencode-zen",
            "proxy-1"
        ));
    }
}
