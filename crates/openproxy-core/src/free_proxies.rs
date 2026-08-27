//! staging table of free scraped/custom proxies + validation.

use futures::StreamExt;
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

    let now = chrono::Utc::now().to_rfc3339();
    let tx = conn
        .transaction()
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;

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
    .map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(std::sync::Arc::new(e)),
    })?;

    tx.commit().map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(std::sync::Arc::new(e)),
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

async fn fetch_upstream_json<T: serde::de::DeserializeOwned>(
    url: &str,
    name: &str,
) -> crate::error::Result<T> {
    use openproxy_adapters::upstream::{TimeoutProfile, UpstreamClient, UpstreamRequest};
    let client = UpstreamClient::new();
    let req = UpstreamRequest::get(url);
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let res = client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("{name} HTTP error: {e:?}")))?;

    if res.status != 200 {
        return Err(crate::error::CoreError::Internal(format!(
            "{name} HTTP status: {}",
            res.status
        )));
    }

    let body_bytes = res
        .collect()
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("{name} body error: {e:?}")))?;
    serde_json::from_slice(&body_bytes)
        .map_err(|e| crate::error::CoreError::Internal(format!("{name} JSON error: {e}")))
}

async fn sync_proxifly() -> crate::error::Result<Vec<ScrapedProxy>> {
    let items: Vec<ProxiflyItem> = fetch_upstream_json(
        "https://api.proxifly.dev/proxy?format=json&quantity=100",
        "Proxifly",
    )
    .await?;

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

fn parse_proxy_host_port(proxy: &str) -> Option<(String, u16)> {
    let (host, port_str) = proxy.rsplit_once(':')?;
    let host = host.trim();
    let port = port_str.trim().parse::<u16>().ok()?;
    if host.is_empty() || port == 0 {
        None
    } else {
        Some((host.to_string(), port))
    }
}

fn parse_plain_proxy_lines(text: &str, src_name: &str, proto: &str) -> Vec<ScrapedProxy> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let (host, port) = parse_proxy_host_port(trimmed)?;
            Some(ScrapedProxy {
                source: src_name.to_string(),
                host,
                port,
                r#type: proto.to_string(),
                country_code: None,
                username: None,
                password: None,
                priority: 0,
            })
        })
        .collect()
}

async fn fetch_github_proxy_file(
    client: &Arc<openproxy_adapters::upstream::UpstreamClient>,
    src_name: &str,
    proto: &str,
    url: &str,
) -> Vec<ScrapedProxy> {
    let req = openproxy_adapters::upstream::UpstreamRequest::get(url);
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let Ok(res) = client
        .call(
            req,
            openproxy_adapters::upstream::TimeoutProfile::ModelDiscovery,
            cancel,
        )
        .await
    else {
        return Vec::new();
    };
    if res.status != 200 {
        return Vec::new();
    }
    let Ok(body_bytes) = res.collect().await else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&body_bytes);
    parse_plain_proxy_lines(&text, src_name, proto)
}

