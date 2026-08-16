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

/// Retrieve or assign an alive proxy for a provider or account.
pub fn get_or_assign_provider_proxy(
    conn: &Connection,
    provider_id: &ProviderId,
    account_id: Option<&AccountId>,
) -> Result<Option<String>> {
    use crate::cooldowns::is_provider_proxy_in_cooldown;

    let Some(provider) = crate::providers::get(conn, provider_id)? else {
        return Ok(None);
    };

    if !provider.use_proxies {
        return Ok(None);
    }

    let is_per_account = provider.proxy_rotation_mode == "account";
    let current_proxy_id: Option<String> = if is_per_account {
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
        provider.current_proxy_id
    };

    if let Some(ref proxy_id) = current_proxy_id
        && !is_provider_proxy_in_cooldown(conn, provider_id.as_str(), proxy_id)
    {
        let exists_and_alive = conn
            .query_row(
                "SELECT host, port, type, username, password FROM free_proxies WHERE id = ?1 AND status = 'alive'",
                params![proxy_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                    ))
                },
            )
            .optional()
            .map_err(crate::error::map_db_error)?;

        if let Some((host, port, proto, username, password)) = exists_and_alive {
            if let (Some(u), Some(p)) = (username, password) {
                return Ok(Some(format!(
                    "{}://{}:{}@{}:{}",
                    proto.to_lowercase(),
                    u,
                    p,
                    host,
                    port
                )));
            }
            return Ok(Some(format!(
                "{}://{}:{}",
                proto.to_lowercase(),
                host,
                port
            )));
        }
    }

    let mut in_use_by_others = std::collections::HashSet::new();
    if is_per_account {
        let mut stmt = conn
            .prepare("SELECT current_proxy_id FROM accounts WHERE provider_id = ?1 AND current_proxy_id IS NOT NULL AND id != ?2")
            .map_err(crate::error::map_db_error)?;
        let rows = stmt
            .query_map(
                params![
                    provider_id.as_str(),
                    account_id.map_or(0, |id| id.0)
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(crate::error::map_db_error)?;
        for r in rows.flatten() {
            in_use_by_others.insert(r);
        }
    }

    let mut stmt = conn
        .prepare("SELECT id, host, port, type, username, password FROM free_proxies WHERE status = 'alive' ORDER BY priority DESC, latency_ms ASC, random() LIMIT 2000")
        .map_err(crate::error::map_db_error)?;

    let candidate_rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })
        .map_err(crate::error::map_db_error)?;

    let mut selected_proxy = None;
    let mut fallback_proxy = None;

    for item in candidate_rows.flatten() {
        if fallback_proxy.is_none() {
            fallback_proxy = Some(item.clone());
        }
        if !is_provider_proxy_in_cooldown(conn, provider_id.as_str(), &item.0)
            && !in_use_by_others.contains(&item.0)
        {
            selected_proxy = Some(item);
            break;
        }
    }

    let new_proxy = selected_proxy.or(fallback_proxy);

    if let Some((new_id, host, port, proto, username, password)) = new_proxy {
        if is_per_account {
            if let Some(acc_id) = account_id {
                conn.execute(
                    "UPDATE accounts SET current_proxy_id = ?1 WHERE id = ?2",
                    params![new_id, acc_id.0],
                )
                .map_err(crate::error::map_db_error)?;
            }
        } else {
            crate::providers::update_current_proxy(conn, provider_id, Some(&new_id))?;
        }

        if let (Some(u), Some(p)) = (username, password) {
            return Ok(Some(format!(
                "{}://{}:{}@{}:{}",
                proto.to_lowercase(),
                u,
                p,
                host,
                port
            )));
        }
        return Ok(Some(format!(
            "{}://{}:{}",
            proto.to_lowercase(),
            host,
            port
        )));
    }

    Err(openproxy_types::error::CoreError::Validation(format!(
        "use_proxies is enabled for provider '{provider_id}', but no alive proxies are available in pool"
    )))
}

/// Lookup proxy status by url string.
pub fn get_proxy_status_by_url(conn: &Connection, url: &str) -> Option<String> {
    let (_, host_port) = url.split_once("://")?;
    let (host, port_str) = host_port.split_once(':')?;
    let port: i64 = port_str.parse().ok()?;
    conn.query_row(
        "SELECT status FROM free_proxies WHERE host = ?1 AND port = ?2",
        params![host, port],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Reorder proxy source priorities in batch.
pub fn reorder_proxy_sources(conn: &Connection, ids: &[String]) -> Result<()> {
    let tx = conn
        .unchecked_transaction()
        .map_err(crate::error::map_db_error)?;

    let n = ids.len();
    for (i, id) in ids.iter().enumerate() {
        let p = ((n - i) * 10) as i32;
        tx.execute(
            "UPDATE proxy_sources SET priority = ?1 WHERE id = ?2",
            params![p, id],
        )
        .map_err(crate::error::map_db_error)?;
    }
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
