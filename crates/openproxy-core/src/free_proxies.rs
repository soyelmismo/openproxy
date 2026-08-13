//! staging table of free scraped/custom proxies + validation.

use openproxy_db::DbPool;
use rusqlite::Connection;
use std::sync::Arc;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FreeProxy {
    pub id: String,
    pub source: String,
    pub host: String,
    pub port: u16,
    pub r#type: String,
    pub country_code: Option<String>,
    pub status: String,
    pub latency_ms: Option<i64>,
    pub last_validated: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub priority: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ScrapedProxy {
    pub source: String,
    pub host: String,
    pub port: u16,
    pub r#type: String,
    pub country_code: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub priority: i32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProxySource {
    pub id: String,
    pub name: String,
    pub url: String,
    pub priority: i32,
    pub active: bool,
    pub is_builtin: bool,
    pub proxies_total: i64,
    pub proxies_alive: i64,
    pub proxies_dead: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CreateProxySourceInput {
    pub name: String,
    pub url: String,
    pub priority: Option<i32>,
    pub active: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UpdateProxySourceInput {
    pub name: Option<String>,
    pub url: Option<String>,
    pub priority: Option<i32>,
    pub active: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncSummary {
    pub fetched: usize,
    pub added: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProxySummary {
    pub total: usize,
    pub alive: usize,
    pub dead: usize,
    pub unknown: usize,
    pub avg_latency_ms: Option<u32>,
    pub sources: Vec<String>,
    pub protocols: Vec<String>,
}

pub fn get_proxy_summary(conn: &Connection) -> crate::error::Result<ProxySummary> {
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
            source: Some(Box::new(e)),
        })?;

    let row = stmt
        .query_row([], |r| {
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
            source: Some(Box::new(e)),
        })?;

    let mut sources_stmt = conn
        .prepare("SELECT DISTINCT source FROM free_proxies WHERE source IS NOT NULL AND source != '' ORDER BY source ASC")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;
    let sources_rows = sources_stmt.query_map([], |r| r.get(0)).map_err(|e| {
        crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        }
    })?;
    let sources: Vec<String> = sources_rows.filter_map(|r| r.ok()).collect();

    let mut proto_stmt = conn
        .prepare("SELECT DISTINCT type FROM free_proxies WHERE type IS NOT NULL AND type != '' ORDER BY type ASC")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;
    let proto_rows =
        proto_stmt
            .query_map([], |r| r.get(0))
            .map_err(|e| crate::error::CoreError::Database {
                message: e.to_string(),
                source: Some(Box::new(e)),
            })?;
    let protocols: Vec<String> = proto_rows.filter_map(|r| r.ok()).collect();

    Ok(ProxySummary {
        total: row.0,
        alive: row.1,
        dead: row.2,
        unknown: row.3,
        avg_latency_ms: row.4,
        sources,
        protocols,
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
            let pattern = format!("%{}%", trimmed);
            params.push(Box::new(pattern.to_owned()));
            params.push(Box::new(pattern.to_owned()));
            params.push(Box::new(pattern.to_owned()));
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
            source: Some(Box::new(e)),
        })?;

    let rows = stmt
        .query_map(
            rusqlite::params_from_iter(params.iter().map(|p| p.as_ref())),
            |row| {
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
            },
        )
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;

    let mut list = Vec::new();
    for r in rows {
        list.push(r.map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?);
    }
    Ok(list)
}

pub fn get_proxy(conn: &Connection, id: &str) -> crate::error::Result<Option<FreeProxy>> {
    let mut stmt = conn
        .prepare("SELECT id, source, host, port, type, country_code, status, latency_ms, last_validated, username, password, priority, created_at, updated_at FROM free_proxies WHERE id = ?1")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;

    let res = stmt.query_row(rusqlite::params![id], |row| {
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
    });

    match res {
        Ok(p) => Ok(Some(p)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        }),
    }
}

pub fn get_proxy_status_by_url(conn: &rusqlite::Connection, url: &str) -> Option<String> {
    let parts: Vec<&str> = url.split("://").collect();
    if parts.len() != 2 {
        return None;
    }
    let host_port = parts[1];
    let host_port_parts: Vec<&str> = host_port.split(':').collect();
    if host_port_parts.len() != 2 {
        return None;
    }
    let host = host_port_parts[0];
    let port: i64 = host_port_parts[1].parse().ok()?;

    conn.query_row(
        "SELECT status FROM free_proxies WHERE host = ?1 AND port = ?2",
        rusqlite::params![host, port],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

pub fn add_custom_proxy(
    conn: &Connection,
    host: String,
    port: u16,
    r#type: String,
    country_code: Option<String>,
    username: Option<String>,
    password: Option<String>,
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
        source: Some(Box::new(e)),
    })?;

    let mut stmt = conn
        .prepare("SELECT id, source, host, port, type, country_code, status, latency_ms, last_validated, username, password, priority, created_at, updated_at FROM free_proxies WHERE host = ?1 AND port = ?2")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
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
            source: Some(Box::new(e)),
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
        source: Some(Box::new(e)),
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
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

pub fn get_or_assign_provider_proxy(
    conn: &Connection,
    provider_id: &crate::ids::ProviderId,
    account_id: Option<&crate::ids::AccountId>,
) -> crate::error::Result<Option<String>> {
    use openproxy_db::cooldowns::is_provider_proxy_in_cooldown;
    use rusqlite::OptionalExtension;

    // 1. Fetch provider details
    let provider = match crate::providers::get(conn, provider_id)? {
        Some(p) => p,
        None => return Ok(None),
    };

    if !provider.use_proxies {
        return Ok(None);
    }

    let is_per_account = provider.proxy_rotation_mode == "account";
    let (current_proxy_id, _account) = if is_per_account {
        if let Some(acc_id) = account_id {
            // Need a dummy master key just to get the account, although we only care about current_proxy_id
            // Wait, we don't need master key to query just the current_proxy_id!
            let acc_proxy_id: Option<String> = conn
                .query_row(
                    "SELECT current_proxy_id FROM accounts WHERE id = ?1",
                    rusqlite::params![acc_id.0],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| crate::error::CoreError::Database {
                    message: e.to_string(),
                    source: Some(Box::new(e)),
                })?
                .flatten();
            (acc_proxy_id, Some(*acc_id))
        } else {
            (None, None)
        }
    } else {
        (provider.current_proxy_id.to_owned(), None)
    };

    // 2. If current_proxy_id is set, verify it is still alive/valid and NOT in cooldown for this provider
    if let Some(ref proxy_id) = current_proxy_id
        && !is_provider_proxy_in_cooldown(provider_id.as_str(), proxy_id)
    {
        let exists_and_alive = conn
            .query_row(
                "SELECT host, port, type, username, password FROM free_proxies WHERE id = ?1 AND status = 'alive'",
                rusqlite::params![proxy_id],
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
            .map_err(|e| crate::error::CoreError::Database {
                message: format!("query current proxy: {}", e),
                source: Some(Box::new(e)),
            })?;

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
            } else {
                return Ok(Some(format!(
                    "{}://{}:{}",
                    proto.to_lowercase(),
                    host,
                    port
                )));
            }
        }
    }

    // Find in-use proxies by other accounts of this provider, to avoid them
    let mut in_use_by_others = std::collections::HashSet::new();
    if is_per_account {
        let mut stmt = conn
            .prepare("SELECT current_proxy_id FROM accounts WHERE provider_id = ?1 AND current_proxy_id IS NOT NULL AND id != ?2")
            .map_err(|e| crate::error::CoreError::Database { message: e.to_string(), source: Some(Box::new(e)) })?;
        let rows = stmt
            .query_map(
                rusqlite::params![provider_id.as_str(), account_id.map(|id| id.0).unwrap_or(0)],
                |row| row.get::<_, String>(0),
            )
            .map_err(|e| crate::error::CoreError::Database {
                message: e.to_string(),
                source: Some(Box::new(e)),
            })?;
        for r in rows.flatten() {
            in_use_by_others.insert(r);
        }
    }

    // 3. Select a new one from the alive pool
    let mut stmt = conn
        .prepare("SELECT id, host, port, type, username, password FROM free_proxies WHERE status = 'alive' ORDER BY priority DESC, latency_ms ASC, random() LIMIT 2000")
        .map_err(|e| crate::error::CoreError::Database {
            message: format!("prepare query new proxy: {}", e),
            source: Some(Box::new(e)),
        })?;

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
        .map_err(|e| crate::error::CoreError::Database {
            message: format!("query new proxy candidates: {}", e),
            source: Some(Box::new(e)),
        })?;

    let mut selected_proxy = None;
    let mut fallback_proxy = None;

    for item in candidate_rows.flatten() {
        if !is_provider_proxy_in_cooldown(provider_id.as_str(), &item.0)
            && !in_use_by_others.contains(&item.0)
        {
            selected_proxy = Some(item);
            break;
        } else if fallback_proxy.is_none() {
            fallback_proxy = Some(item);
        }
    }

    let new_proxy = selected_proxy.or(fallback_proxy);

    if let Some((new_id, host, port, proto, username, password)) = new_proxy {
        if is_per_account {
            if let Some(acc_id) = account_id {
                crate::accounts::update_current_proxy(conn, *acc_id, Some(&new_id))?;
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
        } else {
            return Ok(Some(format!(
                "{}://{}:{}",
                proto.to_lowercase(),
                host,
                port
            )));
        }
    }

    Err(crate::error::CoreError::Validation(format!(
        "use_proxies is enabled for provider '{}', but no alive proxies are available in pool",
        provider_id
    )))
}

pub fn upsert_scraped_proxies(
    conn: &mut Connection,
    proxies: &[ScrapedProxy],
) -> crate::error::Result<()> {
    if proxies.is_empty() {
        return Ok(());
    }

    let now = chrono::Utc::now().to_rfc3339();
    let tx = conn
        .transaction()
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;

    for chunk in proxies.chunks(100) {
        let mut sql = String::from(
            "INSERT INTO free_proxies (id, source, host, port, type, country_code, status, latency_ms, last_validated, username, password, priority, created_at, updated_at) VALUES ",
        );
        let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(chunk.len() * 11);

        for (i, p) in chunk.iter().enumerate() {
            if i > 0 {
                sql.push_str(", ");
            }
            let base = i * 11;
            sql.push_str(&format!(
                "(?{}, ?{}, ?{}, ?{}, ?{}, ?{}, 'unknown', NULL, NULL, ?{}, ?{}, ?{}, ?{}, ?{})",
                base + 1,
                base + 2,
                base + 3,
                base + 4,
                base + 5,
                base + 6,
                base + 7,
                base + 8,
                base + 9,
                base + 10,
                base + 11
            ));

            let id = uuid::Uuid::new_v4().to_string();
            params.push(id.into());
            params.push(p.source.to_owned().into());
            params.push(p.host.to_owned().into());
            params.push(p.port.into());
            params.push(p.r#type.to_owned().into());
            match &p.country_code {
                Some(cc) => params.push(cc.to_owned().into()),
                None => params.push(rusqlite::types::Value::Null),
            }
            match &p.username {
                Some(u) => params.push(u.to_owned().into()),
                None => params.push(rusqlite::types::Value::Null),
            }
            match &p.password {
                Some(pass) => params.push(pass.to_owned().into()),
                None => params.push(rusqlite::types::Value::Null),
            }
            params.push(p.priority.into());
            params.push(now.to_owned().into());
            params.push(now.to_owned().into());
        }

        sql.push_str(" ON CONFLICT(host, port) DO UPDATE SET \
               source = CASE WHEN free_proxies.source = 'custom' THEN 'custom' ELSE excluded.source END, \
               type = excluded.type, \
               country_code = COALESCE(excluded.country_code, free_proxies.country_code), \
               username = excluded.username, \
               password = excluded.password, \
               priority = excluded.priority, \
               updated_at = excluded.updated_at");

        tx.execute(&sql, rusqlite::params_from_iter(params))
            .map_err(|e| crate::error::CoreError::Database {
                message: e.to_string(),
                source: Some(Box::new(e)),
            })?;
    }

    tx.commit().map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(Box::new(e)),
    })?;
    Ok(())
}

// Scraper integrations
#[derive(serde::Deserialize)]
struct ProxiflyGeo {
    country: Option<String>,
}

#[derive(serde::Deserialize)]
struct ProxiflyItem {
    ip: String,
    port: u16,
    protocol: String,
    geolocation: Option<ProxiflyGeo>,
}

async fn sync_proxifly() -> crate::error::Result<Vec<ScrapedProxy>> {
    use openproxy_adapters::upstream::{TimeoutProfile, UpstreamClient, UpstreamRequest};
    let client = UpstreamClient::new();
    let req = UpstreamRequest::get("https://api.proxifly.dev/proxy?format=json&quantity=100");
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let res = client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("Proxifly HTTP error: {:?}", e)))?;

    if res.status != 200 {
        return Err(crate::error::CoreError::Internal(format!(
            "Proxifly HTTP status: {}",
            res.status
        )));
    }

    let body_bytes = res
        .collect()
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("Proxifly body error: {:?}", e)))?;
    let items: Vec<ProxiflyItem> = serde_json::from_slice(&body_bytes)
        .map_err(|e| crate::error::CoreError::Internal(format!("Proxifly JSON error: {}", e)))?;

    let list = items
        .into_iter()
        .map(|item| {
            let country_code = item
                .geolocation
                .and_then(|g| g.country)
                .filter(|c| !c.is_empty());
            ScrapedProxy {
                source: "proxifly".to_string(),
                host: item.ip,
                port: item.port,
                r#type: item.protocol.to_lowercase(),
                country_code,
                username: None,
                password: None,
                priority: 0,
            }
        })
        .collect();
    Ok(list)
}

