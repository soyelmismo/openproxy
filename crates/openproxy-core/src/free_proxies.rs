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
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncSummary {
    pub fetched: usize,
    pub added: usize,
    pub errors: Vec<String>,
}

pub fn list_proxies(
    conn: &Connection,
    source: Option<&str>,
    status: Option<&str>,
) -> crate::error::Result<Vec<FreeProxy>> {
    let mut sql = "SELECT id, source, host, port, type, country_code, status, latency_ms, last_validated, created_at, updated_at FROM free_proxies WHERE 1=1".to_string();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(src) = source {
        sql.push_str(" AND source = ?");
        params.push(Box::new(src.to_string()));
    }

    if let Some(st) = status {
        sql.push_str(" AND status = ?");
        params.push(Box::new(st.to_string()));
    }

    sql.push_str(" ORDER BY status = 'alive' DESC, latency_ms ASC, updated_at DESC");

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
                    created_at: row.get(9)?,
                    updated_at: row.get(10)?,
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
        .prepare("SELECT id, source, host, port, type, country_code, status, latency_ms, last_validated, created_at, updated_at FROM free_proxies WHERE id = ?1")
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
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
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
) -> crate::error::Result<FreeProxy> {
    let id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();

    conn.execute(
        "INSERT INTO free_proxies (id, source, host, port, type, country_code, status, latency_ms, last_validated, created_at, updated_at) \
         VALUES (?1, 'custom', ?2, ?3, ?4, ?5, 'unknown', NULL, NULL, ?6, ?7) \
         ON CONFLICT(host, port) DO UPDATE SET \
           source = 'custom', \
           type = excluded.type, \
           country_code = COALESCE(excluded.country_code, free_proxies.country_code), \
           updated_at = excluded.updated_at",
        rusqlite::params![id, host, port, r#type.to_lowercase(), country_code, now, now],
    )
    .map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(Box::new(e)),
    })?;

    let mut stmt = conn
        .prepare("SELECT id, source, host, port, type, country_code, status, latency_ms, last_validated, created_at, updated_at FROM free_proxies WHERE host = ?1 AND port = ?2")
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
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
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
) -> crate::error::Result<Option<String>> {
    use rusqlite::OptionalExtension;

    // 1. Fetch provider details to see use_proxies and current_proxy_id
    let provider = match crate::providers::get(conn, provider_id)? {
        Some(p) => p,
        None => return Ok(None),
    };

    if !provider.use_proxies {
        return Ok(None);
    }

    // 2. If current_proxy_id is set, verify it is still alive/valid
    if let Some(ref proxy_id) = provider.current_proxy_id {
        let exists_and_alive: Option<(String, i64, String)> = conn
            .query_row(
                "SELECT host, port, type FROM free_proxies WHERE id = ?1 AND status = 'alive'",
                rusqlite::params![proxy_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(|e| crate::error::CoreError::Database {
                message: format!("query current proxy: {}", e),
                source: Some(Box::new(e)),
            })?;

        if let Some((host, port, proto)) = exists_and_alive {
            return Ok(Some(format!(
                "{}://{}:{}",
                proto.to_lowercase(),
                host,
                port
            )));
        }
    }

    // 3. If current_proxy_id is unset or dead, select a new one from the alive pool
    // Order by latency_ms ascending so we pick the fastest one.
    let new_proxy: Option<(String, String, i64, String)> = conn
        .query_row(
            "SELECT id, host, port, type FROM free_proxies WHERE status = 'alive' ORDER BY latency_ms ASC, random() LIMIT 1",
            [],
            |row| Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            )),
        )
        .optional()
        .map_err(|e| crate::error::CoreError::Database {
            message: format!("query new proxy: {}", e),
            source: Some(Box::new(e)),
        })?;

    if let Some((id, host, port, proto)) = new_proxy {
        crate::providers::update_current_proxy(conn, provider_id, Some(&id))?;
        return Ok(Some(format!(
            "{}://{}:{}",
            proto.to_lowercase(),
            host,
            port
        )));
    }

    Ok(None)
}

pub fn upsert_scraped_proxies(
    conn: &mut Connection,
    proxies: &[ScrapedProxy],
) -> crate::error::Result<()> {
    let now = chrono::Utc::now().to_rfc3339();
    let tx = conn.transaction().map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(Box::new(e)),
    })?;
    for p in proxies {
        let id = uuid::Uuid::new_v4().to_string();
        tx.execute(
            "INSERT INTO free_proxies (id, source, host, port, type, country_code, status, latency_ms, last_validated, created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'unknown', NULL, NULL, ?7, ?8) \
             ON CONFLICT(host, port) DO UPDATE SET \
               source = CASE WHEN free_proxies.source = 'custom' THEN 'custom' ELSE excluded.source END, \
               type = excluded.type, \
               country_code = COALESCE(excluded.country_code, free_proxies.country_code), \
               updated_at = excluded.updated_at",
            rusqlite::params![id, p.source, p.host, p.port, p.r#type, p.country_code, now, now],
        )
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
            }
        })
        .collect();
    Ok(list)
}

