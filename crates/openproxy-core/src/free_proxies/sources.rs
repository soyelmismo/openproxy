//! Database operations for proxy sources.

use super::builtin::BuiltinProxySourceDef;
use super::models::{CreateProxySourceInput, ProxySource, UpdateProxySourceInput};
use rusqlite::Connection;

pub fn resolve_scraped_sources<'a>(is_builtin: bool, id: &str, name: &'a str) -> Vec<&'a str> {
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
    let rows =
        stmt.query_map([], row_to_proxy_source)
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

    get_proxy_source(conn, &id)?
        .ok_or_else(|| crate::error::CoreError::not_found("proxy_source", id))
}

pub fn update_proxy_source(
    conn: &Connection,
    id: &str,
    input: UpdateProxySourceInput,
) -> crate::error::Result<ProxySource> {
    let existing = get_proxy_source(conn, id)?
        .ok_or_else(|| crate::error::CoreError::not_found("proxy_source", id))?;

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

    get_proxy_source(conn, id)?
        .ok_or_else(|| crate::error::CoreError::not_found("proxy_source", id))
}