async fn sync_github_lists() -> crate::error::Result<Vec<ScrapedProxy>> {
    use openproxy_adapters::upstream::{TimeoutProfile, UpstreamClient, UpstreamRequest};
    let client = UpstreamClient::new();
    let mut list = Vec::new();

    let sources = vec![
        (
            "iplocate",
            "https://raw.githubusercontent.com/iplocate/free-proxy-list/main/protocols/{}.txt",
            vec!["http", "https", "socks4", "socks5"],
        ),
        (
            "hideip",
            "https://raw.githubusercontent.com/zloi-user/hideip.me/main/{}.txt",
            vec!["http", "socks4", "socks5"],
        ),
        (
            "r00tee",
            "https://raw.githubusercontent.com/r00tee/Proxy-List/main/Socks5.txt",
            vec!["socks5"],
        ),
        (
            "hookzof",
            "https://raw.githubusercontent.com/hookzof/socks5_list/master/proxy.txt",
            vec!["socks5"],
        ),
        (
            "anonymouswork",
            "https://raw.githubusercontent.com/Anonym0usWork1221/Free-Proxies/main/proxy_files/https_proxies.txt",
            vec!["https"],
        ),
        (
            "komutan234",
            "https://raw.githubusercontent.com/komutan234/Proxy-List-Free/main/proxies/http.txt",
            vec!["http"],
        ),
        (
            "yuceltoluyag",
            "https://raw.githubusercontent.com/yuceltoluyag/GoodProxy/main/raw.txt",
            vec!["http"],
        ),
    ];

    for (src_name, url_template, protocols) in sources {
        for proto in protocols {
            let url = url_template.replace("{}", proto);
            let req = UpstreamRequest::get(&url);
            let cancel = openproxy_adapters::upstream::CancellationToken::new();
            let res = match client
                .call(req, TimeoutProfile::ModelDiscovery, cancel)
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("{} fetch error for {}: {:?}", src_name, proto, e);
                    continue;
                }
            };
            if res.status != 200 {
                tracing::warn!("{} status error for {}: {}", src_name, proto, res.status);
                continue;
            }
            let body_bytes = match res.collect().await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!("{} body error for {}: {:?}", src_name, proto, e);
                    continue;
                }
            };
            let text = String::from_utf8_lossy(&body_bytes);
            for line in text.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }
                if let Some(pos) = trimmed.rfind(':') {
                    let host = trimmed[..pos].trim().to_string();
                    if let Ok(port) = trimmed[pos + 1..].trim().parse::<u16>()
                        && !host.is_empty()
                        && port > 0
                    {
                        list.push(ScrapedProxy {
                            source: src_name.to_string(),
                            host,
                            port,
                            r#type: proto.to_string(),
                            country_code: None,
                            username: None,
                            password: None,
                            priority: 0,
                        });
                    }
                }
            }
        }
    }
    Ok(list)
}

