use super::models::{FreeProxy, ProxySummary, ScrapedProxy};
pub use openproxy_db::free_proxies::*;
use rusqlite::Connection;

fn fetch_proxy_stats(
    conn: &Connection,
) -> crate::error::Result<(usize, usize, usize, usize, Option<u32>)> {
    let mut stmt = conn
        .prepare(
            "SELECT \
                COUNT(*), \
                SUM(CASE WHEN status = 'alive' THEN 1 ELSE 0 END), \
                SUM(CASE WHEN status = 'dead' THEN 1 ELSE 0 END), \
                SUM(CASE WHEN status = 'unknown' THEN 1 ELSE 0 END), \
                AVG(CASE WHEN status = 'alive' AND latency_ms IS NOT NULL THEN latency_ms ELSE NULL END) \
             FROM free_proxies",
        )
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;

    stmt.query_row([], |r| {
        let total: i64 = r.get(0)?;
        let alive: Option<i64> = r.get(1)?;
        let dead: Option<i64> = r.get(2)?;
        let unknown: Option<i64> = r.get(3)?;
        let avg_latency: Option<f64> = r.get(4)?;
        Ok((
            total as usize,
            alive.unwrap_or(0) as usize,
            dead.unwrap_or(0) as usize,
            unknown.unwrap_or(0) as usize,
            avg_latency.map(|l| l.round() as u32),
        ))
    })
    .map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(std::sync::Arc::new(e)),
    })
}

fn fetch_distinct_column(conn: &Connection, col: &str) -> crate::error::Result<Vec<String>> {
    let sql = format!(
        "SELECT DISTINCT {col} FROM free_proxies WHERE {col} IS NOT NULL AND {col} != '' ORDER BY {col} ASC"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;
    let rows = stmt
        .query_map([], |r| r.get(0))
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;
    Ok(rows.filter_map(std::result::Result::ok).collect())
}

pub fn get_proxy_summary(conn: &Connection) -> crate::error::Result<ProxySummary> {
    let (total, alive, dead, unknown, avg_latency_ms) = fetch_proxy_stats(conn)?;
    let sources = fetch_distinct_column(conn, "source")?;
    let protocols = fetch_distinct_column(conn, "type")?;

    Ok(ProxySummary {
        total,
        alive,
        dead,
        unknown,
        avg_latency_ms,
        sources,
        protocols,
    })
}

fn row_to_free_proxy(row: &rusqlite::Row<'_>) -> rusqlite::Result<FreeProxy> {
    Ok(FreeProxy {
        id: row.get(0)?,
        source: row.get(1)?,
        host: row.get(2)?,
        port: row.get(3)?,
        r#type: row.get(4)?,
        country_code: row.get(5)?,
        status: row.get(6)?,
        latency_ms: row.get(7)?,
        last_validated: row.get(8)?,
        username: row.get(9)?,
        password: row.get(10)?,
        priority: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
    })
}

pub fn list_proxies(
    conn: &Connection,
    source: Option<&str>,
    status: Option<&str>,
    protocol: Option<&str>,
    search: Option<&str>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> crate::error::Result<Vec<FreeProxy>> {
    let mut sql = "SELECT id, source, host, port, type, country_code, status, latency_ms, last_validated, username, password, priority, created_at, updated_at FROM free_proxies WHERE 1=1".to_string();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(src) = source
        && !src.trim().is_empty()
    {
        sql.push_str(" AND source = ?");
        params.push(Box::new(src.to_string()));
    }

    if let Some(st) = status
        && !st.trim().is_empty()
    {
        sql.push_str(" AND status = ?");
        params.push(Box::new(st.to_string()));
    }

    if let Some(proto) = protocol
        && !proto.trim().is_empty()
    {
        sql.push_str(" AND type = ?");
        params.push(Box::new(proto.to_string()));
    }

    if let Some(s) = search {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            sql.push_str(
                " AND (host LIKE ? OR source LIKE ? OR type LIKE ? OR country_code LIKE ?)",
            );
            let pattern = format!("%{trimmed}%");
            params.push(Box::new(pattern.clone()));
            params.push(Box::new(pattern.clone()));
            params.push(Box::new(pattern.clone()));
            params.push(Box::new(pattern));
        }
    }

    sql.push_str(" ORDER BY priority DESC, status = 'alive' DESC, latency_ms ASC, updated_at DESC");

    let lim = limit.unwrap_or(100).min(500);
    sql.push_str(" LIMIT ?");
    params.push(Box::new(lim as i64));

    if let Some(off) = offset {
        sql.push_str(" OFFSET ?");
        params.push(Box::new(off as i64));
    }

    let mut stmt = conn
        .prepare(&sql)
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;

    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(params.iter().map(std::convert::AsRef::as_ref)),
            row_to_free_proxy,
        )
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;

    rows.map(|r| {
        r.map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })
    })
    .collect::<std::result::Result<Vec<_>, crate::error::CoreError>>()
}

