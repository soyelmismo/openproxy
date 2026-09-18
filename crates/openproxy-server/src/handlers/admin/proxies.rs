use super::{ApiError, AppState, Arc, CoreError, Deserialize};
use crate::extractors::DbReader;
use axum::{
    Json,
    extract::{Path, Query, State},
};
use openproxy_adapters::upstream::is_private_or_reserved;

#[derive(Debug, Default, Deserialize)]
pub struct ListProxiesQuery {
    pub source: Option<String>,
    pub status: Option<String>,
    pub protocol: Option<String>,
    pub search: Option<String>,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
}

#[derive(Debug, Deserialize)]
pub struct CreateCustomProxyInput {
    pub host: String,
    pub port: u16,
    pub r#type: String,
    pub country_code: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route(
            "/",
            axum::routing::get(list_proxies).post(create_custom_proxy),
        )
        .route("/summary", axum::routing::get(get_proxy_summary))
        .route("/sync", axum::routing::post(sync_proxies))
        .route("/test-all", axum::routing::post(test_all_proxies))
        .route(
            "/test-url",
            axum::routing::get(get_proxy_test_url).put(update_proxy_test_url),
        )
        .route("/{id}/test", axum::routing::post(test_proxy))
        .route("/{id}", axum::routing::delete(delete_proxy))
}

pub async fn list_proxies(
    DbReader(r): DbReader,
    Query(query): Query<ListProxiesQuery>,
) -> Result<Json<Vec<openproxy_core::free_proxies::FreeProxy>>, ApiError> {
    let list = openproxy_core::free_proxies::list_proxies(
        &r,
        query.source.as_deref(),
        query.status.as_deref(),
        query.protocol.as_deref(),
        query.search.as_deref(),
        query.limit,
        query.offset,
    )?;
    Ok(Json(list))
}

pub async fn get_proxy_summary(
    DbReader(r): DbReader,
) -> Result<Json<openproxy_core::free_proxies::ProxySummary>, ApiError> {
    let summary = openproxy_core::free_proxies::get_proxy_summary(&r)?;
    Ok(Json(summary))
}

pub async fn sync_proxies(
    State(s): State<AppState>,
) -> Result<Json<openproxy_core::free_proxies::SyncSummary>, ApiError> {
    let summary = openproxy_core::free_proxies::sync_all_providers(Arc::clone(s.db_pool())).await?;
    Ok(Json(summary))
}

async fn validate_custom_proxy_input(body: &CreateCustomProxyInput) -> Result<(), ApiError> {
    let host_str = body.host.trim();
    let host_clean = host_str
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host_str);
    if host_clean.is_empty() || body.port == 0 {
        return Err(ApiError(CoreError::Validation(
            "host and port are required".into(),
        )));
    }
    if let Ok(ip) = host_clean.parse::<std::net::IpAddr>()
        && is_private_or_reserved(&ip)
    {
        return Err(ApiError(CoreError::Validation(format!(
            "host '{host_str}' resolves to a private/reserved IP and is not allowed"
        ))));
    }

    let addrs = tokio::net::lookup_host((host_clean, body.port))
        .await
        .map_err(|e| {
            ApiError(CoreError::Validation(format!(
                "failed to resolve host '{host_str}': {e}"
            )))
        })?;

    for addr in addrs {
        if is_private_or_reserved(&addr.ip()) {
            return Err(ApiError(CoreError::Validation(format!(
                "host '{host_str}' resolves to a private/reserved IP and is not allowed"
            ))));
        }
    }

    Ok(())
}

pub async fn create_custom_proxy(
    State(s): State<AppState>,
    Json(body): Json<CreateCustomProxyInput>,
) -> Result<Json<openproxy_core::free_proxies::FreeProxy>, ApiError> {
    validate_custom_proxy_input(&body).await?;

    let conn_arc = s.db_pool().writer_arc();
    let host = body.host.trim().to_string();
    let r#type = body.r#type.trim().to_string();
    let country_code = body
        .country_code
        .as_deref()
        .map(str::trim)
        .map(String::from);
    let username = body.username.as_deref().map(str::trim).map(String::from);
    let password = body.password.as_deref().map(str::trim).map(String::from);
    let port = body.port;

    let p = tokio::task::spawn_blocking(move || {
        let guard = conn_arc
            .try_lock_arc_for(std::time::Duration::from_secs(5))
            .ok_or_else(|| {
                openproxy_types::CoreError::Internal("writer lock timeout (5s)".into())
            })?;
        openproxy_core::free_proxies::add_custom_proxy(
            &guard,
            &host,
            port,
            &r#type,
            country_code.as_deref(),
            username.as_deref(),
            password.as_deref(),
        )
    })
    .await
    .map_err(|e| {
        ApiError(openproxy_types::CoreError::Internal(format!(
            "writer spawn failed: {e}"
        )))
    })?
    .map_err(ApiError)?;

    Ok(Json(p))
}