#[derive(serde::Deserialize)]
struct OneProxyApiProxy {
    ip: String,
    port: u16,
    protocol: Option<String>,
    country_code: Option<String>,
}

#[derive(serde::Deserialize)]
struct OneProxyApiResponse {
    proxies: Option<Vec<OneProxyApiProxy>>,
}

async fn sync_oneproxy() -> crate::error::Result<Vec<ScrapedProxy>> {
    use openproxy_adapters::upstream::{TimeoutProfile, UpstreamClient, UpstreamRequest};
    let client = UpstreamClient::new();
    let req = UpstreamRequest::get("https://1proxy-api.aitradepulse.com/api/v1/proxies/advanced");
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let res = client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("1proxy HTTP error: {:?}", e)))?;

    if res.status != 200 {
        return Err(crate::error::CoreError::Internal(format!(
            "1proxy HTTP status: {}",
            res.status
        )));
    }

    let body_bytes = res
        .collect()
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("1proxy body error: {:?}", e)))?;
    let body: OneProxyApiResponse = serde_json::from_slice(&body_bytes)
        .map_err(|e| crate::error::CoreError::Internal(format!("1proxy JSON error: {}", e)))?;

    let proxies = body.proxies.unwrap_or_default();
    let list = proxies
        .into_iter()
        .map(|p| ScrapedProxy {
            source: "1proxy".to_string(),
            host: p.ip,
            port: p.port,
            r#type: p
                .protocol
                .unwrap_or_else(|| "http".to_string())
                .to_lowercase(),
            country_code: p.country_code.filter(|c| !c.is_empty()),
            username: None,
            password: None,
            priority: 0,
        })
        .collect();
    Ok(list)
}

#[derive(serde::Deserialize)]
struct ProxyScrapeCdnItem {
    ip: String,
    port: u16,
    protocol: String,
    country_code: Option<String>,
}

async fn sync_proxyscrape_cdn() -> crate::error::Result<Vec<ScrapedProxy>> {
    use openproxy_adapters::upstream::{TimeoutProfile, UpstreamClient, UpstreamRequest};
    let client = UpstreamClient::new();
    let req = UpstreamRequest::get(
        "https://cdn.jsdelivr.net/gh/proxyscrape/free-proxy-list@main/proxies/all/data.json",
    );
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let res = client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| {
            crate::error::CoreError::Internal(format!("ProxyScrape CDN HTTP error: {:?}", e))
        })?;

    if res.status != 200 {
        return Err(crate::error::CoreError::Internal(format!(
            "ProxyScrape CDN HTTP status: {}",
            res.status
        )));
    }

    let body_bytes = res.collect().await.map_err(|e| {
        crate::error::CoreError::Internal(format!("ProxyScrape CDN body error: {:?}", e))
    })?;
    let items: Vec<ProxyScrapeCdnItem> = serde_json::from_slice(&body_bytes).map_err(|e| {
        crate::error::CoreError::Internal(format!("ProxyScrape CDN JSON error: {}", e))
    })?;

    let list = items
        .into_iter()
        .map(|item| ScrapedProxy {
            source: "proxyscrape_cdn".to_string(),
            host: item.ip,
            port: item.port,
            r#type: item.protocol.to_lowercase(),
            country_code: item.country_code.filter(|c| !c.is_empty()),
            username: None,
            password: None,
            priority: 0,
        })
        .collect();
    Ok(list)
}

#[derive(serde::Deserialize)]
struct GeonodeItem {
    ip: String,
    port: String,
    protocols: Vec<String>,
    country: Option<String>,
}

#[derive(serde::Deserialize)]
struct GeonodeResponse {
    data: Vec<GeonodeItem>,
}

async fn sync_geonode() -> crate::error::Result<Vec<ScrapedProxy>> {
    use openproxy_adapters::upstream::{TimeoutProfile, UpstreamClient, UpstreamRequest};
    let client = UpstreamClient::new();
    let mut req = UpstreamRequest::get(
        "https://proxylist.geonode.com/api/proxy-list?limit=500&sort_by=lastChecked&sort_type=desc",
    );
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("Mozilla/5.0 (X11; Linux x86_64)"),
    );
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let res = client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("Geonode HTTP error: {:?}", e)))?;

    if res.status != 200 {
        return Err(crate::error::CoreError::Internal(format!(
            "Geonode HTTP status: {}",
            res.status
        )));
    }

    let body_bytes = res
        .collect()
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("Geonode body error: {:?}", e)))?;
    let body: GeonodeResponse = serde_json::from_slice(&body_bytes)
        .map_err(|e| crate::error::CoreError::Internal(format!("Geonode JSON error: {}", e)))?;

    let mut list = Vec::new();
    for item in body.data {
        if let Ok(port) = item.port.parse::<u16>() {
            let proto = item
                .protocols
                .into_iter()
                .next()
                .unwrap_or_else(|| "http".to_string());
            list.push(ScrapedProxy {
                source: "geonode".to_string(),
                host: item.ip,
                port,
                r#type: proto.to_lowercase(),
                country_code: item.country.filter(|c| !c.is_empty()),
                username: None,
                password: None,
                priority: 0,
            });
        }
    }
    Ok(list)
}

