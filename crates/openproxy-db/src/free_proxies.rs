//! Free proxy database operations and DAOs.

use openproxy_types::{AccountId, ProviderId, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// Update status, latency, and timestamps of a proxy.
pub fn update_proxy_status(
    conn: &Connection,
    proxy_id: &str,
    status: &str,
    _error_msg: Option<&str>,
) -> Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE free_proxies SET status = ?1, latency_ms = ?2, last_validated = ?3, updated_at = ?4 WHERE id = ?5",
        params![status, None::<i64>, now, now, proxy_id],
    )
    .map(|_| ())
    .map_err(crate::error::map_db_error)
}

/// Format a full proxy URL with optional authentication.
pub fn format_proxy_url(
    proto: &str,
    host: &str,
    port: i64,
    username: Option<&str>,
    password: Option<&str>,
) -> String {
    if let (Some(u), Some(p)) = (username, password) {
        format!("{}://{}:{}@{}:{}", proto.to_lowercase(), u, p, host, port)
    } else {
        format!("{}://{}:{}", proto.to_lowercase(), host, port)
    }
}

crate::def_table_select!(
    free_proxy_select,
    "free_proxies",
    "id, host, port, type, username, password"
);

crate::def_table_select!(
    alive_proxy_select,
    "free_proxies",
    "host, port, type, username, password"
);

crate::def_table_select!(proxy_status_select, "free_proxies", "status");

type ProxyRow = (String, String, i64, String, Option<String>, Option<String>);

#[inline]
fn map_proxy_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProxyRow> {
    crate::map_row_tuple!(row => (0, 1, 2, 3, 4, 5))
}

fn check_proxy_enabled(
    conn: &Connection,
    provider_id: &ProviderId,
) -> Result<Option<openproxy_types::Provider>> {
    let Some(provider) = crate::providers::get(conn, provider_id)? else {
        return Ok(None);
    };
    if !provider.use_proxies {
        return Ok(None);
    }
    Ok(Some(provider))
}

fn resolve_current_proxy_id(
    conn: &Connection,
    provider: &openproxy_types::Provider,
    account_id: Option<&AccountId>,
    is_per_account: bool,
) -> Result<Option<String>> {
    if is_per_account {
        if let Some(acc_id) = account_id {
            conn.query_row(
                "SELECT current_proxy_id FROM accounts WHERE id = ?1",
                params![acc_id.0],
                |row| row.get(0),
            )
            .optional()
            .map_err(crate::error::map_db_error)?
            .flatten()
        } else {
            None
        }
    } else {
        provider.current_proxy_id.clone()
    }
    .pipe(Ok)
}

// Extension helper trait to pipe values into Ok
trait PipeExt: Sized {
    fn pipe<F, R>(self, f: F) -> R
    where
        F: FnOnce(Self) -> R,
    {
        f(self)
    }
}
impl<T> PipeExt for T {}

fn fetch_alive_proxy_url(
    conn: &Connection,
    provider_id: &ProviderId,
    proxy_id: &str,
) -> Result<Option<String>> {
    use crate::cooldowns::is_provider_proxy_in_cooldown;

    if is_provider_proxy_in_cooldown(conn, provider_id.as_str(), proxy_id) {
        return Ok(None);
    }

    let exists_and_alive = conn
        .query_row(
            alive_proxy_select!("WHERE id = ?1 AND status = 'alive'"),
            params![proxy_id],
            |row| {
                crate::map_row_tuple!(row => (
                    (0, String),
                    (1, i64),
                    (2, String),
                    (3, Option<String>),
                    (4, Option<String>),
                ))
            },
        )
        .optional()
        .map_err(crate::error::map_db_error)?;

    Ok(exists_and_alive.map(|(host, port, proto, username, password)| {
        format_proxy_url(
            &proto,
            &host,
            port,
            username.as_deref(),
            password.as_deref(),
        )
    }))
}

fn check_current_proxy(
    conn: &Connection,
    provider_id: &ProviderId,
    provider: &openproxy_types::Provider,
    account_id: Option<&AccountId>,
    is_per_account: bool,
) -> Result<Option<String>> {
    let current_proxy_id = resolve_current_proxy_id(conn, provider, account_id, is_per_account)?;
    if let Some(proxy_id) = current_proxy_id {
        fetch_alive_proxy_url(conn, provider_id, &proxy_id)
    } else {
        Ok(None)
    }
}