async fn sync_iplocate() -> crate::error::Result<Vec<ScrapedProxy>> {
    use openproxy_adapters::upstream::{TimeoutProfile, UpstreamClient, UpstreamRequest};
    let client = UpstreamClient::new();
    let mut list = Vec::new();
    let protocols = vec!["http", "https", "socks4", "socks5"];

    for proto in protocols {
        let url = format!(
            "https://raw.githubusercontent.com/iplocate/free-proxy-list/main/protocols/{}.txt",
            proto
        );
        let req = UpstreamRequest::get(url);
        let cancel = openproxy_adapters::upstream::CancellationToken::new();
        let res = match client
            .call(req, TimeoutProfile::ModelDiscovery, cancel)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("iplocate fetch error for {}: {:?}", proto, e);
                continue;
            }
        };
        if res.status != 200 {
            tracing::warn!("iplocate status error for {}: {}", proto, res.status);
            continue;
        }
        let body_bytes = match res.collect().await {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("iplocate body error for {}: {:?}", proto, e);
                continue;
            }
        };
        let text = match String::from_utf8(body_bytes.to_vec()) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("iplocate decode error for {}: {}", proto, e);
                continue;
            }
        };
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
                        source: "iplocate".to_string(),
                        host,
                        port,
                        r#type: proto.to_string(),
                        country_code: None,
                    });
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
        })
        .collect();
    Ok(list)
}

pub async fn sync_all_providers(db_pool: Arc<DbPool>) -> crate::error::Result<SyncSummary> {
    let mut errors = Vec::new();
    let mut fetched = 0;
    let mut scraped = Vec::new();

    // 1. Proxifly
    match sync_proxifly().await {
        Ok(mut list) => {
            fetched += list.len();
            scraped.append(&mut list);
        }
        Err(e) => {
            errors.push(format!("Proxifly sync failed: {}", e));
        }
    }

    // 2. IPLocate
    match sync_iplocate().await {
        Ok(mut list) => {
            fetched += list.len();
            scraped.append(&mut list);
        }
        Err(e) => {
            errors.push(format!("IPLocate sync failed: {}", e));
        }
    }

    // 3. 1proxy
    match sync_oneproxy().await {
        Ok(mut list) => {
            fetched += list.len();
            scraped.append(&mut list);
        }
        Err(e) => {
            errors.push(format!("1proxy sync failed: {}", e));
        }
    }

    let mut added = 0;
    if !scraped.is_empty() {
        let scraped_clone = scraped.clone();
        let db_pool = db_pool.clone();
        let (before_count, after_count) = tokio::task::spawn_blocking(move || -> Result<(i64, i64), crate::error::CoreError> {
            let mut w = db_pool.open_connection()?;
            let before: i64 = w
                .query_row("SELECT COUNT(*) FROM free_proxies", [], |r| r.get(0))
                .unwrap_or(0);
            
            upsert_scraped_proxies(&mut w, &scraped_clone)?;
            
            let after: i64 = w
                .query_row("SELECT COUNT(*) FROM free_proxies", [], |r| r.get(0))
                .unwrap_or(0);
            Ok((before, after))
        }).await.map_err(|e| crate::error::CoreError::Internal(e.to_string()))??;

        added = (after_count - before_count) as usize;
    }

    Ok(SyncSummary {
        fetched,
        added,
        errors,
    })
}

// Proxy validation logic
pub async fn test_proxy_connection(r#type: &str, host: &str, port: u16) -> Result<i64, String> {
    use openproxy_adapters::upstream::{ResolvedTimeouts, TimeoutProfile, UpstreamClient, UpstreamRequest};
    let proxy_url = format!("{}://{}:{}", r#type, host, port);

    let client = UpstreamClient::new();
    let mut req = UpstreamRequest::get("https://clients3.google.com/generate_204");
    req.proxy = Some(proxy_url);

    // Tight timeout for the proxy test (equivalent to the old 5s timeout).
    let profile = TimeoutProfile::Custom(ResolvedTimeouts {
        dns_ms: 2000,
        dial_ms: 3000,
        tls_ms: 3000,
        write_ms: 2000,
        headers_ms: 5000,
        body_chunk_ms: 2000,
        total_ms: 5000,
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
    let (r#type, host, port) = {
        let r = db_pool.reader();
        let mut stmt = r
            .prepare("SELECT type, host, port FROM free_proxies WHERE id = ?1")
            .map_err(|e| crate::error::CoreError::Database {
                message: e.to_string(),
                source: Some(Box::new(e)),
            })?;
        stmt.query_row(rusqlite::params![id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, u16>(2)?,
            ))
        })
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?
    };

    let res = test_proxy_connection(&r#type, &host, port).await;

    let w = db_pool.writer();
    match res {
        Ok(latency) => {
            update_proxy_status(&w, id, "alive", Some(latency))?;
        }
        Err(_) => {
            update_proxy_status(&w, id, "dead", None)?;
        }
    }

    let r = db_pool.reader();
    let mut stmt = r
        .prepare("SELECT id, source, host, port, type, country_code, status, latency_ms, last_validated, created_at, updated_at FROM free_proxies WHERE id = ?1")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;

    let p = stmt
        .query_row(rusqlite::params![id], |row| {
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
                created_at: row.get(9)?,
                updated_at: row.get(10)?,
            })
        })
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;

    Ok(p)
}