#[derive(serde::Deserialize)]
struct ClearProxyItem {
    ip: String,
    port: u16,
    protocol: String,
    country_code: Option<String>,
}

async fn sync_clearproxy() -> crate::error::Result<Vec<ScrapedProxy>> {
    use openproxy_adapters::upstream::{TimeoutProfile, UpstreamClient, UpstreamRequest};
    let client = UpstreamClient::new();
    let req = UpstreamRequest::get(
        "https://raw.githubusercontent.com/ClearProxy/checked-proxy-list/main/http/json/all.json",
    );
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let res = client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| {
            crate::error::CoreError::Internal(format!("ClearProxy HTTP error: {:?}", e))
        })?;

    if res.status != 200 {
        return Err(crate::error::CoreError::Internal(format!(
            "ClearProxy HTTP status: {}",
            res.status
        )));
    }

    let body_bytes = res.collect().await.map_err(|e| {
        crate::error::CoreError::Internal(format!("ClearProxy body error: {:?}", e))
    })?;
    let items: Vec<ClearProxyItem> = serde_json::from_slice(&body_bytes)
        .map_err(|e| crate::error::CoreError::Internal(format!("ClearProxy JSON error: {}", e)))?;

    let list = items
        .into_iter()
        .map(|item| ScrapedProxy {
            source: "clearproxy".to_string(),
            host: item.ip,
            port: item.port,
            r#type: item.protocol.to_lowercase(),
            country_code: item.country_code.filter(|c| !c.is_empty()),
            username: None,
            password: None,
            priority: 0,
        })
        .collect();
    Ok(list)
}

#[derive(serde::Deserialize)]
struct VakhovItem {
    ip: String,
    port: serde_json::Value,
    country_code: Option<String>,
}

async fn sync_vakhov() -> crate::error::Result<Vec<ScrapedProxy>> {
    use openproxy_adapters::upstream::{TimeoutProfile, UpstreamClient, UpstreamRequest};
    let client = UpstreamClient::new();
    let req = UpstreamRequest::get("https://vakhov.github.io/fresh-proxy-list/proxylist.json");
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let res = client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("Vakhov HTTP error: {:?}", e)))?;

    if res.status != 200 {
        return Err(crate::error::CoreError::Internal(format!(
            "Vakhov HTTP status: {}",
            res.status
        )));
    }

    let body_bytes = res
        .collect()
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("Vakhov body error: {:?}", e)))?;
    let items: Vec<VakhovItem> = serde_json::from_slice(&body_bytes)
        .map_err(|e| crate::error::CoreError::Internal(format!("Vakhov JSON error: {}", e)))?;

    let mut list = Vec::new();
    for item in items {
        let port_u16 = match item.port {
            serde_json::Value::Number(n) => n.as_u64().map(|v| v as u16),
            serde_json::Value::String(s) => s.parse::<u16>().ok(),
            _ => None,
        };
        if let Some(port) = port_u16 {
            list.push(ScrapedProxy {
                source: "vakhov".to_string(),
                host: item.ip,
                port,
                r#type: "http".to_string(),
                country_code: item.country_code.filter(|c| !c.is_empty()),
                username: None,
                password: None,
                priority: 0,
            });
        }
    }
    Ok(list)
}

#[derive(serde::Deserialize)]
struct GProxyNetItem {
    proxy: String,
    protocol: Option<String>,
    country: Option<String>,
}

async fn sync_gproxynet() -> crate::error::Result<Vec<ScrapedProxy>> {
    use openproxy_adapters::upstream::{TimeoutProfile, UpstreamClient, UpstreamRequest};
    let client = UpstreamClient::new();
    let req = UpstreamRequest::get(
        "https://raw.githubusercontent.com/gproxynet/free-proxy-list/main/proxies.json",
    );
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let res = client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("GProxyNet HTTP error: {:?}", e)))?;

    if res.status != 200 {
        return Err(crate::error::CoreError::Internal(format!(
            "GProxyNet HTTP status: {}",
            res.status
        )));
    }

    let body_bytes = res
        .collect()
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("GProxyNet body error: {:?}", e)))?;
    let items: Vec<GProxyNetItem> = serde_json::from_slice(&body_bytes)
        .map_err(|e| crate::error::CoreError::Internal(format!("GProxyNet JSON error: {}", e)))?;

    let mut list = Vec::new();
    for item in items {
        if let Some(pos) = item.proxy.rfind(':') {
            let host = item.proxy[..pos].trim().to_string();
            if let Ok(port) = item.proxy[pos + 1..].trim().parse::<u16>() {
                let proto = item.protocol.unwrap_or_else(|| "http".to_string());
                list.push(ScrapedProxy {
                    source: "gproxynet".to_string(),
                    host,
                    port,
                    r#type: proto.to_lowercase(),
                    country_code: item.country.filter(|c| !c.is_empty()),
                    username: None,
                    password: None,
                    priority: 0,
                });
            }
        }
    }
    Ok(list)
}

pub fn list_proxy_sources(conn: &Connection) -> crate::error::Result<Vec<ProxySource>> {
    let mut stmt = conn
        .prepare("SELECT id, name, url, priority, active, is_builtin, created_at, updated_at FROM proxy_sources ORDER BY priority DESC, name ASC")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;

    let mut stats_stmt = conn
        .prepare("SELECT source, status, COUNT(*) FROM free_proxies GROUP BY source, status")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;
    let mut stats = std::collections::HashMap::new();
    let stats_rows = stats_stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;
    for row in stats_rows.flatten() {
        stats
            .entry(row.0)
            .or_insert_with(Vec::new)
            .push((row.1, row.2));
    }

    let rows = stmt
        .query_map([], |row| {
            Ok(ProxySource {
                id: row.get(0)?,
                name: row.get(1)?,
                url: row.get(2)?,
                priority: row.get(3)?,
                active: row.get::<_, i32>(4)? != 0,
                is_builtin: row.get::<_, i32>(5)? != 0,
                proxies_total: 0,
                proxies_alive: 0,
                proxies_dead: 0,
                created_at: row.get(6)?,
                updated_at: row.get(7)?,
            })
        })
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;

    let mut list = Vec::new();
    for row_res in rows {
        let mut r = row_res.map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;

        let sources = if r.is_builtin {
            match r.id.as_str() {
                "builtin_proxifly" => vec!["proxifly"],
                "builtin_github" => vec![
                    "iplocate",
                    "hideip",
                    "r00tee",
                    "hookzof",
                    "anonymouswork",
                    "komutan234",
                    "yuceltoluyag",
                ],
                "builtin_oneproxy" => vec!["1proxy"],
                "builtin_proxyscrape" => vec!["proxyscrape_cdn"],
                "builtin_geonode" => vec!["geonode"],
                "builtin_clearproxy" => vec!["clearproxy"],
                "builtin_vakhov" => vec!["vakhov"],
                "builtin_gproxynet" => vec!["gproxynet"],
                _ => vec![r.name.as_str()],
            }
        } else {
            vec![r.name.as_str()]
        };

        for s in sources {
            if let Some(st) = stats.get(s) {
                for (status, count) in st {
                    r.proxies_total += count;
                    if status == "alive" {
                        r.proxies_alive += count;
                    } else if status == "dead" {
                        r.proxies_dead += count;
                    }
                }
            }
        }

        list.push(r);
    }
    Ok(list)
}