fn fetch_accounts_in_use_proxies(
    conn: &Connection,
    provider_id: &ProviderId,
    account_id: Option<&AccountId>,
) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn
        .prepare("SELECT current_proxy_id FROM accounts WHERE provider_id = ?1 AND current_proxy_id IS NOT NULL AND id != ?2")
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map(
            params![provider_id.as_str(), account_id.map_or(0, |id| id.0)],
            |row| row.get::<_, String>(0),
        )
        .map_err(crate::error::map_db_error)?;
    let mut set = std::collections::HashSet::new();
    for r in rows.flatten() {
        set.insert(r);
    }
    Ok(set)
}

type ProxyTuple = (String, String, i64, String, Option<String>, Option<String>);

fn select_available_proxy(
    conn: &Connection,
    provider_id: &ProviderId,
    in_use_by_others: &std::collections::HashSet<String>,
) -> Result<Option<ProxyTuple>> {
    use crate::cooldowns::is_provider_proxy_in_cooldown;

    let mut stmt = conn
        .prepare(free_proxy_select!(
            "WHERE status = 'alive' ORDER BY priority DESC, latency_ms ASC, random() LIMIT 2000"
        ))
        .map_err(crate::error::map_db_error)?;

    let candidate_rows = stmt
        .query_map([], map_proxy_row)
        .map_err(crate::error::map_db_error)?;

    for item in candidate_rows.flatten() {
        if !is_provider_proxy_in_cooldown(conn, provider_id.as_str(), &item.0)
            && !in_use_by_others.contains(&item.0)
        {
            return Ok(Some(item));
        }
    }
    Ok(None)
}

fn save_assigned_proxy(
    conn: &Connection,
    provider_id: &ProviderId,
    account_id: Option<&AccountId>,
    is_per_account: bool,
    new_id: &str,
) -> Result<()> {
    if is_per_account {
        if let Some(acc_id) = account_id {
            conn.execute(
                "UPDATE accounts SET current_proxy_id = ?1 WHERE id = ?2",
                params![new_id, acc_id.0],
            )
            .map_err(crate::error::map_db_error)?;
        }
    } else {
        crate::providers::update_current_proxy(conn, provider_id, Some(new_id))?;
    }
    Ok(())
}

fn assign_new_proxy(
    conn: &Connection,
    provider_id: &ProviderId,
    account_id: Option<&AccountId>,
    is_per_account: bool,
) -> Result<Option<String>> {
    let in_use = if is_per_account {
        fetch_accounts_in_use_proxies(conn, provider_id, account_id)?
    } else {
        std::collections::HashSet::new()
    };

    let Some((new_id, host, port, proto, username, password)) =
        select_available_proxy(conn, provider_id, &in_use)?
    else {
        return Err(openproxy_types::error::CoreError::Validation(format!(
            "use_proxies is enabled for provider '{provider_id}', but no alive proxies are available in pool"
        )));
    };

    save_assigned_proxy(conn, provider_id, account_id, is_per_account, &new_id)?;

    Ok(Some(format_proxy_url(
        &proto,
        &host,
        port,
        username.as_deref(),
        password.as_deref(),
    )))
}

/// Retrieve or assign an alive proxy for a provider or account.
pub fn get_or_assign_provider_proxy(
    conn: &Connection,
    provider_id: &ProviderId,
    account_id: Option<&AccountId>,
) -> Result<Option<String>> {
    let Some(provider) = check_proxy_enabled(conn, provider_id)? else {
        return Ok(None);
    };

    let is_per_account = provider.proxy_rotation_mode == "account";
    if let Some(url) = check_current_proxy(conn, provider_id, &provider, account_id, is_per_account)? {
        return Ok(Some(url));
    }

    assign_new_proxy(conn, provider_id, account_id, is_per_account)
}