pub fn test_all_proxies_background(db_pool: Arc<DbPool>) {
    tokio::spawn(async move {
        let proxies = {
            let r = db_pool.reader();
            let mut stmt = match r.prepare(
                "
                SELECT id, type, host, port FROM free_proxies 
                ORDER BY 
                    CASE status 
                        WHEN 'unknown' THEN 1 
                        WHEN 'alive' THEN 2 
                        ELSE 3 
                    END ASC
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
        let pool_clone = db_pool.clone();

        futures::stream::iter(proxies)
            .map(move |(id, r#type, host, port)| {
                let pool = pool_clone.clone();
                async move {
                    let test_res = test_proxy_connection(&r#type, &host, port).await;
                    let db_pool = pool.clone();
                    let id_clone = id.clone();
                    let _ = tokio::task::spawn_blocking(move || -> Result<(), crate::error::CoreError> {
                        let w = db_pool.open_connection()?;
                        match test_res {
                            Ok(latency) => {
                                let _ = update_proxy_status(&w, &id_clone, "alive", Some(latency));
                            }
                            Err(_) => {
                                let _ = update_proxy_status(&w, &id_clone, "dead", None);
                            }
                        }
                        Ok(())
                    }).await;
                }
            })
            .buffer_unordered(20)
            .collect::<Vec<()>>()
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
              UNIQUE(host, port)
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
              active INTEGER NOT NULL DEFAULT 1,
              created_at TEXT NOT NULL DEFAULT (datetime('now')),
              updated_at TEXT NOT NULL DEFAULT (datetime('now')),
              CHECK (format IN ('openai', 'anthropic', 'mixed', 'gemini', 'responses'))
            );",
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
        )
        .unwrap();
        assert_eq!(p.host, "1.2.3.4");
        assert_eq!(p.port, 8080);
        assert_eq!(p.r#type, "http");
        assert_eq!(p.country_code.as_deref(), Some("US"));
        assert_eq!(p.status, "unknown");

        let list = list_proxies(&conn, None, None).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, p.id);

        update_proxy_status(&conn, &p.id, "alive", Some(150)).unwrap();

        let list2 = list_proxies(&conn, None, Some("alive")).unwrap();
        assert_eq!(list2.len(), 1);
        assert_eq!(list2[0].status, "alive");
        assert_eq!(list2[0].latency_ms, Some(150));

        delete_proxy(&conn, &p.id).unwrap();
        let list3 = list_proxies(&conn, None, None).unwrap();
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
            },
            ScrapedProxy {
                source: "iplocate".to_string(),
                host: "10.0.0.2".to_string(),
                port: 1080,
                r#type: "socks5".to_string(),
                country_code: None,
            },
        ];

        upsert_scraped_proxies(&mut conn, &scraped).unwrap();

        let list = list_proxies(&conn, None, None).unwrap();
        assert_eq!(list.len(), 2);

        upsert_scraped_proxies(&mut conn, &scraped).unwrap();
        let list2 = list_proxies(&conn, None, None).unwrap();
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
        let proxy = get_or_assign_provider_proxy(&conn, &provider_id).unwrap();
        assert_eq!(proxy, None);

        // 2. Enable use_proxies = 1
        conn.execute(
            "UPDATE providers SET use_proxies = 1 WHERE id = ?1",
            rusqlite::params![provider_id.0],
        )
        .unwrap();

        // Still no proxies in DB, so it should return Ok(None)
        let proxy = get_or_assign_provider_proxy(&conn, &provider_id).unwrap();
        assert_eq!(proxy, None);

        // 3. Add an alive proxy
        let p = add_custom_proxy(
            &conn,
            "1.2.3.4".to_string(),
            8080,
            "socks5".to_string(),
            None,
        )
        .unwrap();
        update_proxy_status(&conn, &p.id, "alive", Some(100)).unwrap();

        // Now it should assign and return this socks5 proxy!
        let proxy = get_or_assign_provider_proxy(&conn, &provider_id).unwrap();
        assert_eq!(proxy, Some("socks5://1.2.3.4:8080".to_string()));

        // The provider's current_proxy_id should now be bound to p.id
        let bound_id: Option<String> = conn
            .query_row(
                "SELECT current_proxy_id FROM providers WHERE id = ?1",
                rusqlite::params![provider_id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bound_id, Some(p.id.clone()));

        // Calling it again should return the same cached proxy
        let proxy2 = get_or_assign_provider_proxy(&conn, &provider_id).unwrap();
        assert_eq!(proxy2, Some("socks5://1.2.3.4:8080".to_string()));

        // 4. Mark the proxy as dead / inactive
        update_proxy_status(&conn, &p.id, "dead", Some(9999)).unwrap();

        // Since it's dead, get_or_assign_provider_proxy should detect it as dead,
        // search for a new one, find none, and return Ok(None).
        let proxy3 = get_or_assign_provider_proxy(&conn, &provider_id).unwrap();
        assert_eq!(proxy3, None);
    }
}