pub async fn test_proxy(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<openproxy_core::free_proxies::FreeProxy>, ApiError> {
    let p = openproxy_core::free_proxies::test_single_proxy(Arc::clone(s.db_pool()), &id).await?;
    Ok(Json(p))
}

pub async fn test_all_proxies(
    State(s): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    openproxy_core::free_proxies::test_all_proxies_background(Arc::clone(s.db_pool()));
    Ok(Json(serde_json::json!({ "status": "started" })))
}

crate::admin_entity_action_handler! {
    pub async fn delete_proxy(
        DbWriter(w): DbWriter,
        Path(id): Path<String>,
    ) -> Result<Json<serde_json::Value>, ApiError> {
        openproxy_core::free_proxies::delete_proxy(&w, &id)?;
        Ok(Json(serde_json::json!({ "status": "deleted" })))
    }
}

pub async fn get_proxy_test_url(
    DbReader(r): DbReader,
) -> Result<Json<serde_json::Value>, ApiError> {
    let url = openproxy_db::app_config::load_proxy_test_url(&r)?;
    Ok(Json(serde_json::json!({ "proxy_test_url": url })))
}

#[derive(serde::Deserialize)]
pub struct UpdateProxyTestUrlInput {
    pub proxy_test_url: String,
}

pub async fn update_proxy_test_url(
    State(s): State<AppState>,
    Json(body): Json<UpdateProxyTestUrlInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let url = body.proxy_test_url.trim();
    if url.is_empty() {
        return Err(ApiError(CoreError::Validation(
            "url cannot be empty".into(),
        )));
    }

    let parsed_url: axum::http::Uri = url
        .parse()
        .map_err(|_| ApiError(CoreError::Validation("invalid URL format".into())))?;

    match parsed_url.scheme_str() {
        Some("http" | "https") => {}
        _ => {
            return Err(ApiError(CoreError::Validation(
                "URL scheme must be http or https".into(),
            )));
        }
    }

    let host = parsed_url
        .host()
        .ok_or_else(|| ApiError(CoreError::Validation("URL must contain a host".into())))?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);

    let port = parsed_url
        .port_u16()
        .unwrap_or_else(|| match parsed_url.scheme_str() {
            Some("https") => 443,
            _ => 80,
        });

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if is_private_or_reserved(&ip) {
            return Err(ApiError(CoreError::Validation(format!(
                "url host '{host}' resolves to a private/reserved IP and is not allowed"
            ))));
        }
    } else {
        let addrs = tokio::net::lookup_host((host, port)).await.map_err(|e| {
            ApiError(CoreError::Validation(format!(
                "failed to resolve url host '{host}': {e}"
            )))
        })?;

        for addr in addrs {
            if is_private_or_reserved(&addr.ip()) {
                return Err(ApiError(CoreError::Validation(format!(
                    "url host '{host}' resolves to a private/reserved IP and is not allowed"
                ))));
            }
        }
    }

    let conn_arc = s.db_pool().writer_arc();
    let url_str = url.to_string();
    tokio::task::spawn_blocking(move || {
        let guard = conn_arc
            .try_lock_arc_for(std::time::Duration::from_secs(5))
            .ok_or_else(|| {
                openproxy_types::CoreError::Internal("writer lock timeout (5s)".into())
            })?;
        openproxy_db::app_config::save_proxy_test_url(&guard, &url_str)
    })
    .await
    .map_err(|e| {
        ApiError(openproxy_types::CoreError::Internal(format!(
            "writer spawn failed: {e}"
        )))
    })?
    .map_err(ApiError)?;

    Ok(Json(serde_json::json!({ "proxy_test_url": url })))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_test_state() -> (AppState, openproxy_db::testing::TempDir) {
        let temp_dir =
            openproxy_db::testing::TempDir::new("openproxy-proxies-test").expect("mkdir");
        let pool = std::sync::Arc::new(
            openproxy_db::DbPool::open(&temp_dir.path().join("test.db")).expect("open pool"),
        );
        {
            let mut w = pool.writer();
            openproxy_db::migrations::run(&mut w).expect("migrations");
        }
        let mk = openproxy_db::secrets::MasterKey::generate().unwrap();
        let adapters = std::sync::Arc::new(parking_lot::RwLock::new(std::sync::Arc::new(
            openproxy_adapters::adapters::builtin_adapters(),
        )));
        let state = AppState::for_test(
            openproxy_core::AppConfig::default(),
            pool,
            std::sync::Arc::new(mk),
            adapters,
        );
        (state, temp_dir)
    }

    #[tokio::test]
    async fn failed_dns_in_create_custom_proxy_does_not_block_sqlite_writer() {
        let (state, _temp_dir) = create_test_state().await;

        // Verify writer is free before call
        assert!(
            state
                .db_pool()
                .try_writer_for(std::time::Duration::from_millis(50))
                .is_some()
        );

        let input = CreateCustomProxyInput {
            host: "invalid.domain.that.does.not.exist.test.123456789".to_string(),
            port: 9999,
            r#type: "socks5".to_string(),
            country_code: None,
            username: None,
            password: None,
        };

        // Call create_custom_proxy which will fail at DNS lookup
        let res = create_custom_proxy(State(state.clone()), Json(input)).await;
        assert!(res.is_err(), "should fail DNS validation");

        // Verify writer lock was never held or locked by create_custom_proxy and is immediately acquirable
        let writer_guard = state
            .db_pool()
            .try_writer_for(std::time::Duration::from_millis(50));
        assert!(
            writer_guard.is_some(),
            "writer lock must be free immediately after DNS failure"
        );
    }

    #[tokio::test]
    async fn failed_dns_in_update_proxy_test_url_does_not_block_sqlite_writer() {
        let (state, _temp_dir) = create_test_state().await;

        let input = UpdateProxyTestUrlInput {
            proxy_test_url: "http://invalid.domain.that.does.not.exist.test.123456789:9999"
                .to_string(),
        };

        let res = update_proxy_test_url(State(state.clone()), Json(input)).await;
        assert!(res.is_err(), "should fail DNS validation");

        // Verify writer lock was never held or locked and is immediately acquirable
        let writer_guard = state
            .db_pool()
            .try_writer_for(std::time::Duration::from_millis(50));
        assert!(
            writer_guard.is_some(),
            "writer lock must be free immediately after DNS failure"
        );
    }

    #[tokio::test]
    async fn create_custom_proxy_persists_when_valid() {
        let (state, _temp_dir) = create_test_state().await;

        // Using a valid public IP skips DNS lookup and tests persistence path
        let input = CreateCustomProxyInput {
            host: "8.8.8.8".to_string(),
            port: 1080,
            r#type: "socks5".to_string(),
            country_code: Some("US".to_string()),
            username: None,
            password: None,
        };

        let res = create_custom_proxy(State(state.clone()), Json(input)).await;
        assert!(
            res.is_ok(),
            "valid proxy should persist cleanly: {:?}",
            res.err()
        );
        let proxy = res.unwrap().0;
        assert_eq!(proxy.host, "8.8.8.8");
        assert_eq!(proxy.port, 1080);
    }

    #[tokio::test]
    async fn stress_test_dns_failure_does_not_starve_sqlite_reads_or_writes() {
        let (state, _temp_dir) = create_test_state().await;
        let pool = std::sync::Arc::clone(state.db_pool());

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let write_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let read_count = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

        let stop_clone = std::sync::Arc::clone(&stop);
        let pool_clone = std::sync::Arc::clone(&pool);
        let write_count_clone = std::sync::Arc::clone(&write_count);
        let writer_handle = tokio::spawn(async move {
            while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                let start = std::time::Instant::now();
                let pool = std::sync::Arc::clone(&pool_clone);
                let res = pool
                    .spawn_write(|conn| {
                        conn.execute(
                            "INSERT OR REPLACE INTO app_config (key, value, updated_at) VALUES ('test_key', 'test_val', 1)",
                            [],
                        )
                        .map_err(|e| openproxy_types::CoreError::Database {
                            message: e.to_string(),
                            source: None,
                        })
                    })
                    .await;
                assert!(res.is_ok(), "SQLite write failed");
                assert!(
                    start.elapsed() < std::time::Duration::from_millis(1500),
                    "SQLite write was delayed: {:?}",
                    start.elapsed()
                );
                write_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        });

        let stop_clone = std::sync::Arc::clone(&stop);
        let pool_clone = std::sync::Arc::clone(&pool);
        let read_count_clone = std::sync::Arc::clone(&read_count);
        let reader_handle = tokio::spawn(async move {
            while !stop_clone.load(std::sync::atomic::Ordering::Relaxed) {
                let start = std::time::Instant::now();
                let pool = std::sync::Arc::clone(&pool_clone);
                let res = pool
                    .spawn_read(|conn| {
                        let val: String = conn
                            .query_row(
                                "SELECT value FROM app_config WHERE key = 'test_key'",
                                [],
                                |r| r.get(0),
                            )
                            .unwrap_or_else(|_| "default".into());
                        Ok(val)
                    })
                    .await;
                assert!(res.is_ok(), "SQLite read failed");
                assert!(
                    start.elapsed() < std::time::Duration::from_millis(1500),
                    "SQLite read was delayed: {:?}",
                    start.elapsed()
                );
                read_count_clone.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
            }
        });

        // Fire 20 concurrent admin proxy requests with invalid/failing DNS domains
        let mut dns_tasks = Vec::new();
        for i in 0..20 {
            let state = state.clone();
            dns_tasks.push(tokio::spawn(async move {
                let input = CreateCustomProxyInput {
                    host: format!("failing.dns.stress.test.{i}.invalid.corp"),
                    port: 8080,
                    r#type: "http".to_string(),
                    country_code: None,
                    username: None,
                    password: None,
                };
                let res = create_custom_proxy(State(state), Json(input)).await;
                assert!(res.is_err(), "DNS resolution must fail for fake domain");
            }));
        }

        for t in dns_tasks {
            t.await.unwrap();
        }

        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        writer_handle.await.unwrap();
        reader_handle.await.unwrap();

        let writes = write_count.load(std::sync::atomic::Ordering::Relaxed);
        let reads = read_count.load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            writes > 0,
            "must have completed writes during DNS lookup: {writes}"
        );
        assert!(
            reads > 0,
            "must have completed reads during DNS lookup: {reads}"
        );
    }

    #[tokio::test]
    async fn test_proxy_admin_ssrf_blocking_and_ip_edge_cases() {
        let (state, _temp_dir) = create_test_state().await;

        let blocked_hosts = [
            "127.0.0.1",
            "127.0.0.2",
            "localhost",
            "::1",
            "::ffff:127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "fc00::1",
            "fd00::1",
        ];

        for host in blocked_hosts {
            let input = CreateCustomProxyInput {
                host: host.to_string(),
                port: 8080,
                r#type: "http".to_string(),
                country_code: None,
                username: None,
                password: None,
            };
            let res = create_custom_proxy(State(state.clone()), Json(input)).await;
            assert!(
                res.is_err(),
                "Host '{host}' should have been BLOCKED by SSRF protection"
            );
        }
    }

    #[tokio::test]
    async fn test_proxy_test_url_edge_cases() {
        let (state, _temp_dir) = create_test_state().await;

        let invalid_urls = [
            "",                                   // empty
            "not-a-url",                          // no scheme
            "ftp://example.com/test",             // non-http/https
            "file:///etc/passwd",                 // file scheme
            "http:///path-without-host",          // no host
            "http://127.0.0.1:8080/test",         // loopback IPv4
            "http://169.254.169.254/latest/meta", // AWS metadata
            "http://10.1.2.3:8080/test",          // private IPv4
            "http://192.168.0.1/test",            // private IPv4
            "http://localhost:8080/test",         // loopback hostname
        ];

        for url in invalid_urls {
            let input = UpdateProxyTestUrlInput {
                proxy_test_url: url.to_string(),
            };
            let res = update_proxy_test_url(State(state.clone()), Json(input)).await;
            assert!(
                res.is_err(),
                "URL '{url}' must be rejected as invalid or SSRF-blocked"
            );
        }

        // Test IPv6 URL behavior in update_proxy_test_url
        let ipv6_loopback = UpdateProxyTestUrlInput {
            proxy_test_url: "http://[::1]:8080/test".to_string(),
        };
        let res_ipv6_loopback =
            update_proxy_test_url(State(state.clone()), Json(ipv6_loopback)).await;
        assert!(
            res_ipv6_loopback.is_err(),
            "http://[::1]:8080/test must be rejected"
        );

        // Test valid public IPv4 URL
        let valid_input = UpdateProxyTestUrlInput {
            proxy_test_url: "https://1.1.1.1/generate_204".to_string(),
        };
        let res_valid = update_proxy_test_url(State(state.clone()), Json(valid_input)).await;
        assert!(
            res_valid.is_ok(),
            "https://1.1.1.1/generate_204 should be accepted: {:?}",
            res_valid.err()
        );

        // Test valid public IPv6 URL
        let valid_ipv6 = UpdateProxyTestUrlInput {
            proxy_test_url: "https://[2606:4700:4700::1111]/generate_204".to_string(),
        };
        let res_valid_ipv6 = update_proxy_test_url(State(state.clone()), Json(valid_ipv6)).await;
        assert!(
            res_valid_ipv6.is_ok(),
            "https://[2606:4700:4700::1111]/generate_204 should be accepted: {:?}",
            res_valid_ipv6.err()
        );
    }
}