async fn sync_github_lists() -> crate::error::Result<Vec<ScrapedProxy>> {
    let client = openproxy_adapters::upstream::UpstreamClient::new();
    let mut list = Vec::new();
    let sources: &[(&str, &str, &[&str])] = &[
        (
            "iplocate",
            "https://raw.githubusercontent.com/iplocate/free-proxy-list/main/protocols/{}.txt",
            &["http", "https", "socks4", "socks5"],
        ),
        (
            "hideip",
            "https://raw.githubusercontent.com/zloi-user/hideip.me/main/{}.txt",
            &["http", "socks4", "socks5"],
        ),
        (
            "r00tee",
            "https://raw.githubusercontent.com/r00tee/Proxy-List/main/Socks5.txt",
            &["socks5"],
        ),
        (
            "hookzof",
            "https://raw.githubusercontent.com/hookzof/socks5_list/master/proxy.txt",
            &["socks5"],
        ),
        (
            "anonymouswork",
            "https://raw.githubusercontent.com/Anonym0usWork1221/Free-Proxies/main/proxy_files/https_proxies.txt",
            &["https"],
        ),
        (
            "komutan234",
            "https://raw.githubusercontent.com/komutan234/Proxy-List-Free/main/proxies/http.txt",
            &["http"],
        ),
        (
            "yuceltoluyag",
            "https://raw.githubusercontent.com/yuceltoluyag/GoodProxy/main/raw.txt",
            &["http"],
        ),
    ];

    for &(src_name, url_template, protocols) in sources {
        for &proto in protocols {
            let url = url_template.replace("{}", proto);
            let mut proxies = fetch_github_proxy_file(&client, src_name, proto, &url).await;
            list.append(&mut proxies);
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
    let body: OneProxyApiResponse = fetch_upstream_json(
        "https://1proxy-api.aitradepulse.com/api/v1/proxies/advanced",
        "1proxy",
    )
    .await?;

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
    let items: Vec<ProxyScrapeCdnItem> = fetch_upstream_json(
        "https://cdn.jsdelivr.net/gh/proxyscrape/free-proxy-list@main/proxies/all/data.json",
        "ProxyScrape CDN",
    )
    .await?;

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
        .map_err(|e| crate::error::CoreError::Internal(format!("Geonode HTTP error: {e:?}")))?;

    if res.status != 200 {
        return Err(crate::error::CoreError::Internal(format!(
            "Geonode HTTP status: {}",
            res.status
        )));
    }

    let body_bytes = res
        .collect()
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("Geonode body error: {e:?}")))?;
    let body: GeonodeResponse = serde_json::from_slice(&body_bytes)
        .map_err(|e| crate::error::CoreError::Internal(format!("Geonode JSON error: {e}")))?;

    let list = body
        .data
        .into_iter()
        .filter_map(|item| {
            let port = item.port.parse::<u16>().ok()?;
            let proto = item
                .protocols
                .into_iter()
                .next()
                .unwrap_or_else(|| "http".to_string());
            Some(ScrapedProxy {
                source: "geonode".to_string(),
                host: item.ip,
                port,
                r#type: proto.to_lowercase(),
                country_code: item.country.filter(|c| !c.is_empty()),
                username: None,
                password: None,
                priority: 0,
            })
        })
        .collect();
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
    let items: Vec<ClearProxyItem> = fetch_upstream_json(
        "https://raw.githubusercontent.com/ClearProxy/checked-proxy-list/main/http/json/all.json",
        "ClearProxy",
    )
    .await?;

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

fn parse_vakhov_port(port: &serde_json::Value) -> Option<u16> {
    match port {
        serde_json::Value::Number(n) => n.as_u64().map(|v| v as u16),
        serde_json::Value::String(s) => s.parse::<u16>().ok(),
        _ => None,
    }
}

async fn sync_vakhov() -> crate::error::Result<Vec<ScrapedProxy>> {
    let items: Vec<VakhovItem> = fetch_upstream_json(
        "https://vakhov.github.io/fresh-proxy-list/proxylist.json",
        "Vakhov",
    )
    .await?;

    let list = items
        .into_iter()
        .filter_map(|item| {
            let port = parse_vakhov_port(&item.port)?;
            Some(ScrapedProxy {
                source: "vakhov".to_string(),
                host: item.ip,
                port,
                r#type: "http".to_string(),
                country_code: item.country_code.filter(|c| !c.is_empty()),
                username: None,
                password: None,
                priority: 0,
            })
        })
        .collect();
    Ok(list)
}

#[derive(serde::Deserialize)]
struct GProxyNetItem {
    proxy: String,
    protocol: Option<String>,
    country: Option<String>,
}

async fn sync_gproxynet() -> crate::error::Result<Vec<ScrapedProxy>> {
    let items: Vec<GProxyNetItem> = fetch_upstream_json(
        "https://raw.githubusercontent.com/gproxynet/free-proxy-list/main/proxies.json",
        "GProxyNet",
    )
    .await?;

    let list = items
        .into_iter()
        .filter_map(|item| {
            let (host, port) = parse_proxy_host_port(&item.proxy)?;
            let proto = item.protocol.unwrap_or_else(|| "http".to_string());
            Some(ScrapedProxy {
                source: "gproxynet".to_string(),
                host,
                port,
                r#type: proto.to_lowercase(),
                country_code: item.country.filter(|c| !c.is_empty()),
                username: None,
                password: None,
                priority: 0,
            })
        })
        .collect();
    Ok(list)
}

pub struct BuiltinProxySourceDef {
    pub id: &'static str,
    pub name: &'static str,
    pub url: &'static str,
    pub scraped_sources: &'static [&'static str],
    pub sync_fn:
        fn() -> futures::future::BoxFuture<'static, crate::error::Result<Vec<ScrapedProxy>>>,
}

