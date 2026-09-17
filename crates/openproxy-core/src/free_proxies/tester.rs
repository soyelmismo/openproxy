//! Background and on-demand proxy probe and testing execution.

use super::crud::{get_proxy, update_proxy_status};
use super::models::{FreeProxy, SHARED_PROXY_CLIENT};
use futures::StreamExt;
use openproxy_adapters::upstream::{ResolvedTimeouts, TimeoutProfile, UpstreamRequest};
use openproxy_db::DbPool;
use rusqlite::Connection;
use std::sync::Arc;

pub async fn test_proxy_connection(
    test_url: &str,
    r#type: &str,
    host: &str,
    port: u16,
    username: Option<&str>,
    password: Option<&str>,
) -> Result<i64, String> {
    let proxy_url = if let (Some(u), Some(p)) = (username, password) {
        format!("{type}://{u}:{p}@{host}:{port}")
    } else {
        format!("{type}://{host}:{port}")
    };

    let client = &*SHARED_PROXY_CLIENT;
    let req = build_probe_request(test_url, proxy_url);

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

fn fetch_proxy_test_target(conn: &Connection, id: &str) -> crate::error::Result<ParsedProxyTuple> {
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
    const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    let id_owned = id.to_string();

    let (test_url, type_host_port_user_pass) = {
        let pool = Arc::clone(&db_pool);
        let id = id_owned.clone();
        tokio::task::spawn_blocking(move || -> crate::error::Result<_> {
            let r = pool.try_reader_for(LOCK_TIMEOUT).ok_or_else(|| {
                crate::error::CoreError::Internal(format!(
                    "test_single_proxy: reader lock not acquired within {LOCK_TIMEOUT:?}"
                ))
            })?;
            let test_url = openproxy_db::app_config::load_proxy_test_url(&r)
                .unwrap_or_else(|_| openproxy_db::app_config::PROXY_TEST_URL_DEFAULT.to_string());
            let type_host_port_user_pass = fetch_proxy_test_target(&r, &id)?;
            Ok((test_url, type_host_port_user_pass))
        })
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("spawn_blocking join: {e}")))??
    };

    let (r#type, host, port, username, password) = type_host_port_user_pass;

    let test_res = test_proxy_connection(
        &test_url,
        &r#type,
        &host,
        port,
        username.as_deref(),
        password.as_deref(),
    )
    .await;

    let pool = Arc::clone(&db_pool);
    let id = id_owned;
    let p = tokio::task::spawn_blocking(move || -> crate::error::Result<FreeProxy> {
        let w = pool.try_writer_for(LOCK_TIMEOUT).ok_or_else(|| {
            crate::error::CoreError::Internal(format!(
                "test_single_proxy: writer lock not acquired within {LOCK_TIMEOUT:?}"
            ))
        })?;
        apply_single_proxy_test_result(&w, &id, test_res)?;
        get_proxy(&w, &id)?.ok_or_else(|| crate::error::CoreError::NotFound {
            what: "proxy".to_string(),
            id,
        })
    })
    .await
    .map_err(|e| crate::error::CoreError::Internal(format!("spawn_blocking join: {e}")))??;

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
    batch: &[(String, Result<i64, String>)],
) -> Result<(), crate::error::CoreError> {
    openproxy_db::error::with_busy_retry("execute_proxy_batch_update", || {
        let tx_db = conn
            .transaction()
            .map_err(openproxy_db::error::map_db_error)?;
        let now = chrono::Utc::now().to_rfc3339();
        {
            let mut stmt = tx_db
                .prepare_cached(
                    "UPDATE free_proxies SET status = ?1, latency_ms = ?2, last_validated = ?3, updated_at = ?4 WHERE id = ?5",
                )
                .map_err(openproxy_db::error::map_db_error)?;

            for (id, test_res) in batch {
                let (status, latency) = match test_res {
                    Ok(lat) => ("alive", Some(*lat)),
                    Err(_) => ("dead", None),
                };
                let _ = stmt.execute(rusqlite::params![status, latency, now, now, id]);
            }
        }
        tx_db.commit().map_err(openproxy_db::error::map_db_error)?;
        Ok(())
    })
}

pub fn test_all_proxies_background(db_pool: Arc<DbPool>) {
    const READER_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    tokio::spawn(async move {
        let initial = {
            let pool = Arc::clone(&db_pool);
            tokio::task::spawn_blocking(move || -> crate::error::Result<_> {
                let r = pool.try_reader_for(READER_LOCK_TIMEOUT).ok_or_else(|| {
                    crate::error::CoreError::Internal(format!(
                        "test_all_proxies_background: reader lock not acquired within {READER_LOCK_TIMEOUT:?}"
                    ))
                })?;
                let proxies = fetch_background_test_proxies(&r);
                let test_url = openproxy_db::app_config::load_proxy_test_url(&r)
                    .unwrap_or_else(|_| {
                        openproxy_db::app_config::PROXY_TEST_URL_DEFAULT.to_string()
                    });
                Ok((proxies, test_url))
            })
            .await
        };
        let (proxies, test_url) = match initial {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => {
                tracing::error!("test_all_proxies_background: initial load failed: {e}");
                return;
            }
            Err(e) => {
                tracing::error!("test_all_proxies_background: spawn_blocking join: {e}");
                return;
            }
        };

        if proxies.is_empty() {
            return;
        }

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
                let _ =
                    tokio::task::spawn_blocking(move || -> Result<(), crate::error::CoreError> {
                        let mut w = pool.open_connection()?;
                        execute_proxy_batch_update(&mut w, &batch)
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

pub(crate) fn build_probe_request(test_url: &str, proxy_url: String) -> UpstreamRequest {
    let mut req = UpstreamRequest::get(test_url);
    req.proxy = Some(proxy_url);
    req.headers.insert(
        axum::http::header::CONNECTION,
        axum::http::HeaderValue::from_static("close"),
    );
    req
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_request_sets_connection_close() {
        let req = build_probe_request("http://example.com/generate_204", "http://1.2.3.4:8080".to_string());
        assert_eq!(req.proxy.as_deref(), Some("http://1.2.3.4:8080"));
        assert_eq!(
            req.headers.get(axum::http::header::CONNECTION),
            Some(&axum::http::HeaderValue::from_static("close"))
        );
    }
}