pub fn get_proxy(conn: &Connection, id: &str) -> crate::error::Result<Option<FreeProxy>> {
    use rusqlite::OptionalExtension;
    let mut stmt = conn
        .prepare("SELECT id, source, host, port, type, country_code, status, latency_ms, last_validated, username, password, priority, created_at, updated_at FROM free_proxies WHERE id = ?1")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;

    stmt.query_row(rusqlite::params![id], row_to_free_proxy)
        .optional()
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })
}

pub fn get_proxy_status_by_url(conn: &rusqlite::Connection, url: &str) -> Option<String> {
    let (_, host_port) = url.split_once("://")?;
    let (host, port_str) = host_port.split_once(':')?;
    let port: i64 = port_str.parse().ok()?;

    conn.query_row(
        "SELECT status FROM free_proxies WHERE host = ?1 AND port = ?2",
        rusqlite::params![host, port],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

pub fn add_custom_proxy(
    conn: &Connection,
    host: &str,
    port: u16,
    r#type: &str,
    country_code: Option<&str>,
    username: Option<&str>,
    password: Option<&str>,
) -> crate::error::Result<FreeProxy> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO free_proxies (id, source, host, port, type, country_code, status, latency_ms, last_validated, username, password, priority, created_at, updated_at) \
         VALUES (?1, 'custom', ?2, ?3, ?4, ?5, 'unknown', NULL, NULL, ?6, ?7, 0, ?8, ?9) \
         ON CONFLICT(host, port) DO UPDATE SET \
           source = 'custom', \
           type = excluded.type, \
           country_code = COALESCE(excluded.country_code, free_proxies.country_code), \
           username = excluded.username, \
           password = excluded.password, \
           updated_at = excluded.updated_at",
        rusqlite::params![id, host, port, r#type.to_lowercase(), country_code, username, password, now, now],
    )
    .map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(std::sync::Arc::new(e)),
    })?;

    let mut stmt = conn
        .prepare("SELECT id, source, host, port, type, country_code, status, latency_ms, last_validated, username, password, priority, created_at, updated_at FROM free_proxies WHERE host = ?1 AND port = ?2")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;

    let p = stmt
        .query_row(rusqlite::params![host, port], |row| {
            Ok(FreeProxy {
                id: row.get(0)?,
                source: row.get(1)?,
                host: row.get(2)?,
                port: row.get(3)?,
                r#type: row.get(4)?,
                country_code: row.get(5)?,
                status: row.get(6)?,
                latency_ms: row.get(7)?,
                last_validated: row.get(8)?,
                username: row.get(9)?,
                password: row.get(10)?,
                priority: row.get(11)?,
                created_at: row.get(12)?,
                updated_at: row.get(13)?,
            })
        })
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;

    Ok(p)
}

pub fn delete_proxy(conn: &Connection, id: &str) -> crate::error::Result<()> {
    conn.execute(
        "DELETE FROM free_proxies WHERE id = ?1",
        rusqlite::params![id],
    )
    .map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(std::sync::Arc::new(e)),
    })?;
    Ok(())
}

pub fn update_proxy_status(
    conn: &Connection,
    id: &str,
    status: &str,
    latency_ms: Option<i64>,
) -> crate::error::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE free_proxies SET status = ?1, latency_ms = ?2, last_validated = ?3, updated_at = ?4 WHERE id = ?5",
        rusqlite::params![status, latency_ms, now, now, id],
    )
    .map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(std::sync::Arc::new(e)),
    })?;
    Ok(())
}

pub fn get_or_assign_provider_proxy(
    conn: &Connection,
    provider_id: &crate::ids::ProviderId,
    account_id: Option<&crate::ids::AccountId>,
) -> crate::error::Result<Option<String>> {
    openproxy_db::free_proxies::get_or_assign_provider_proxy(conn, provider_id, account_id)
}

/// Unified async resolution of the active proxy for a provider / account.
///
/// If `use_proxies` is enabled for the provider, returns `Ok(Some(proxy_url))`.
/// If proxies are disabled, returns `Ok(None)`.
pub async fn resolve_active_proxy(
    db_pool: &std::sync::Arc<openproxy_db::DbPool>,
    provider_id: &crate::ids::ProviderId,
    account_id: Option<crate::ids::AccountId>,
) -> crate::error::Result<Option<String>> {
    let pool = std::sync::Arc::clone(db_pool);
    let pid = provider_id.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool
            .try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT)
            .ok_or_else(|| {
                crate::error::CoreError::Internal(
                    "timeout waiting for db writer lock during proxy resolution".into(),
                )
            })?;
        openproxy_db::free_proxies::get_or_assign_provider_proxy(&conn, &pid, account_id.as_ref())
    })
    .await
    .map_err(|e| {
        crate::error::CoreError::Internal(format!("proxy resolution task panicked: {e}"))
    })?
}