impl BuiltinProxySourceDef {
    pub fn find_by_id(id: &str) -> Option<&'static BuiltinProxySourceDef> {
        BUILTIN_PROXY_SOURCES.iter().find(|s| s.id == id)
    }
}

pub static BUILTIN_PROXY_SOURCES: &[BuiltinProxySourceDef] = &[
    BuiltinProxySourceDef {
        id: "builtin_proxifly",
        name: "Proxifly (Built-in)",
        url: "",
        scraped_sources: &["proxifly"],
        sync_fn: || Box::pin(sync_proxifly()),
    },
    BuiltinProxySourceDef {
        id: "builtin_github",
        name: "GitHub Lists (Built-in)",
        url: "",
        scraped_sources: &[
            "iplocate",
            "hideip",
            "r00tee",
            "hookzof",
            "anonymouswork",
            "komutan234",
            "yuceltoluyag",
        ],
        sync_fn: || Box::pin(sync_github_lists()),
    },
    BuiltinProxySourceDef {
        id: "builtin_oneproxy",
        name: "1proxy (Built-in)",
        url: "",
        scraped_sources: &["1proxy"],
        sync_fn: || Box::pin(sync_oneproxy()),
    },
    BuiltinProxySourceDef {
        id: "builtin_proxyscrape",
        name: "ProxyScrape (Built-in)",
        url: "",
        scraped_sources: &["proxyscrape_cdn"],
        sync_fn: || Box::pin(sync_proxyscrape_cdn()),
    },
    BuiltinProxySourceDef {
        id: "builtin_geonode",
        name: "Geonode (Built-in)",
        url: "",
        scraped_sources: &["geonode"],
        sync_fn: || Box::pin(sync_geonode()),
    },
    BuiltinProxySourceDef {
        id: "builtin_clearproxy",
        name: "ClearProxy (Built-in)",
        url: "",
        scraped_sources: &["clearproxy"],
        sync_fn: || Box::pin(sync_clearproxy()),
    },
    BuiltinProxySourceDef {
        id: "builtin_vakhov",
        name: "Vakhov (Built-in)",
        url: "",
        scraped_sources: &["vakhov"],
        sync_fn: || Box::pin(sync_vakhov()),
    },
    BuiltinProxySourceDef {
        id: "builtin_gproxynet",
        name: "GProxyNet (Built-in)",
        url: "",
        scraped_sources: &["gproxynet"],
        sync_fn: || Box::pin(sync_gproxynet()),
    },
];

fn resolve_scraped_sources<'a>(is_builtin: bool, id: &str, name: &'a str) -> Vec<&'a str> {
    if is_builtin && let Some(def) = BuiltinProxySourceDef::find_by_id(id) {
        return def.scraped_sources.to_vec();
    }
    vec![name]
}