/// Retrieve up to `limit` alive candidate proxies not in cooldown for provider.
pub fn get_candidate_proxies_for_provider(
    conn: &Connection,
    provider_id: &ProviderId,
    limit: usize,
) -> Result<Vec<(String, String)>> {
    use crate::cooldowns::is_provider_proxy_in_cooldown;

    let mut stmt = conn
        .prepare(free_proxy_select!(
            "WHERE status = 'alive' ORDER BY priority DESC, latency_ms ASC, random() LIMIT 2000"
        ))
        .map_err(crate::error::map_db_error)?;

    let rows = stmt
        .query_map([], map_proxy_row)
        .map_err(crate::error::map_db_error)?;

    let candidates = rows
        .flatten()
        .filter(|item| !is_provider_proxy_in_cooldown(conn, provider_id.as_str(), &item.0))
        .take(limit)
        .map(|item| {
            let url = format_proxy_url(&item.3, &item.1, item.2, item.4.as_deref(), item.5.as_deref());
            (item.0, url)
        })
        .collect();

    Ok(candidates)
}

/// Lookup proxy status by url string.
pub fn get_proxy_status_by_url(conn: &Connection, url: &str) -> Option<String> {
    let (_, host_port) = url.split_once("://")?;
    let (host, port_str) = host_port.split_once(':')?;
    let port: i64 = port_str.parse().ok()?;
    conn.query_row(
        proxy_status_select!("WHERE host = ?1 AND port = ?2"),
        params![host, port],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

fn apply_source_priorities(tx: &rusqlite::Transaction, ids: &[String]) -> Result<()> {
    let mut stmt = tx
        .prepare_cached("UPDATE proxy_sources SET priority = ?1 WHERE id = ?2")
        .map_err(crate::error::map_db_error)?;

    let n = ids.len();
    for (i, id) in ids.iter().enumerate() {
        let p = ((n - i) * 10) as i32;
        stmt.execute(params![p, id])
            .map_err(crate::error::map_db_error)?;
    }
    Ok(())
}

/// Reorder proxy source priorities in batch.
pub fn reorder_proxy_sources(conn: &Connection, ids: &[String]) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .map_err(crate::error::map_db_error)?;

    apply_source_priorities(&tx, ids)?;
    tx.commit().map_err(crate::error::map_db_error)?;

    Ok(())
}

/// Prune dead proxies from free_proxies table.
pub fn prune_dead_proxies(conn: &Connection) -> Result<usize> {
    conn.execute("DELETE FROM free_proxies WHERE status = 'dead'", [])
        .map_err(crate::error::map_db_error)
}

/// Delete a proxy source by ID.
pub fn delete_proxy_source(conn: &Connection, id: &str) -> Result<bool> {
    let count = conn
        .execute("DELETE FROM proxy_sources WHERE id = ?1", params![id])
        .map_err(crate::error::map_db_error)?;
    Ok(count > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_proxy_url() {
        assert_eq!(
            format_proxy_url("HTTP", "127.0.0.1", 8080, None, None),
            "http://127.0.0.1:8080"
        );
        assert_eq!(
            format_proxy_url("SOCKS5", "proxy.org", 1080, Some("user"), Some("pass")),
            "socks5://user:pass@proxy.org:1080"
        );
    }

    fn insert_test_source(conn: &Connection, id: &str, name: &str, url: &str) -> Result<()> {
        conn.execute(
            "INSERT INTO proxy_sources (id, name, url, priority) VALUES (?1, ?2, ?3, 0)",
            params![id, name, url],
        )
        .map_err(crate::error::map_db_error)?;
        Ok(())
    }

    fn get_source_priority(conn: &Connection, id: &str) -> Result<i32> {
        conn.query_row(
            "SELECT priority FROM proxy_sources WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .map_err(crate::error::map_db_error)
    }

    #[test]
    fn test_reorder_proxy_sources() -> Result<()> {
        let mut conn = Connection::open_in_memory().map_err(crate::error::map_db_error)?;
        crate::migrations::run(&mut conn).map_err(crate::error::map_db_error)?;

        insert_test_source(&conn, "src1", "Source 1", "http://a.com")?;
        insert_test_source(&conn, "src2", "Source 2", "http://b.com")?;

        let ids = vec!["src1".to_string(), "src2".to_string()];
        reorder_proxy_sources(&conn, &ids)?;

        assert_eq!(get_source_priority(&conn, "src1")?, 20);
        assert_eq!(get_source_priority(&conn, "src2")?, 10);

        Ok(())
    }
}
