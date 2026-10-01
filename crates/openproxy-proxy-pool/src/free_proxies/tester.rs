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

pub const CANDIDATE_BATCH_LIMIT: usize = 100;

fn fetch_background_test_proxies(conn: &Connection) -> Vec<ProxyTestCandidate> {
    fetch_background_test_proxies_with_limit(conn, CANDIDATE_BATCH_LIMIT)
}

pub(crate) fn fetch_background_test_proxies_with_limit(
    conn: &Connection,
    limit: usize,
) -> Vec<ProxyTestCandidate> {
    let mut stmt = match conn.prepare(
        "
        SELECT id, type, host, port, username, password FROM free_proxies 
        ORDER BY 
            CASE 
                WHEN status = 'alive' AND last_validated IS NOT NULL AND datetime(last_validated) < datetime('now', '-30 minutes') THEN 1
                WHEN status = 'unknown' THEN 2
                WHEN status = 'alive' AND last_validated IS NULL THEN 3
                WHEN status = 'dead' AND last_validated IS NOT NULL AND datetime(last_validated) < datetime('now', '-2 hours') THEN 4
                WHEN status = 'alive' THEN 5
                ELSE 6
            END ASC,
            priority DESC,
            CASE WHEN last_validated IS NULL THEN 0 ELSE 1 END ASC,
            datetime(last_validated) ASC
        LIMIT ?1
    ",
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("Failed to prepare list query in background test: {}", e);
            return Vec::new();
        }
    };
    let rows = match stmt.query_map(rusqlite::params![limit as i64], |row| {
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

static TESTING_ACTIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct ActiveGuard;
impl Drop for ActiveGuard {
    fn drop(&mut self) {
        TESTING_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
    }
}

pub async fn test_all_proxies(db_pool: Arc<DbPool>) {
    if TESTING_ACTIVE
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
        )
        .is_err()
    {
        tracing::debug!(
            "test_all_proxies: validation pass already running, skipping duplicate run"
        );
        return;
    }

    let _guard = ActiveGuard;

    const READER_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    let initial = {
        let pool = Arc::clone(&db_pool);
        tokio::task::spawn_blocking(move || -> crate::error::Result<_> {
            let r = pool.try_reader_for(READER_LOCK_TIMEOUT).ok_or_else(|| {
                crate::error::CoreError::Internal(format!(
                    "test_all_proxies: reader lock not acquired within {READER_LOCK_TIMEOUT:?}"
                ))
            })?;
            let proxies = fetch_background_test_proxies(&r);
            let test_url = openproxy_db::app_config::load_proxy_test_url(&r)
                .unwrap_or_else(|_| openproxy_db::app_config::PROXY_TEST_URL_DEFAULT.to_string());
            Ok((proxies, test_url))
        })
        .await
    };
    let (proxies, test_url) = match initial {
        Ok(Ok(pair)) => pair,
        Ok(Err(e)) => {
            tracing::error!("test_all_proxies: initial load failed: {e}");
            return;
        }
        Err(e) => {
            tracing::error!("test_all_proxies: spawn_blocking join: {e}");
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
            let _ = tokio::task::spawn_blocking(move || -> Result<(), crate::error::CoreError> {
                let mut w = pool.writer();
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
}

pub fn test_all_proxies_background(db_pool: Arc<DbPool>) {
    tokio::spawn(test_all_proxies(db_pool));
}

pub(crate) fn build_probe_request(test_url: &str, proxy_url: String) -> UpstreamRequest {
    let mut req = UpstreamRequest::get(test_url);
    req.proxy = Some(proxy_url);
    req.headers.insert(
        http::header::CONNECTION,
        http::HeaderValue::from_static("close"),
    );
    req
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_probe_request_sets_connection_close() {
        let req = build_probe_request(
            "http://example.com/generate_204",
            "http://1.2.3.4:8080".to_string(),
        );
        assert_eq!(req.proxy.as_deref(), Some("http://1.2.3.4:8080"));
        assert_eq!(
            req.headers.get(http::header::CONNECTION),
            Some(&http::HeaderValue::from_static("close"))
        );
    }

    #[test]
    fn test_fetch_background_test_proxies_with_limit() {
        let pool =
            openproxy_db::conn::DbPool::test_pool_with_prefix("openproxy-test-candidate-limit")
                .expect("test pool");
        let conn = pool.writer();

        for i in 0..15 {
            conn.execute(
                "INSERT INTO free_proxies (id, source, host, port, type, status, priority)
                 VALUES (?1, 'custom', ?2, ?3, 'http', 'unknown', ?4)",
                rusqlite::params![
                    format!("proxy-{i}"),
                    format!("10.0.0.{i}"),
                    8000 + i as u16,
                    i as i64,
                ],
            )
            .expect("insert proxy");
        }

        let candidates = fetch_background_test_proxies_with_limit(&conn, 5);
        assert_eq!(candidates.len(), 5);
        assert_eq!(candidates[0].0, "proxy-14");
        assert_eq!(candidates[1].0, "proxy-13");

        let candidates_10 = fetch_background_test_proxies_with_limit(&conn, 10);
        assert_eq!(candidates_10.len(), 10);
        assert_eq!(CANDIDATE_BATCH_LIMIT, 100);
    }

    #[test]
    fn test_fetch_background_test_proxies_pagination_over_150_rows() {
        let pool = openproxy_db::conn::DbPool::test_pool_with_prefix(
            "openproxy-test-candidate-150-pagination",
        )
        .expect("test pool");
        let conn = pool.writer();

        for i in 0..165 {
            let status = match i % 3 {
                0 => "unknown",
                1 => "alive",
                _ => "dead",
            };
            conn.execute(
                "INSERT INTO free_proxies (id, source, host, port, type, status, priority)
                 VALUES (?1, 'custom', ?2, ?3, 'http', ?4, ?5)",
                rusqlite::params![
                    format!("proxy-{i:03}"),
                    format!("10.0.0.{}", i % 250),
                    8000 + (i as u16),
                    status,
                    i as i64,
                ],
            )
            .expect("insert proxy");
        }

        let total_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM free_proxies", [], |r| r.get(0))
            .expect("count");
        assert_eq!(
            total_count, 165,
            "Total inserted candidate rows must be 165 (>150)"
        );

        let candidates = fetch_background_test_proxies(&conn);
        assert_eq!(
            candidates.len(),
            100,
            "fetch_background_test_proxies must return exactly 100 rows when >150 exist"
        );

        // De 165 items: los 55 primeros deben quedar unknown.
        for item in &candidates[0..55] {
            let id = &item.0;
            let status: String = conn
                .query_row("SELECT status FROM free_proxies WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .expect("status");
            assert_eq!(status, "unknown");
        }
        for item in &candidates[55..100] {
            let id = &item.0;
            let status: String = conn
                .query_row("SELECT status FROM free_proxies WHERE id = ?1", [id], |r| {
                    r.get(0)
                })
                .expect("status");
            assert_eq!(status, "alive");
        }
    }

    #[tokio::test]
    async fn test_concurrent_writer_stress_from_tester_sync_and_runner() {
        let pool = std::sync::Arc::new(
            openproxy_db::conn::DbPool::test_pool_with_prefix("openproxy-test-writer-stress")
                .expect("test pool"),
        );

        {
            let conn = pool.writer();
            for i in 0..50 {
                conn.execute(
                    "INSERT INTO free_proxies (id, source, host, port, type, status, priority)
                     VALUES (?1, 'init', ?2, ?3, 'http', 'unknown', ?4)",
                    rusqlite::params![
                        format!("proxy-stress-{i}"),
                        format!("192.168.1.{i}"),
                        9000 + i as u16,
                        i as i64,
                    ],
                )
                .expect("insert proxy");
            }
            conn.execute(
                "INSERT OR IGNORE INTO providers (id, name, format) VALUES ('test-provider', 'Test', 'openai')",
                [],
            )
            .expect("insert provider");
        }

        let mut tasks = Vec::new();

        // Patrón de tester.rs: batch update en spawn_blocking con writer()
        for t in 0..10 {
            let pool = std::sync::Arc::clone(&pool);
            tasks.push(tokio::spawn(async move {
                for iter in 0..5 {
                    let batch = vec![
                        (
                            format!("proxy-stress-{}", (t * 5 + iter) % 50),
                            Ok(150 + iter as i64),
                        ),
                        (
                            format!("proxy-stress-{}", (t * 5 + iter + 1) % 50),
                            Err("timeout".to_string()),
                        ),
                    ];
                    let pool_clone = std::sync::Arc::clone(&pool);
                    tokio::task::spawn_blocking(move || {
                        let mut w = pool_clone.writer();
                        execute_proxy_batch_update(&mut w, &batch).expect("tester batch update");
                    })
                    .await
                    .expect("join spawn_blocking");
                }
            }));
        }

        // Patrón de sync.rs: inserción de sources y upsert_scraped_proxies en spawn_blocking con writer()
        for s in 0..10 {
            let pool = std::sync::Arc::clone(&pool);
            tasks.push(tokio::spawn(async move {
                for iter in 0..5 {
                    let source_id = format!("source-{s}-{iter}");
                    let scraped = vec![crate::free_proxies::models::ScrapedProxy {
                        source: source_id.clone(),
                        host: format!("10.20.{s}.{iter}"),
                        port: 8080 + iter as u16,
                        r#type: "http".to_string(),
                        country_code: Some("US".to_string()),
                        username: None,
                        password: None,
                        priority: 5,
                    }];
                    let pool_clone = std::sync::Arc::clone(&pool);
                    tokio::task::spawn_blocking(move || {
                        let mut w = pool_clone.writer();
                        let _ = w.execute(
                            "INSERT OR IGNORE INTO proxy_sources (id, name, url, active, is_builtin) VALUES (?1, ?2, 'http://test.com', 1, 0)",
                            rusqlite::params![source_id, source_id],
                        );
                        let _ = crate::free_proxies::sources::list_proxy_sources(&w);
                        crate::free_proxies::crud::upsert_scraped_proxies(&mut w, &scraped)
                            .expect("upsert scraped proxies");
                    })
                    .await
                    .expect("join spawn_blocking");
                }
            }));
        }

        // Patrón de runner.rs: notifications y auto-activación en spawn_blocking con writer()
        for r in 0..10 {
            let pool = std::sync::Arc::clone(&pool);
            tasks.push(tokio::spawn(async move {
                for iter in 0..5 {
                    let err_msg = format!("discovery failed on runner {r} iter {iter}");
                    let pool_clone = std::sync::Arc::clone(&pool);
                    tokio::task::spawn_blocking(move || {
                        let notif_conn = pool_clone.writer();
                        let _ = crate::notifications::record_system(
                            &notif_conn,
                            crate::notifications::CODE_DISCOVERY_FAILED,
                            &err_msg,
                            Some("test-provider"),
                            None,
                        );
                    })
                    .await
                    .expect("join spawn_blocking");

                    let pool_clone = std::sync::Arc::clone(&pool);
                    tokio::task::spawn_blocking(move || {
                        let aa_conn = pool_clone.writer();
                        let provider_id =
                            openproxy_types::ids::ProviderId("test-provider".to_string());
                        let _ = crate::models::apply_auto_activation_with_retry(
                            &aa_conn,
                            &provider_id,
                            Some("gpt"),
                        );
                    })
                    .await
                    .expect("join spawn_blocking");
                }
            }));
        }

        // Lectores concurrentes: fetch de candidatos y conteos con reader()
        for _ in 0..10 {
            let pool = std::sync::Arc::clone(&pool);
            tasks.push(tokio::spawn(async move {
                for _ in 0..10 {
                    let pool_clone = std::sync::Arc::clone(&pool);
                    tokio::task::spawn_blocking(move || {
                        let r = pool_clone.reader();
                        let candidates = fetch_background_test_proxies(&r);
                        assert!(candidates.len() <= CANDIDATE_BATCH_LIMIT);
                        let count: i64 = r
                            .query_row("SELECT COUNT(*) FROM free_proxies", [], |row| row.get(0))
                            .expect("count query");
                        assert!(count >= 50);
                    })
                    .await
                    .expect("join spawn_blocking");
                }
            }));
        }

        // Espera las 40 tareas con timeout para detectar deadlocks al instante
        let timeout_res = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            futures::future::try_join_all(tasks),
        )
        .await;

        assert!(
            timeout_res.is_ok(),
            "Concurrent writer stress test timed out! Possible deadlock or connection leak."
        );
        let results = timeout_res.unwrap();
        assert!(
            results.is_ok(),
            "Task failed in concurrent stress test: {results:?}"
        );

        // Sin fugas: writer y readers deben acquiredse al instante
        let writer_guard = pool.try_writer_for(std::time::Duration::from_millis(200));
        assert!(
            writer_guard.is_some(),
            "Writer lock must be instantly acquirable after stress test (no connection leaks)"
        );
        drop(writer_guard);

        let reader_guard = pool.try_reader_for(std::time::Duration::from_millis(200));
        assert!(
            reader_guard.is_some(),
            "Reader lock must be instantly acquirable after stress test (no connection leaks)"
        );
        drop(reader_guard);

        // El pool de readers no debe crecer (sin agotamiento): la cantidad
        // automática es acotada, y el test la verifica acotada en vez de fijarla.
        assert!(
            (2..=8).contains(&pool.reader_count()),
            "Reader pool must stay bounded without leaking connections"
        );
        assert!(
            pool.reader_count() >= 2,
            "At least two readers are required"
        );
        let r0 = pool.reader_guard();
        let r1 = pool.reader_guard();
        let val0: i64 = r0
            .query_row("SELECT 1", [], |r| r.get(0))
            .expect("query r0");
        let val1: i64 = r1
            .query_row("SELECT 1", [], |r| r.get(0))
            .expect("query r1");
        assert_eq!(val0, 1);
        assert_eq!(val1, 1);
        drop(r0);
        drop(r1);

        // Integridad: existen notifications, proxies scrapeados y estados
        let w = pool.writer();
        let notif_count: i64 = w
            .query_row("SELECT COUNT(*) FROM notifications", [], |r| r.get(0))
            .expect("notif count");
        assert!(
            notif_count > 0,
            "System notifications must have been inserted"
        );

        let total_proxies: i64 = w
            .query_row("SELECT COUNT(*) FROM free_proxies", [], |r| r.get(0))
            .expect("proxy count");
        assert!(
            total_proxies >= 50,
            "Proxies must have been inserted and maintained"
        );
    }

    static TEST_GUARD_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn test_all_proxies_empty_db_and_guard_lifecycle() {
        let _lock = TEST_GUARD_LOCK.lock().await;
        let pool = std::sync::Arc::new(
            openproxy_db::conn::DbPool::test_pool_with_prefix("openproxy-test-guard-lifecycle")
                .expect("test pool"),
        );

        TESTING_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);

        // 1. Empty DB async returns cleanly without contacting network
        test_all_proxies(Arc::clone(&pool)).await;
        assert!(
            !TESTING_ACTIVE.load(std::sync::atomic::Ordering::SeqCst),
            "Guard must be dropped and TESTING_ACTIVE reset to false after empty DB pass"
        );

        // 2. Duplicate guard behavior: while active, duplicate invocation exits early
        TESTING_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        test_all_proxies(Arc::clone(&pool)).await;
        assert!(
            TESTING_ACTIVE.load(std::sync::atomic::Ordering::SeqCst),
            "TESTING_ACTIVE remains true as held by initial holder"
        );

        // 3. Drop guard resets state and subsequent pass runs deterministically
        TESTING_ACTIVE.store(false, std::sync::atomic::Ordering::SeqCst);
        test_all_proxies(Arc::clone(&pool)).await;
        assert!(
            !TESTING_ACTIVE.load(std::sync::atomic::Ordering::SeqCst),
            "TESTING_ACTIVE must be false upon successful completion"
        );
    }

    #[test]
    fn test_active_guard_drop_resets_flag() {
        let _lock = TEST_GUARD_LOCK.blocking_lock();
        TESTING_ACTIVE.store(true, std::sync::atomic::Ordering::SeqCst);
        {
            let _guard = ActiveGuard;
            assert!(TESTING_ACTIVE.load(std::sync::atomic::Ordering::SeqCst));
        }
        assert!(
            !TESTING_ACTIVE.load(std::sync::atomic::Ordering::SeqCst),
            "ActiveGuard::drop must reset TESTING_ACTIVE to false"
        );
    }
}