fn row_to_proxy_source(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProxySource> {
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
}

fn fetch_proxy_source_stats(
    conn: &Connection,
) -> crate::error::Result<std::collections::HashMap<String, Vec<(String, i64)>>> {
    let mut stats_stmt = conn
        .prepare("SELECT source, status, COUNT(*) FROM free_proxies GROUP BY source, status")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
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
            source: Some(std::sync::Arc::new(e)),
        })?;
    for row in stats_rows.flatten() {
        stats
            .entry(row.0)
            .or_insert_with(Vec::new)
            .push((row.1, row.2));
    }
    Ok(stats)
}

fn apply_source_stats(
    r: &mut ProxySource,
    stats: &std::collections::HashMap<String, Vec<(String, i64)>>,
) {
    let sources = resolve_scraped_sources(r.is_builtin, &r.id, &r.name);
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
}

pub fn list_proxy_sources(conn: &Connection) -> crate::error::Result<Vec<ProxySource>> {
    let mut stmt = conn
        .prepare("SELECT id, name, url, priority, active, is_builtin, created_at, updated_at FROM proxy_sources ORDER BY priority DESC, name ASC")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;

    let stats = fetch_proxy_source_stats(conn)?;
    let rows = stmt
        .query_map([], row_to_proxy_source)
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;

    let mut result = Vec::new();
    for row_res in rows {
        let mut r = row_res.map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;
        apply_source_stats(&mut r, &stats);
        result.push(r);
    }
    Ok(result)
}

pub fn get_proxy_source(conn: &Connection, id: &str) -> crate::error::Result<Option<ProxySource>> {
    use rusqlite::OptionalExtension;
    let mut stmt = conn
        .prepare("SELECT id, name, url, priority, active, is_builtin, created_at, updated_at FROM proxy_sources WHERE id = ?1")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;

    let res = stmt
        .query_row(rusqlite::params![id], row_to_proxy_source)
        .optional()
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
        })?;

    let Some(mut r) = res else {
        return Ok(None);
    };
    let stats = fetch_proxy_source_stats(conn)?;
    apply_source_stats(&mut r, &stats);
    Ok(Some(r))
}

pub fn create_proxy_source(
    conn: &Connection,
    input: &CreateProxySourceInput,
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
        source: Some(std::sync::Arc::new(e)),
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
        source: Some(std::sync::Arc::new(e)),
    })?;

    get_proxy_source(conn, id)?.ok_or_else(|| crate::error::CoreError::NotFound {
        what: "proxy_source".to_string(),
        id: id.to_string(),
    })
}

pub use openproxy_db::free_proxies::*;

fn parse_custom_proxy_auth(auth_part: Option<&str>) -> (Option<String>, Option<String>) {
    match auth_part {
        Some(auth) => match auth.split_once(':') {
            Some((u, p)) => (Some(u.trim().to_string()), Some(p.trim().to_string())),
            None => (Some(auth.trim().to_string()), None),
        },
        None => (None, None),
    }
}

fn parse_custom_proxy_line(line: &str, source_name: &str, priority: i32) -> Option<ScrapedProxy> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }

    let (proto, host_port) = trimmed.split_once("://").unwrap_or(("http", trimmed));
    let (raw_host, rest) = host_port.split_once(':')?;
    let (raw_port, auth_part) = rest
        .split_once(':')
        .map_or((rest, None), |(p, a)| (p, Some(a)));

    let host = raw_host.trim().to_string();
    let port = raw_port.trim().parse::<u16>().ok()?;
    if host.is_empty() || port == 0 {
        return None;
    }

    let (username, password) = parse_custom_proxy_auth(auth_part);
    Some(ScrapedProxy {
        source: source_name.to_string(),
        host,
        port,
        r#type: proto.to_lowercase(),
        country_code: None,
        username,
        password,
        priority,
    })
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
            crate::error::CoreError::Internal(format!("Custom proxy source HTTP error: {e:?}"))
        })?;

    if res.status != 200 {
        return Err(crate::error::CoreError::Internal(format!(
            "Custom proxy source HTTP status: {}",
            res.status
        )));
    }

    let body_bytes = res.collect().await.map_err(|e| {
        crate::error::CoreError::Internal(format!("Custom proxy source body error: {e:?}"))
    })?;
    let text = String::from_utf8_lossy(&body_bytes);
    let list = text
        .lines()
        .filter_map(|l| parse_custom_proxy_line(l, source_name, priority))
        .collect();

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

