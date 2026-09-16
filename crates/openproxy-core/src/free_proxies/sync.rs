//! Synchronization of proxy sources with the database.

use super::builtin::{BUILTIN_PROXY_SOURCES, BuiltinProxySourceDef};
use super::crud::upsert_scraped_proxies;
use super::models::{ProxySource, SHARED_PROXY_CLIENT, ScrapedProxy, SyncSummary};
use super::scrapers::parse_custom_proxy_line;
use super::sources::list_proxy_sources;
use openproxy_db::DbPool;
use std::sync::Arc;

pub async fn fetch_custom_proxy_source(
    source_name: &str,
    url: &str,
    priority: i32,
) -> crate::error::Result<Vec<ScrapedProxy>> {
    use openproxy_adapters::upstream::{TimeoutProfile, UpstreamRequest, is_private_or_reserved};

    // SSRF Mitigation
    let uri: axum::http::Uri = url.parse().map_err(|e| {
        crate::error::CoreError::Internal(format!("Invalid URL for custom proxy source: {e}"))
    })?;

    match uri.scheme_str() {
        Some("http" | "https") => {}
        _ => {
            return Err(crate::error::CoreError::Internal(
                "Custom proxy source URL scheme must be http or https".to_string(),
            ));
        }
    }

    let host = uri.host().ok_or_else(|| {
        crate::error::CoreError::Internal("Custom proxy source URL missing host".to_string())
    })?;

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if is_private_or_reserved(&ip) {
            return Err(crate::error::CoreError::Internal(
                "SSRF Block: Custom proxy source host resolves to a private or reserved IP"
                    .to_string(),
            ));
        }
    } else if let Ok(mut addrs) = tokio::net::lookup_host((host, 0)).await {
        for addr in addrs.by_ref() {
            if is_private_or_reserved(&addr.ip()) {
                return Err(crate::error::CoreError::Internal(
                    "SSRF Block: Custom proxy source host resolves to a private or reserved IP"
                        .to_string(),
                ));
            }
        }
    } else {
        return Err(crate::error::CoreError::Internal(
            "Failed to resolve host for custom proxy source".to_string(),
        ));
    }

    let client = &*SHARED_PROXY_CLIENT;
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
        let Some(def) = BuiltinProxySourceDef::find_by_id(&src.id) else {
            return;
        };
        match (def.sync_fn)(def.url).await {
            Ok(mut list) => {
                *fetched += list.len();
                scraped.append(&mut list);
            }
            Err(e) => errors.push(format!(
                "Built-in proxy source '{}' sync failed: {}",
                src.name, e
            )),
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