pub fn get_proxy_source(conn: &Connection, id: &str) -> crate::error::Result<Option<ProxySource>> {
    let mut stmt = conn
        .prepare("SELECT id, name, url, priority, active, is_builtin, created_at, updated_at FROM proxy_sources WHERE id = ?1")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;

    let res = stmt.query_row(rusqlite::params![id], |row| {
        Ok(ProxySource {
            id: row.get(0)?,
            name: row.get(1)?,
            url: row.get(2)?,
            priority: row.get(3)?,
            active: row.get::<_, i32>(4)? != 0,
            is_builtin: row.get::<_, i32>(5)? != 0,
            proxies_total: 0,
            proxies_alive: 0,
            proxies_dead: 0,
            created_at: row.get(6)?,
            updated_at: row.get(7)?,
        })
    });

    match res {
        Ok(mut source) => {
            let mut stats_stmt = conn
                .prepare(
                    "SELECT source, status, COUNT(*) FROM free_proxies GROUP BY source, status",
                )
                .map_err(|e| crate::error::CoreError::Database {
                    message: e.to_string(),
                    source: Some(Box::new(e)),
                })?;
            let mut stats = std::collections::HashMap::new();
            let stats_rows = stats_stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
                .map_err(|e| crate::error::CoreError::Database {
                    message: e.to_string(),
                    source: Some(Box::new(e)),
                })?;
            for row in stats_rows.flatten() {
                stats
                    .entry(row.0)
                    .or_insert_with(Vec::new)
                    .push((row.1, row.2));
            }

            let sources = if source.is_builtin {
                match source.id.as_str() {
                    "builtin_proxifly" => vec!["proxifly"],
                    "builtin_github" => vec![
                        "iplocate",
                        "hideip",
                        "r00tee",
                        "hookzof",
                        "anonymouswork",
                        "komutan234",
                        "yuceltoluyag",
                    ],
                    "builtin_oneproxy" => vec!["1proxy"],
                    "builtin_proxyscrape" => vec!["proxyscrape_cdn"],
                    "builtin_geonode" => vec!["geonode"],
                    "builtin_clearproxy" => vec!["clearproxy"],
                    "builtin_vakhov" => vec!["vakhov"],
                    "builtin_gproxynet" => vec!["gproxynet"],
                    _ => vec![source.name.as_str()],
                }
            } else {
                vec![source.name.as_str()]
            };

            for s in sources {
                if let Some(st) = stats.get(s) {
                    for (status, count) in st {
                        source.proxies_total += count;
                        if status == "alive" {
                            source.proxies_alive += count;
                        } else if status == "dead" {
                            source.proxies_dead += count;
                        }
                    }
                }
            }

            Ok(Some(source))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        }),
    }
}

pub fn create_proxy_source(
    conn: &Connection,
    input: CreateProxySourceInput,
) -> crate::error::Result<ProxySource> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let priority = input.priority.unwrap_or(0);

    conn.execute(
        "INSERT INTO proxy_sources (id, name, url, priority, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![id, input.name, input.url, priority, now, now],
    )
    .map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(Box::new(e)),
    })?;

    get_proxy_source(conn, &id)?.ok_or_else(|| crate::error::CoreError::NotFound {
        what: "proxy_source".to_string(),
        id,
    })
}

pub fn update_proxy_source(
    conn: &Connection,
    id: &str,
    input: UpdateProxySourceInput,
) -> crate::error::Result<ProxySource> {
    let existing =
        get_proxy_source(conn, id)?.ok_or_else(|| crate::error::CoreError::NotFound {
            what: "proxy_source".to_string(),
            id: id.to_string(),
        })?;

    let name = input.name.unwrap_or(existing.name);
    let url = input.url.unwrap_or(existing.url);
    let priority = input.priority.unwrap_or(existing.priority);
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "UPDATE proxy_sources SET name = ?1, url = ?2, priority = ?3, updated_at = ?4 WHERE id = ?5",
        rusqlite::params![name, url, priority, now, id],
    )
    .map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(Box::new(e)),
    })?;

    get_proxy_source(conn, id)?.ok_or_else(|| crate::error::CoreError::NotFound {
        what: "proxy_source".to_string(),
        id: id.to_string(),
    })
}

pub fn delete_proxy_source(conn: &Connection, id: &str) -> crate::error::Result<bool> {
    let count = conn
        .execute(
            "DELETE FROM proxy_sources WHERE id = ?1",
            rusqlite::params![id],
        )
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;
    Ok(count > 0)
}

pub async fn fetch_custom_proxy_source(
    source_name: &str,
    url: &str,
    priority: i32,
) -> crate::error::Result<Vec<ScrapedProxy>> {
    use openproxy_adapters::upstream::{TimeoutProfile, UpstreamClient, UpstreamRequest};
    let client = UpstreamClient::new();
    let req = UpstreamRequest::get(url);
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let res = client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| {
            crate::error::CoreError::Internal(format!("Custom proxy source HTTP error: {:?}", e))
        })?;

    if res.status != 200 {
        return Err(crate::error::CoreError::Internal(format!(
            "Custom proxy source HTTP status: {}",
            res.status
        )));
    }

    let body_bytes = res.collect().await.map_err(|e| {
        crate::error::CoreError::Internal(format!("Custom proxy source body error: {:?}", e))
    })?;
    let text = String::from_utf8_lossy(&body_bytes);
    let mut list = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let (proto, host_port) = if let Some(idx) = trimmed.find("://") {
            (&trimmed[..idx], &trimmed[idx + 3..])
        } else {
            ("http", trimmed)
        };

        let parts: Vec<&str> = host_port.split(':').collect();
        if parts.len() >= 2 {
            let host = parts[0].trim().to_string();
            if let Ok(port) = parts[1].trim().parse::<u16>() {
                let username = parts.get(2).map(|s| s.trim().to_string());
                let password = parts.get(3).map(|s| s.trim().to_string());
                if !host.is_empty() && port > 0 {
                    list.push(ScrapedProxy {
                        source: source_name.to_string(),
                        host,
                        port,
                        r#type: proto.to_lowercase(),
                        country_code: None,
                        username,
                        password,
                        priority,
                    });
                }
            }
        }
    }

    Ok(list)
}