async fn sync_single_source(
    src: &ProxySource,
    errors: &mut Vec<String>,
    scraped: &mut Vec<ScrapedProxy>,
    fetched: &mut usize,
) {
    if !src.active {
        return;
    }
    if src.is_builtin {
        if let Some(def) = BuiltinProxySourceDef::find_by_id(&src.id)
            && let Ok(mut list) = (def.sync_fn)().await
        {
            *fetched += list.len();
            scraped.append(&mut list);
        }
        return;
    }

    match fetch_custom_proxy_source(&src.name, &src.url, src.priority).await {
        Ok(mut list) => {
            *fetched += list.len();
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

pub async fn sync_all_providers(db_pool: Arc<DbPool>) -> crate::error::Result<SyncSummary> {
    let mut errors = Vec::new();
    let mut fetched = 0;
    let mut scraped = Vec::new();

    let pool_for_sources = Arc::clone(&db_pool);
    let sources_res = tokio::task::spawn_blocking(move || -> crate::error::Result<_> {
        let w = pool_for_sources.open_connection().map_err(openproxy_db::error::map_db_error)?;
        // Ensure built-in sources exist
        for def in BUILTIN_PROXY_SOURCES {
            let _ = w.execute(
                "INSERT OR IGNORE INTO proxy_sources (id, name, url, active, is_builtin) VALUES (?1, ?2, ?3, 1, 1)",
                rusqlite::params![def.id, def.name, def.url],
            );
        }
        list_proxy_sources(&w)
    })
    .await;

    if let Ok(Ok(custom_sources)) = sources_res {
        for src in custom_sources {
            sync_single_source(&src, &mut errors, &mut scraped, &mut fetched).await;
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
        format!("{type}://{u}:{p}@{host}:{port}")
    } else {
        format!("{type}://{host}:{port}")
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
        Err(e) => Err(format!("Connection probe failed: {e:?}")),
    }
}

type ParsedProxyTuple = (String, String, u16, Option<String>, Option<String>);

fn fetch_proxy_test_target(
    conn: &Connection,
    id: &str,
) -> crate::error::Result<ParsedProxyTuple> {
    let mut stmt = conn
        .prepare("SELECT type, host, port, username, password FROM free_proxies WHERE id = ?1")
        .map_err(|e| crate::error::CoreError::Database {
            message: e.to_string(),
            source: Some(std::sync::Arc::new(e)),
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
        source: Some(std::sync::Arc::new(e)),
    })
}

fn apply_single_proxy_test_result(
    conn: &Connection,
    id: &str,
    test_res: Result<i64, String>,
) -> crate::error::Result<()> {
    match test_res {
        Ok(latency) => update_proxy_status(conn, id, "alive", Some(latency)),
        Err(_) => update_proxy_status(conn, id, "dead", None),
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
        fetch_proxy_test_target(&r, id)?
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
    apply_single_proxy_test_result(&w, id, test_res)?;

    let p = get_proxy(&w, id)?.ok_or_else(|| crate::error::CoreError::NotFound {
        what: "proxy".to_string(),
        id: id.to_string(),
    })?;

    Ok(p)
}

type ProxyTestCandidate = (String, String, String, u16, Option<String>, Option<String>);

fn fetch_background_test_proxies(conn: &Connection) -> Vec<ProxyTestCandidate> {
    let mut stmt = match conn.prepare(
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
            return Vec::new();
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
            return Vec::new();
        }
    };
    rows.flatten().collect()
}

fn execute_proxy_batch_update(
    conn: &mut Connection,
    batch: Vec<(String, Result<i64, String>)>,
) -> Result<(), crate::error::CoreError> {
    let tx_db = conn.transaction().map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(std::sync::Arc::new(e)),
    })?;
    let now = chrono::Utc::now().to_rfc3339();
    {
        let mut stmt = tx_db
            .prepare_cached(
                "UPDATE free_proxies SET status = ?1, latency_ms = ?2, last_validated = ?3, updated_at = ?4 WHERE id = ?5",
            )
            .map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(std::sync::Arc::new(e)),
    })?;

        for (id, test_res) in batch {
            let (status, latency) = match test_res {
                Ok(lat) => ("alive", Some(lat)),
                Err(_) => ("dead", None),
            };
            let _ = stmt.execute(rusqlite::params![status, latency, now, now, id]);
        }
    }
    tx_db.commit().map_err(|e| crate::error::CoreError::Database {
        message: e.to_string(),
        source: Some(std::sync::Arc::new(e)),
    })?;
    Ok(())
}