/// Reports a proxy failure for a provider/account, triggering cooldown, clearing binding,
/// and optionally marking the proxy dead (on connection errors).
/// Emits a system notification with `CODE_PROXY_FAILED`.
pub async fn report_proxy_failure(
    db_pool: &std::sync::Arc<openproxy_db::DbPool>,
    provider_id: &crate::ids::ProviderId,
    account_id: Option<crate::ids::AccountId>,
    is_connect_error: bool,
) -> crate::error::Result<bool> {
    let pool = std::sync::Arc::clone(db_pool);
    let pid = provider_id.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool
            .try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT)
            .ok_or_else(|| {
                crate::error::CoreError::Internal(
                    "timeout waiting for db writer lock during proxy failure reporting".into(),
                )
            })?;

        let Some(provider) = openproxy_db::providers::get(&conn, &pid)? else {
            return Ok(false);
        };
        if !provider.use_proxies {
            return Ok(false);
        }

        let is_per_account = provider.proxy_rotation_mode.as_ref() == "account";
        let bad_proxy_id = if is_per_account {
            account_id.and_then(|aid| {
                openproxy_db::accounts::get_current_proxy_id(&conn, aid).unwrap_or(None)
            })
        } else {
            provider
                .current_proxy_id
                .as_deref()
                .map(ToString::to_string)
        };

        let Some(bad_proxy) = bad_proxy_id else {
            return Ok(false);
        };

        if is_connect_error {
            let _ =
                openproxy_db::free_proxies::update_proxy_status(&conn, &bad_proxy, "dead", None);
        }

        let _ = openproxy_db::cooldowns::add_provider_proxy_cooldown(
            &conn,
            pid.as_str(),
            &bad_proxy,
            std::time::Duration::from_secs(900),
        );

        if is_per_account {
            if let Some(aid) = account_id {
                let _ = openproxy_db::accounts::clear_current_proxy_id(&conn, aid);
            }
        } else {
            let _ = openproxy_db::providers::update_current_proxy(&conn, &pid, None);
        }

        let payload = serde_json::json!({
            "code": crate::notifications::CODE_PROXY_FAILED,
            "message": format!(
                "Proxy {} failed for provider {} (connect_error={}), rotating...",
                bad_proxy, pid, is_connect_error
            ),
            "provider_id": pid.as_str(),
            "details": {
                "proxy_id": bad_proxy,
                "provider_id": pid.as_str(),
                "account_id": account_id.map(|a| a.0),
                "is_connect_error": is_connect_error,
            },
        });
        let _ = crate::notifications::insert_and_broadcast(
            &conn,
            crate::notifications::KIND_SYSTEM,
            &payload,
            None,
            Some(pid.as_str()),
        );

        Ok(true)
    })
    .await
    .map_err(|e| crate::error::CoreError::Internal(format!("proxy failure task panicked: {e}")))?
}

pub fn get_candidate_proxies_for_provider(
    conn: &Connection,
    provider_id: &crate::ids::ProviderId,
    limit: usize,
) -> crate::error::Result<Vec<(String, String)>> {
    openproxy_db::free_proxies::get_candidate_proxies_for_provider(conn, provider_id, limit)
}

pub fn upsert_scraped_proxies(
    conn: &mut Connection,
    proxies: &[ScrapedProxy],
) -> crate::error::Result<()> {
    if proxies.is_empty() {
        return Ok(());
    }

    openproxy_db::error::with_busy_retry("upsert_scraped_proxies", || {
        let now = chrono::Utc::now().to_rfc3339();
        let tx = conn
            .transaction()
            .map_err(openproxy_db::error::map_db_error)?;

        let on_conflict_suffix = "ON CONFLICT(host, port) DO UPDATE SET \
                   source = CASE WHEN free_proxies.source = 'custom' THEN 'custom' ELSE excluded.source END, \
                   type = excluded.type, \
                   country_code = COALESCE(excluded.country_code, free_proxies.country_code), \
                   username = excluded.username, \
                   password = excluded.password, \
                   priority = excluded.priority, \
                   updated_at = excluded.updated_at";

        openproxy_db::batch::batch_insert(
            &tx,
            "INSERT INTO",
            "free_proxies",
            &[
                "id",
                "source",
                "host",
                "port",
                "type",
                "country_code",
                "status",
                "latency_ms",
                "last_validated",
                "username",
                "password",
                "priority",
                "created_at",
                "updated_at",
            ],
            proxies,
            Some(on_conflict_suffix),
            |p, params| {
                let id = uuid::Uuid::new_v4().to_string();
                params.push(id.into());
                params.push(p.source.clone().into());
                params.push(p.host.clone().into());
                params.push(p.port.into());
                params.push(p.r#type.clone().into());
                match &p.country_code {
                    Some(cc) => params.push(cc.to_owned().into()),
                    None => params.push(rusqlite::types::Value::Null),
                }
                params.push("unknown".to_string().into());
                params.push(rusqlite::types::Value::Null);
                params.push(rusqlite::types::Value::Null);
                match &p.username {
                    Some(u) => params.push(u.to_owned().into()),
                    None => params.push(rusqlite::types::Value::Null),
                }
                match &p.password {
                    Some(pass) => params.push(pass.to_owned().into()),
                    None => params.push(rusqlite::types::Value::Null),
                }
                params.push(p.priority.into());
                params.push(now.clone().into());
                params.push(now.clone().into());
            },
        )
        .map_err(openproxy_db::error::map_db_error)?;

        tx.commit().map_err(openproxy_db::error::map_db_error)?;
        Ok(())
    })
}