pub async fn test_proxy_source_url(url: &str) -> crate::error::Result<usize> {
    let list = fetch_custom_proxy_source("test", url, 0).await?;
    Ok(list.len())
}

/// Sync all providers using a `ServiceContainer` for dependency injection.
pub async fn sync_all_providers_with_container(
    services: &crate::di::ServiceContainer,
) -> crate::error::Result<SyncSummary> {
    let db_pool = services.db_pool()?;
    sync_all_providers(db_pool).await
}

pub async fn sync_all_providers(db_pool: Arc<DbPool>) -> crate::error::Result<SyncSummary> {
    let mut errors = Vec::new();
    let mut fetched = 0;
    let mut scraped = Vec::new();

    let pool_for_sources = Arc::clone(&db_pool);
    let sources_res = tokio::task::spawn_blocking(move || -> crate::error::Result<_> {
        let w = pool_for_sources.open_connection().map_err(openproxy_db::error::map_db_error)?;
        // Ensure built-in sources exist
        let builtins = vec![
            ("builtin_proxifly", "Proxifly (Built-in)", ""),
            ("builtin_github", "GitHub Lists (Built-in)", ""),
            ("builtin_oneproxy", "1proxy (Built-in)", ""),
            ("builtin_proxyscrape", "ProxyScrape (Built-in)", ""),
            ("builtin_geonode", "Geonode (Built-in)", ""),
            ("builtin_clearproxy", "ClearProxy (Built-in)", ""),
            ("builtin_vakhov", "Vakhov (Built-in)", ""),
            ("builtin_gproxynet", "GProxyNet (Built-in)", ""),
        ];
        for (id, name, url) in builtins {
            let _ = w.execute(
                "INSERT OR IGNORE INTO proxy_sources (id, name, url, active, is_builtin) VALUES (?1, ?2, ?3, 1, 1)",
                rusqlite::params![id, name, url],
            );
        }
        list_proxy_sources(&w)
    })
    .await;

    if let Ok(Ok(custom_sources)) = sources_res {
        for src in custom_sources {
            if !src.active {
                continue;
            }
            if src.is_builtin {
                match src.id.as_str() {
                    "builtin_proxifly" => {
                        if let Ok(mut list) = sync_proxifly().await {
                            scraped.append(&mut list);
                            fetched += list.len();
                        }
                    }
                    "builtin_github" => {
                        if let Ok(mut list) = sync_github_lists().await {
                            scraped.append(&mut list);
                            fetched += list.len();
                        }
                    }
                    "builtin_oneproxy" => {
                        if let Ok(mut list) = sync_oneproxy().await {
                            scraped.append(&mut list);
                            fetched += list.len();
                        }
                    }
                    "builtin_proxyscrape" => {
                        if let Ok(mut list) = sync_proxyscrape_cdn().await {
                            scraped.append(&mut list);
                            fetched += list.len();
                        }
                    }
                    "builtin_geonode" => {
                        if let Ok(mut list) = sync_geonode().await {
                            scraped.append(&mut list);
                            fetched += list.len();
                        }
                    }
                    "builtin_clearproxy" => {
                        if let Ok(mut list) = sync_clearproxy().await {
                            scraped.append(&mut list);
                            fetched += list.len();
                        }
                    }
                    "builtin_vakhov" => {
                        if let Ok(mut list) = sync_vakhov().await {
                            scraped.append(&mut list);
                            fetched += list.len();
                        }
                    }
                    "builtin_gproxynet" => {
                        if let Ok(mut list) = sync_gproxynet().await {
                            scraped.append(&mut list);
                            fetched += list.len();
                        }
                    }
                    _ => {}
                }
                continue;
            }

            match fetch_custom_proxy_source(&src.name, &src.url, src.priority).await {
                Ok(mut list) => {
                    fetched += list.len();
                    scraped.append(&mut list);
                }
                Err(e) => {
                    errors.push(format!(
                        "Custom proxy source '{}' sync failed: {}",
                        src.name, e
                    ));
                }
            }
        }
    }

    let mut added = 0;
    if !scraped.is_empty() {
        let (before_count, after_count) =
            tokio::task::spawn_blocking(move || -> Result<(i64, i64), crate::error::CoreError> {
                let mut w = db_pool.open_connection()?;
                let before: i64 = w
                    .query_row("SELECT COUNT(*) FROM free_proxies", [], |r| r.get(0))
                    .unwrap_or(0);

                upsert_scraped_proxies(&mut w, &scraped)?;

                let after: i64 = w
                    .query_row("SELECT COUNT(*) FROM free_proxies", [], |r| r.get(0))
                    .unwrap_or(0);
                Ok((before, after))
            })
            .await
            .map_err(|e| crate::error::CoreError::Internal(e.to_string()))??;

        added = (after_count - before_count) as usize;
    }

    Ok(SyncSummary {
        fetched,
        added,
        errors,
    })
}