pub fn test_all_proxies_background(db_pool: Arc<DbPool>) {
    tokio::spawn(async move {
        let proxies = {
            let r = db_pool.reader();
            fetch_background_test_proxies(&r)
        };

        if proxies.is_empty() {
            return;
        }

        let test_url = {
            let r = db_pool.reader();
            openproxy_db::app_config::load_proxy_test_url(&r)
                .unwrap_or_else(|_| openproxy_db::app_config::PROXY_TEST_URL_DEFAULT.to_string())
        };

        let (tx, mut rx) = tokio::sync::mpsc::channel::<(String, Result<i64, String>)>(100);
        let pool_writer = Arc::clone(&db_pool);

        let writer_handle = tokio::spawn(async move {
            while let Some(first) = rx.recv().await {
                let mut batch = vec![first];
                while batch.len() < 50 {
                    match rx.try_recv() {
                        Ok(item) => batch.push(item),
                        Err(_) => break,
                    }
                }

                let pool = Arc::clone(&pool_writer);
                let _ = tokio::task::spawn_blocking(move || -> Result<(), crate::error::CoreError> {
                    let mut w = pool.open_connection()?;
                    execute_proxy_batch_update(&mut w, batch)
                })
                .await;
            }
        });

        let test_url_ref = &test_url;
        futures::stream::iter(proxies)
            .for_each_concurrent(20, |(id, r#type, host, port, username, password)| {
                let tx = tx.clone();
                async move {
                    let test_res = test_proxy_connection(
                        test_url_ref,
                        &r#type,
                        &host,
                        port,
                        username.as_deref(),
                        password.as_deref(),
                    )
                    .await;
                    let _ = tx.send((id, test_res)).await;
                }
            })
            .await;

        drop(tx);
        let _ = writer_handle.await;
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
              favicon_base64 TEXT,
              created_at TEXT NOT NULL DEFAULT (datetime('now')),
              updated_at TEXT NOT NULL DEFAULT (datetime('now')),
              CHECK (format IN ('openai', 'anthropic', 'mixed', 'gemini', 'responses'))
            );
            CREATE TABLE accounts (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              provider_id TEXT NOT NULL,
              current_proxy_id TEXT
            );
            CREATE TABLE provider_proxy_cooldowns (
              provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
              proxy_id TEXT NOT NULL REFERENCES free_proxies(id) ON DELETE CASCADE,
              cooldown_until TEXT NOT NULL,
              created_at TEXT NOT NULL DEFAULT (datetime('now')),
              PRIMARY KEY (provider_id, proxy_id)
            );
            ",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_crud_custom_proxy() {
        let conn = setup_test_db();

        let p = add_custom_proxy(&conn, "1.2.3.4", 8080, "http", Some("US"), None, None).unwrap();
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
        let p = add_custom_proxy(&conn, "1.2.3.4", 8080, "socks5", None, None, None).unwrap();
        update_proxy_status(&conn, &p.id, "alive", Some(100)).unwrap();

        // Now it should assign and return this socks5 proxy!
        let proxy = get_or_assign_provider_proxy(&conn, &provider_id, None).unwrap();
        assert_eq!(proxy, Some("socks5://1.2.3.4:8080".to_string()));

        // Add 15m cooldown for this proxy on test-provider
        openproxy_db::cooldowns::add_provider_proxy_cooldown(
            &conn,
            "test-provider",
            &p.id,
            std::time::Duration::from_mins(15),
        )
        .unwrap();

        // When in cooldown and no alternative alive proxy exists, should return Err (not spam cooldown proxy)
        assert!(get_or_assign_provider_proxy(&conn, &provider_id, None).is_err());

        // Add a second alive proxy
        let p2 = add_custom_proxy(&conn, "5.6.7.8", 9090, "http", None, None, None).unwrap();
        update_proxy_status(&conn, &p2.id, "alive", Some(120)).unwrap();

        // Now it should assign and return p2
        let proxy_after_cooldown = get_or_assign_provider_proxy(&conn, &provider_id, None).unwrap();
        assert_eq!(
            proxy_after_cooldown,
            Some("http://5.6.7.8:9090".to_string())
        );

        // The provider's current_proxy_id should now be bound to p2.id
        let bound_id: Option<String> = conn
            .query_row(
                "SELECT current_proxy_id FROM providers WHERE id = ?1",
                rusqlite::params![provider_id.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(bound_id.as_deref(), Some(p2.id.as_str()));

        // Calling it again should return the same cached proxy
        let proxy2 = get_or_assign_provider_proxy(&conn, &provider_id, None).unwrap();
        assert_eq!(proxy2, Some("http://5.6.7.8:9090".to_string()));

        // 4. Mark p2 as dead / inactive
        update_proxy_status(&conn, &p2.id, "dead", Some(9999)).unwrap();

        // Since p2 is dead and p1 is still in cooldown, should return Err
        assert!(get_or_assign_provider_proxy(&conn, &provider_id, None).is_err());
    }

    #[test]
    fn test_get_candidate_proxies_for_provider() {
        let mut conn = Connection::open_in_memory().unwrap();
        openproxy_db::migrations::run(&mut conn).unwrap();

        let provider_id = openproxy_types::ids::ProviderId::new("zen-provider");
        crate::providers::create(
            &conn,
            crate::providers::NewProvider {
                id: &provider_id,
                name: "Zen Provider",
                base_url: "https://api.example.com",
                auth_type: openproxy_types::providers::AuthType::None,
                format: openproxy_types::providers::ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            },
        )
        .unwrap();
        conn.execute(
            "UPDATE providers SET use_proxies = 1 WHERE id = 'zen-provider'",
            [],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO free_proxies (id, source, host, port, type, status, latency_ms) VALUES ('p1', 'test', '1.1.1.1', 8080, 'socks5', 'alive', 10)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO free_proxies (id, source, host, port, type, username, password, status, latency_ms) VALUES ('p2', 'test', '2.2.2.2', 8080, 'http', 'u', 'p', 'alive', 20)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO free_proxies (id, source, host, port, type, status, latency_ms) VALUES ('p3', 'test', '3.3.3.3', 8080, 'http', 'alive', 30)",
            [],
        )
        .unwrap();

        let candidates = get_candidate_proxies_for_provider(&conn, &provider_id, 2).unwrap();
        assert_eq!(candidates.len(), 2);

        // Put p1 on cooldown
        openproxy_db::cooldowns::add_provider_proxy_cooldown(
            &conn,
            provider_id.as_str(),
            "p1",
            std::time::Duration::from_secs(3600),
        )
        .unwrap();

        let candidates_after_cd =
            get_candidate_proxies_for_provider(&conn, &provider_id, 3).unwrap();
        assert_eq!(candidates_after_cd.len(), 2);
        assert!(!candidates_after_cd.iter().any(|(id, _)| id == "p1"));
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

        let p = add_custom_proxy(&conn, "1.2.3.4", 8080, "socks5", None, None, None).unwrap();

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
            &CreateProxySourceInput {
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