// Proxy validation logic
pub async fn test_proxy_connection(
    test_url: &str,
    r#type: &str,
    host: &str,
    port: u16,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<i64, String> {
    use openproxy_adapters::upstream::{
        ResolvedTimeouts, TimeoutProfile, UpstreamClient, UpstreamRequest,
    };

    let proxy_url = if let (Some(u), Some(p)) = (username, password) {
        format!("{}://{}:{}@{}:{}", r#type, u, p, host, port)
    } else {
        format!("{}://{}:{}", r#type, host, port)
    };

    let client = UpstreamClient::new();
    let mut req = UpstreamRequest::get(test_url);
    req.proxy = Some(proxy_url);

    // Timeout budget for proxy probe test.
    let profile = TimeoutProfile::Custom(ResolvedTimeouts {
        dns_ms: 3000,
        dial_ms: 5000,
        tls_ms: 5000,
        write_ms: 3000,
        headers_ms: 8000,
        body_chunk_ms: 3000,
        total_ms: 8000,
    });
    let cancel = openproxy_adapters::upstream::CancellationToken::new();

    let start = std::time::Instant::now();
    let res = client.call(req, profile, cancel).await;

    match res {
        Ok(r) => {
            if r.status == 204 || r.status == 200 {
                let latency = start.elapsed().as_millis() as i64;
                Ok(latency)
            } else {
                Err(format!("Status check failed: HTTP {}", r.status))
            }
        }
        Err(e) => Err(format!("Connection probe failed: {:?}", e)),
    }
}

pub async fn test_single_proxy(db_pool: Arc<DbPool>, id: &str) -> crate::error::Result<FreeProxy> {
    let test_url = {
        let r = db_pool.reader();
        openproxy_db::app_config::load_proxy_test_url(&r)
            .unwrap_or_else(|_| openproxy_db::app_config::PROXY_TEST_URL_DEFAULT.to_string())
    };

    let (r#type, host, port, username, password) = {
        let r = db_pool.reader();
        let mut stmt = r
            .prepare("SELECT type, host, port, username, password FROM free_proxies WHERE id = ?1")
            .map_err(|e| crate::error::CoreError::Database {
                message: e.to_string(),
                source: Some(Box::new(e)),
            })?;
        stmt.query_row(rusqlite::params![id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u16>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?
    };

    let test_res = test_proxy_connection(
        &test_url,
        &r#type,
        &host,
        port,
        username.as_deref(),
        password.as_deref(),
    )
    .await;

    let w = db_pool.writer();
    match test_res {
        Ok(latency) => {
            update_proxy_status(&w, id, "alive", Some(latency))?;
        }
        Err(_) => {
            update_proxy_status(&w, id, "dead", None)?;
        }
    }

    let p = get_proxy(&w, id)?.ok_or_else(|| crate::error::CoreError::NotFound {
        what: "proxy".to_string(),
        id: id.to_string(),
    })?;

    Ok(p)
}

pub fn test_all_proxies_background(db_pool: Arc<DbPool>) {
    tokio::spawn(async move {
        let proxies = {
            let r = db_pool.reader();
            let mut stmt = match r.prepare(
                "
                SELECT id, type, host, port, username, password FROM free_proxies 
                ORDER BY 
                    CASE status 
                        WHEN 'unknown' THEN 1 
                        WHEN 'alive' THEN 2 
                        ELSE 3 
                    END ASC,
                    priority DESC
            ",
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("Failed to prepare list query in background test: {}", e);
                    return;
                }
            };
            let rows = match stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, u16>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            }) {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!("Failed to query list in background test: {}", e);
                    return;
                }
            };
            let mut list = Vec::new();
            for item in rows.flatten() {
                list.push(item);
            }
            list
        };

        use futures::StreamExt;
        let pool_clone = Arc::clone(&db_pool);

        let test_url = {
            let r = db_pool.reader();
            openproxy_db::app_config::load_proxy_test_url(&r)
                .unwrap_or_else(|_| openproxy_db::app_config::PROXY_TEST_URL_DEFAULT.to_string())
        };

        futures::stream::iter(proxies)
            .for_each_concurrent(20, move |(id, r#type, host, port, username, password)| {
                let pool = Arc::clone(&pool_clone);
                let test_url = test_url.to_owned();
                async move {
                    let test_res = test_proxy_connection(
                        &test_url,
                        &r#type,
                        &host,
                        port,
                        username.as_deref(),
                        password.as_deref(),
                    )
                    .await;
                    let _ = tokio::task::spawn_blocking(
                        move || -> Result<(), crate::error::CoreError> {
                            let w = pool.open_connection()?;
                            match test_res {
                                Ok(latency) => {
                                    let _ =
                                        update_proxy_status(&w, &id, "alive", Some(latency));
                                }
                                Err(_) => {
                                    // Only mark dead if status was not already alive
                                    let current_status: Option<String> = w
                                        .query_row(
                                            "SELECT status FROM free_proxies WHERE id = ?1",
                                            rusqlite::params![&id],
                                            |r| r.get(0),
                                        )
                                        .ok();
                                    if current_status.as_deref() != Some("alive") {
                                        let _ = update_proxy_status(&w, &id, "dead", None);
                                    }
                                }
                            }
                            Ok(())
                        },
                    )
                    .await;
                }
            })
            .await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE free_proxies (
              id TEXT PRIMARY KEY,
              source TEXT NOT NULL,
              host TEXT NOT NULL,
              port INTEGER NOT NULL,
              type TEXT NOT NULL DEFAULT 'http',
              country_code TEXT,
              status TEXT NOT NULL DEFAULT 'unknown',
              latency_ms INTEGER,
              last_validated TEXT,
              created_at TEXT NOT NULL DEFAULT (datetime('now')),
              updated_at TEXT NOT NULL DEFAULT (datetime('now')),
              username TEXT,
              password TEXT,
              priority INTEGER DEFAULT 0,
              UNIQUE(host, port)
            );
            CREATE TABLE proxy_sources (
              id TEXT PRIMARY KEY,
              name TEXT NOT NULL,
              url TEXT NOT NULL,
              priority INTEGER NOT NULL DEFAULT 0,
              active INTEGER NOT NULL DEFAULT 1,
              is_builtin INTEGER NOT NULL DEFAULT 0,
              created_at TEXT NOT NULL DEFAULT (datetime('now')),
              updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE providers (
              id TEXT PRIMARY KEY,
              name TEXT NOT NULL,
              base_url TEXT NOT NULL,
              auth_type TEXT NOT NULL,
              format TEXT NOT NULL,
              extra_headers_json TEXT,
              auto_activate_keyword TEXT,
              use_proxies INTEGER DEFAULT 0,
              current_proxy_id TEXT,
              proxy_rotation_errors TEXT DEFAULT '429,connect_error,timeout',
              rate_limit_scope TEXT DEFAULT 'account',
              proxy_rotation_mode TEXT DEFAULT 'global',
              active INTEGER NOT NULL DEFAULT 1,
              created_at TEXT NOT NULL DEFAULT (datetime('now')),
              updated_at TEXT NOT NULL DEFAULT (datetime('now')),
              CHECK (format IN ('openai', 'anthropic', 'mixed', 'gemini', 'responses'))
            );
            CREATE TABLE accounts (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              provider_id TEXT NOT NULL,
              current_proxy_id TEXT
            );
            ",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_crud_custom_proxy() {
        let conn = setup_test_db();

        let p = add_custom_proxy(
            &conn,
            "1.2.3.4".to_string(),
            8080,
            "http".to_string(),
            Some("US".to_string()),
            None,
            None,
        )
        .unwrap();
        assert_eq!(p.host, "1.2.3.4");
        assert_eq!(p.port, 8080);
        assert_eq!(p.r#type, "http");
        assert_eq!(p.country_code.as_deref(), Some("US"));
        assert_eq!(p.status, "unknown");

        let list = list_proxies(&conn, None, None, None, None, None, None).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, p.id);

        update_proxy_status(&conn, &p.id, "alive", Some(150)).unwrap();

        let list2 = list_proxies(&conn, None, Some("alive"), None, None, None, None).unwrap();
        assert_eq!(list2.len(), 1);
        assert_eq!(list2[0].status, "alive");
        assert_eq!(list2[0].latency_ms, Some(150));

        delete_proxy(&conn, &p.id).unwrap();
        let list3 = list_proxies(&conn, None, None, None, None, None, None).unwrap();
        assert_eq!(list3.len(), 0);
    }

    #[test]
    fn test_upsert_scraped_proxies() {
        let mut conn = setup_test_db();

        let scraped = vec![
            ScrapedProxy {
                source: "proxifly".to_string(),
                host: "10.0.0.1".to_string(),
                port: 3128,
                r#type: "https".to_string(),
                country_code: Some("FR".to_string()),
                username: None,
                password: None,
                priority: 0,
            },
            ScrapedProxy {
                source: "iplocate".to_string(),
                host: "10.0.0.2".to_string(),
                port: 1080,
                r#type: "socks5".to_string(),
                country_code: None,
                username: None,
                password: None,
                priority: 0,
            },
        ];

        upsert_scraped_proxies(&mut conn, &scraped).unwrap();

        let list = list_proxies(&conn, None, None, None, None, None, None).unwrap();
        assert_eq!(list.len(), 2);

        upsert_scraped_proxies(&mut conn, &scraped).unwrap();
        let list2 = list_proxies(&conn, None, None, None, None, None, None).unwrap();
        assert_eq!(list2.len(), 2);
    }

    #[test]
    fn test_get_or_assign_provider_proxy_flow() {
        let conn = setup_test_db();

        let provider_id = crate::ids::ProviderId::new("test-provider");

        // 1. Insert a provider with use_proxies = 0 (default)
        conn.execute(
            "INSERT INTO providers (id, name, base_url, auth_type, format) VALUES (?1, 'Test', 'http://localhost', 'bearer', 'openai')",
            rusqlite::params![provider_id.0],
        ).unwrap();

        // No proxies in database yet. Since use_proxies = 0, should return Ok(None)
        let proxy = get_or_assign_provider_proxy(&conn, &provider_id, None).unwrap();
        assert_eq!(proxy, None);

        // 2. Enable use_proxies = 1
        conn.execute(
            "UPDATE providers SET use_proxies = 1 WHERE id = ?1",
            rusqlite::params![provider_id.0],
        )
        .unwrap();

        // Still no proxies in DB, so it should return Err because use_proxies = 1
        assert!(get_or_assign_provider_proxy(&conn, &provider_id, None).is_err());

        // 3. Add an alive proxy
        let p = add_custom_proxy(
            &conn,
            "1.2.3.4".to_string(),
            8080,
            "socks5".to_string(),
            None,
            None,
            None,
        )
        .unwrap();
        update_proxy_status(&conn, &p.id, "alive", Some(100)).unwrap();

        // Now it should assign and return this socks5 proxy!
        let proxy = get_or_assign_provider_proxy(&conn, &provider_id, None).unwrap();
        assert_eq!(proxy, Some("socks5://1.2.3.4:8080".to_string()));

        // Add 15m cooldown for this proxy on test-provider
        openproxy_db::cooldowns::add_provider_proxy_cooldown(
            "test-provider",
            &p.id,
            std::time::Duration::from_secs(900),
        );

        // When in cooldown, get_or_assign_provider_proxy should not assign it if other alive proxy exists or returns err if no other exists
        let proxy_after_cooldown = get_or_assign_provider_proxy(&conn, &provider_id, None).unwrap();
        assert_eq!(
            proxy_after_cooldown,
            Some("socks5://1.2.3.4:8080".to_string())
        ); // fallback when only 1 alive

        // The provider's current_proxy_id should now be bound to p.id
        let bound_id: Option<String> = conn
            .query_row(
                "SELECT current_proxy_id FROM providers WHERE id = ?1",
                rusqlite::params![provider_id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bound_id.as_deref(), Some(p.id.as_str()));

        // Calling it again should return the same cached proxy
        let proxy2 = get_or_assign_provider_proxy(&conn, &provider_id, None).unwrap();
        assert_eq!(proxy2, Some("socks5://1.2.3.4:8080".to_string()));

        // 4. Mark the proxy as dead / inactive
        update_proxy_status(&conn, &p.id, "dead", Some(9999)).unwrap();

        // Since it's dead, get_or_assign_provider_proxy should detect it as dead,
        // search for a new one, find none, and return Err because use_proxies = 1.
        assert!(get_or_assign_provider_proxy(&conn, &provider_id, None).is_err());
    }

    #[test]
    fn test_proxyscrape_cdn_json_parsing() {
        let json_data = r#"[
            {
                "protocol": "socks4",
                "ip": "95.217.167.252",
                "port": 11117,
                "country": "Finland",
                "country_code": "FI"
            }
        ]"#;
        let items: Vec<ProxyScrapeCdnItem> = serde_json::from_str(json_data).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].ip, "95.217.167.252");
        assert_eq!(items[0].port, 11117);
        assert_eq!(items[0].protocol, "socks4");
        assert_eq!(items[0].country_code.as_deref(), Some("FI"));
    }

    #[test]
    fn test_geonode_json_parsing() {
        let json_data = r#"{
            "data": [
                {
                    "ip": "193.233.72.56",
                    "port": "988",
                    "protocols": ["socks5"],
                    "country": "US"
                }
            ]
        }"#;
        let body: GeonodeResponse = serde_json::from_str(json_data).unwrap();
        assert_eq!(body.data.len(), 1);
        assert_eq!(body.data[0].ip, "193.233.72.56");
        assert_eq!(body.data[0].port, "988");
        assert_eq!(body.data[0].port.parse::<u16>().unwrap(), 988);
        assert_eq!(body.data[0].protocols, vec!["socks5"]);
    }

    #[test]
    fn test_get_proxy_status_by_url() {
        let conn = setup_test_db();

        let p = add_custom_proxy(
            &conn,
            "1.2.3.4".to_string(),
            8080,
            "socks5".to_string(),
            None,
            None,
            None,
        )
        .unwrap();

        // 1. Initial status should be "unknown"
        assert_eq!(
            get_proxy_status_by_url(&conn, "socks5://1.2.3.4:8080"),
            Some("unknown".to_string())
        );

        // 2. Change status and verify it's updated
        update_proxy_status(&conn, &p.id, "alive", Some(100)).unwrap();
        assert_eq!(
            get_proxy_status_by_url(&conn, "socks5://1.2.3.4:8080"),
            Some("alive".to_string())
        );

        // 3. Test malformed URLs and non-existent proxy
        assert_eq!(get_proxy_status_by_url(&conn, "1.2.3.4:8080"), None);
        assert_eq!(get_proxy_status_by_url(&conn, "socks5://1.2.3.4"), None);
        assert_eq!(
            get_proxy_status_by_url(&conn, "socks5://9.9.9.9:8080"),
            None
        );
    }

    #[test]
    fn test_proxy_sources_crud() {
        let conn = setup_test_db();

        let src = create_proxy_source(
            &conn,
            CreateProxySourceInput {
                name: "Test Source".to_string(),
                url: "http://example.com/proxies.txt".to_string(),
                priority: Some(5),
                active: Some(true),
            },
        )
        .unwrap();

        assert_eq!(src.name, "Test Source");
        assert_eq!(src.url, "http://example.com/proxies.txt");
        assert_eq!(src.priority, 5);

        let list = list_proxy_sources(&conn).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, src.id);

        let fetched = get_proxy_source(&conn, &src.id).unwrap().unwrap();
        assert_eq!(fetched.name, "Test Source");

        let updated = update_proxy_source(
            &conn,
            &src.id,
            UpdateProxySourceInput {
                name: Some("Updated Source".to_string()),
                url: None,
                priority: Some(10),
                active: None,
            },
        )
        .unwrap();

        assert_eq!(updated.name, "Updated Source");
        assert_eq!(updated.priority, 10);

        let deleted = delete_proxy_source(&conn, &src.id).unwrap();
        assert!(deleted);

        let list_after = list_proxy_sources(&conn).unwrap();
        assert_eq!(list_after.len(), 0);
    }
}
