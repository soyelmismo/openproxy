//! Background and on-demand proxy probe and testing execution.

use super::crud::get_proxy;
use super::models::{FreeProxy, SHARED_PROXY_CLIENT};
use futures::StreamExt;
use openproxy_adapters::upstream::{
    ResolvedTimeouts, TimeoutProfile, UpstreamError, UpstreamRequest, is_private_or_reserved,
};
use openproxy_db::DbPool;
use openproxy_types::proxy_endpoint::{canonical_host, canonical_scheme, valid_auth};
use rusqlite::Connection;
use std::sync::Arc;

/// Validate a proxy probe target URL (audit H-2).
///
/// The probe is sent THROUGH third-party proxies, so a probe target that
/// points at private/reserved space turns the whole free-proxy fleet into a
/// blind internal scanner. The save path (the admin handler) validates, but
/// `app_config` is restorable from a backup bundle — so the value is
/// validated again at EVERY use (defense in depth: a planted or
/// directly-DB-written URL never reaches the probe).
///
/// Checks: non-empty, http(s) scheme, host present, and the host is (or
/// resolves ONLY to) a public address.
pub async fn validate_probe_url(url: &str) -> std::result::Result<(), String> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err("url cannot be empty".into());
    }
    let parsed: http::Uri = trimmed
        .parse()
        .map_err(|_| "invalid URL format".to_string())?;
    match parsed.scheme_str() {
        Some("http" | "https") => {}
        _ => return Err("URL scheme must be http or https".into()),
    }
    let host = parsed
        .host()
        .ok_or_else(|| "URL must contain a host".to_string())?;
    let host = host
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host);

    let port = parsed.port_u16().unwrap_or(match parsed.scheme_str() {
        Some("https") => 443,
        _ => 80,
    });

    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        if is_private_or_reserved(&ip) {
            return Err(format!(
                "url host '{host}' is a private/reserved IP and is not allowed"
            ));
        }
        return Ok(());
    }
    let mut addrs = tokio::time::timeout(
        std::time::Duration::from_millis(1500),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| format!("DNS timeout for url host '{host}'"))?
    .map_err(|e| format!("failed to resolve url host '{host}': {e}"))?
    .peekable();
    if addrs.peek().is_none() {
        return Err(format!("no addresses for url host '{host}'"));
    }
    for addr in addrs {
        if is_private_or_reserved(&addr.ip()) {
            return Err(format!(
                "url host '{host}' resolves to a private/reserved IP and is not allowed"
            ));
        }
    }
    Ok(())
}

/// Validate a loaded probe target, mapping rejection to `None`.
async fn validated_probe_target(raw: &str) -> Option<String> {
    match validate_probe_url(raw).await {
        Ok(()) => Some(raw.to_string()),
        Err(reason) => {
            tracing::warn!(
                url = %raw,
                reason = %reason,
                "proxy probe target rejected (not a public http(s) host); skipping this round"
            );
            None
        }
    }
}

const FALLBACK_PROBE_URL: &str = "https://www.gstatic.com/generate_204";

type ProbeResult = Result<i64, ProbeFailure>;

#[derive(Debug, PartialEq, Eq)]
enum ProbeFailure {
    Invalid,
    Authentication,
    Timeout,
    Transport,
    Status(u16),
}

impl ProbeFailure {
    fn label(&self) -> &'static str {
        match self {
            Self::Invalid => "invalid_configuration",
            Self::Authentication => "authentication",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::Status(_) => "target_status",
        }
    }
}

async fn run_probe(test_url: &str, proxy_url: Option<&str>) -> ProbeResult {
    let client = &*SHARED_PROXY_CLIENT;
    let req = match proxy_url {
        Some(url) => build_probe_request(test_url, url.to_owned()),
        None => UpstreamRequest::get(test_url),
    };

    let profile = TimeoutProfile::Custom(ResolvedTimeouts {
        dns_ms: 1500,
        dial_ms: 2500,
        tls_ms: 2500,
        write_ms: 1500,
        headers_ms: 3500,
        body_chunk_ms: 1500,
        total_ms: 3500,
    });
    let cancel = openproxy_adapters::upstream::CancellationToken::new();

    let start = std::time::Instant::now();
    let res = client.call(req, profile, cancel).await;

    match res {
        Ok(r) => match r.status.as_u16() {
            200 | 204 => Ok(start.elapsed().as_millis() as i64),
            407 => Err(ProbeFailure::Authentication),
            code => Err(ProbeFailure::Status(code)),
        },
        Err(UpstreamError::Timeout(_)) => Err(ProbeFailure::Timeout),
        Err(UpstreamError::Invalid(_)) => Err(ProbeFailure::Invalid),
        Err(UpstreamError::Connection(msg)) if msg.contains("Proxy authentication:") => {
            Err(ProbeFailure::Authentication)
        }
        Err(_) => Err(ProbeFailure::Transport),
    }
}

async fn healthy_probe_targets(primary: &str) -> Vec<String> {
    let checks = [primary, FALLBACK_PROBE_URL];
    let results =
        futures::future::join_all(checks.into_iter().enumerate().map(|(i, url)| async move {
            if i == 1 && url == primary {
                return None;
            }
            // Direct controls prevent a public target outage from poisoning the pool.
            let url = validated_probe_target(url).await?;
            match run_probe(&url, None).await {
                Ok(_) => Some(url),
                Err(reason) => {
                    tracing::warn!(category = reason.label(), "Proxy probe control failed");
                    None
                }
            }
        }))
        .await;
    results.into_iter().flatten().collect()
}

async fn test_proxy_candidate(targets: &[String], proxy: &ParsedProxyTuple) -> ProbeResult {
    let (scheme, host, port, username, password, status) = proxy;
    let scheme = canonical_scheme(scheme).ok_or(ProbeFailure::Invalid)?;
    let host = canonical_host(host)
        .filter(|_| *port != 0)
        .ok_or(ProbeFailure::Invalid)?;
    if !valid_auth(scheme, username.as_deref(), password.as_deref()) {
        return Err(ProbeFailure::Invalid);
    }
    let proxy_url = openproxy_db::free_proxies::format_proxy_url(
        scheme,
        &host,
        i64::from(*port),
        username.as_deref(),
        password.as_deref(),
    );
    let first = run_probe(&targets[0], Some(&proxy_url)).await;
    let retry = matches!(first, Err(ProbeFailure::Status(_)))
        || (status == "alive"
            && matches!(first, Err(ProbeFailure::Timeout | ProbeFailure::Transport)));
    if retry {
        let fallback = targets.get(1).unwrap_or(&targets[0]);
        return run_probe(fallback, Some(&proxy_url)).await;
    }
    first
}

type ParsedProxyTuple = (String, String, u16, Option<String>, Option<String>, String);
type ProxyTestCandidate = (String, ParsedProxyTuple, Option<String>);

fn map_candidate(row: &rusqlite::Row<'_>) -> rusqlite::Result<ProxyTestCandidate> {
    Ok((
        row.get(0)?,
        (
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
        ),
        row.get(7)?,
    ))
}

fn fetch_proxy_test_target(
    conn: &Connection,
    id: &str,
) -> crate::error::Result<ProxyTestCandidate> {
    conn.query_row(
        "SELECT id, type, host, port, username, password, status, last_validated FROM free_proxies WHERE id = ?1",
        [id], map_candidate,
    ).map_err(openproxy_db::error::map_db_error)
}

// A concurrent import or validation may have changed the candidate while the
// network request was in flight. Never apply a result to that newer snapshot.
const UPDATE_PROBE: &str = "UPDATE free_proxies
    SET status = ?1, latency_ms = ?2, last_validated = ?3, updated_at = ?3
    WHERE id = ?4 AND type = ?5 AND host = ?6 AND port = ?7
    AND username IS ?8 AND password IS ?9 AND status = ?10 AND last_validated IS ?11";

fn apply_proxy_test_result(
    stmt: &mut rusqlite::Statement<'_>,
    candidate: &ProxyTestCandidate,
    result: &ProbeResult,
    now: &str,
) -> crate::error::Result<()> {
    let (id, (scheme, host, port, user, pass, previous), last_validated) = candidate;
    let (status, latency) = match result {
        Ok(ms) => ("alive", Some(*ms)),
        Err(_) => ("dead", None),
    };
    stmt.execute(rusqlite::params![
        status,
        latency,
        now,
        id,
        scheme,
        host,
        port,
        user,
        pass,
        previous,
        last_validated
    ])
    .map_err(openproxy_db::error::map_db_error)?;
    Ok(())
}

pub async fn test_single_proxy(db_pool: Arc<DbPool>, id: &str) -> crate::error::Result<FreeProxy> {
    const LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    let id_owned = id.to_string();

    let (raw_test_url, type_host_port_user_pass) = {
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

    // Security (audit H-2): revalidate the stored probe target at every use
    // — a value planted via a restored backup or written straight to the DB
    // must never turn the probe into an internal scanner.
    let targets = healthy_probe_targets(&raw_test_url).await;
    if targets.is_empty() {
        return Err(crate::error::CoreError::ServiceUnavailable(
            "No healthy proxy probe target".into(),
        ));
    }
    let candidate = type_host_port_user_pass;
    let test_res = test_proxy_candidate(&targets, &candidate.1).await;
    if let Err(reason) = &test_res {
        tracing::info!(proxy_id = %id, category = reason.label(), "Manual proxy probe failed");
    }

    let pool = Arc::clone(&db_pool);
    let id = id_owned;
    let p = tokio::task::spawn_blocking(move || -> crate::error::Result<FreeProxy> {
        let w = pool.try_writer_for(LOCK_TIMEOUT).ok_or_else(|| {
            crate::error::CoreError::Internal(format!(
                "test_single_proxy: writer lock not acquired within {LOCK_TIMEOUT:?}"
            ))
        })?;
        let mut stmt = w
            .prepare_cached(UPDATE_PROBE)
            .map_err(openproxy_db::error::map_db_error)?;
        apply_proxy_test_result(
            &mut stmt,
            &candidate,
            &test_res,
            &chrono::Utc::now().to_rfc3339(),
        )?;
        get_proxy(&w, &id)?.ok_or_else(|| crate::error::CoreError::NotFound {
            what: "proxy".to_string(),
            id,
        })
    })
    .await
    .map_err(|e| crate::error::CoreError::Internal(format!("spawn_blocking join: {e}")))??;

    Ok(p)
}

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
        SELECT id, type, host, port, username, password, status, last_validated FROM free_proxies
        WHERE 
            status = 'unknown' 
            OR (status = 'alive' AND (last_validated IS NULL OR datetime(last_validated) < datetime('now', '-30 minutes')))
            OR (status = 'dead' AND (last_validated IS NULL OR datetime(last_validated) < datetime('now', '-2 hours')))
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
    let rows = match stmt.query_map(rusqlite::params![limit as i64], map_candidate) {
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
    batch: &[(ProxyTestCandidate, ProbeResult)],
) -> Result<(), crate::error::CoreError> {
    openproxy_db::error::with_busy_retry("execute_proxy_batch_update", || {
        let tx_db = conn
            .transaction()
            .map_err(openproxy_db::error::map_db_error)?;
        let now = chrono::Utc::now().to_rfc3339();
        {
            let mut stmt = tx_db
                .prepare_cached(UPDATE_PROBE)
                .map_err(openproxy_db::error::map_db_error)?;

            for (candidate, test_res) in batch {
                apply_proxy_test_result(&mut stmt, candidate, test_res, &now)?;
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
    const CONCURRENT_PROBES: usize = 60;
    const MAX_BATCHES: usize = 30;

    let raw_test_url = {
        let pool = Arc::clone(&db_pool);
        match tokio::task::spawn_blocking(move || -> crate::error::Result<String> {
            let r = pool.try_reader_for(READER_LOCK_TIMEOUT).ok_or_else(|| {
                crate::error::CoreError::Internal(format!(
                    "test_all_proxies: reader lock not acquired within {READER_LOCK_TIMEOUT:?}"
                ))
            })?;
            Ok(openproxy_db::app_config::load_proxy_test_url(&r)
                .unwrap_or_else(|_| openproxy_db::app_config::PROXY_TEST_URL_DEFAULT.to_string()))
        })
        .await
        {
            Ok(Ok(url)) => url,
            Ok(Err(e)) => {
                tracing::error!("test_all_proxies: load test_url failed: {e}");
                return;
            }
            Err(e) => {
                tracing::error!("test_all_proxies: spawn_blocking join: {e}");
                return;
            }
        }
    };

    let mut targets = None;
    let mut checked = 0;
    let mut failures = std::collections::BTreeMap::new();
    let started = std::time::Instant::now();
    for _ in 0..MAX_BATCHES {
        let pool_reader = Arc::clone(&db_pool);
        let proxies = match tokio::task::spawn_blocking(
            move || -> crate::error::Result<Vec<ProxyTestCandidate>> {
                let r = pool_reader.try_reader_for(READER_LOCK_TIMEOUT).ok_or_else(|| {
                crate::error::CoreError::Internal(format!(
                    "test_all_proxies: reader lock not acquired within {READER_LOCK_TIMEOUT:?}"
                ))
            })?;
                Ok(fetch_background_test_proxies(&r))
            },
        )
        .await
        {
            Ok(Ok(p)) => p,
            Ok(Err(e)) => {
                tracing::error!("test_all_proxies: candidate fetch failed: {e}");
                break;
            }
            Err(e) => {
                tracing::error!("test_all_proxies: spawn_blocking join: {e}");
                break;
            }
        };

        if proxies.is_empty() {
            break;
        }

        let targets = targets.get_or_insert_with(Vec::new);
        if targets.is_empty() {
            *targets = healthy_probe_targets(&raw_test_url).await;
            if targets.is_empty() {
                return;
            }
        }
        let batch_count = proxies.len();
        let targets = targets.as_slice();
        let batch = futures::stream::iter(proxies)
            .map(|candidate| async move {
                let result = test_proxy_candidate(targets, &candidate.1).await;
                (candidate, result)
            })
            .buffer_unordered(CONCURRENT_PROBES)
            .collect::<Vec<_>>()
            .await;
        for (_, result) in &batch {
            if let Err(reason) = result {
                *failures.entry(reason.label()).or_insert(0u32) += 1;
            }
        }
        let pool = Arc::clone(&db_pool);
        match tokio::task::spawn_blocking(move || {
            let mut w = pool.try_writer_for(READER_LOCK_TIMEOUT).ok_or_else(|| {
                crate::error::CoreError::Internal("Proxy validation writer lock timed out".into())
            })?;
            execute_proxy_batch_update(&mut w, &batch)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(error = %e, "Proxy validation batch write failed");
                break;
            }
            Err(e) => {
                tracing::error!(error = %e, "Proxy validation writer task failed");
                break;
            }
        }
        checked += batch_count;

        if batch_count < CANDIDATE_BATCH_LIMIT {
            break;
        }
    }
    if checked != 0 {
        tracing::info!(checked, failures = ?failures, elapsed_ms = started.elapsed().as_millis() as u64, "Proxy validation pass completed");
    }
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

    /// Audit H-2: the probe target must be revalidated at every use. The
    /// literal-IP and scheme cases are synchronous and fully covered here;
    /// the DNS path is exercised by the server's adversarial tests.
    #[tokio::test]
    async fn validate_probe_url_rejects_private_and_non_http_targets() {
        // Private / reserved literals: the whole point of the check.
        for bad in [
            "http://127.0.0.1:8787/trace",
            "http://10.0.0.5/trace",
            "http://192.168.1.10/trace",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/trace",
            "http://[fd00::1]/trace",
            "http://0.0.0.0/trace",
        ] {
            let err = validate_probe_url(bad).await.expect_err(bad);
            assert!(
                err.contains("private/reserved"),
                "{bad}: unexpected reason: {err}"
            );
        }
        // Scheme / shape.
        assert!(validate_probe_url("").await.is_err());
        assert!(
            validate_probe_url("ftp://cloudflare.com/trace")
                .await
                .is_err()
        );
        assert!(validate_probe_url("not a url").await.is_err());
        // A public literal is fine.
        validate_probe_url("https://1.1.1.1/cdn-cgi/trace")
            .await
            .expect("public literal must pass");
    }

    #[test]
    fn changed_credentials_invalidate_health_and_inflight_results() {
        let pool = openproxy_db::conn::DbPool::test_pool_with_prefix("proxy-config-change")
            .expect("test db");
        let mut conn = pool.writer();
        let row = super::super::crud::add_custom_proxy(
            &conn,
            "proxy.example",
            1080,
            "socks5",
            None,
            Some("user"),
            Some("old"),
        )
        .expect("fixture");
        super::super::crud::update_proxy_status(&conn, &row.id, "alive", Some(120)).expect("alive");
        let stale = fetch_proxy_test_target(&conn, &row.id).expect("snapshot");
        let current = super::super::crud::add_custom_proxy(
            &conn,
            "proxy.example",
            1080,
            "socks5",
            None,
            Some("user"),
            Some("new"),
        )
        .expect("updated");
        assert_eq!(current.status, "unknown");
        assert!(current.latency_ms.is_none());
        assert!(current.last_validated.is_none());
        execute_proxy_batch_update(&mut conn, &[(stale, Ok(120))]).expect("write");
        let fresh = fetch_proxy_test_target(&conn, &row.id).expect("snapshot");
        assert_eq!(fresh.1.5, "unknown");
        execute_proxy_batch_update(&mut conn, &[(fresh, Ok(80))]).expect("write");
        let current = get_proxy(&conn, &row.id).expect("read").expect("proxy");
        assert_eq!(current.status, "alive");
        assert_eq!(current.latency_ms, Some(80));
    }

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
                    let pool_clone = std::sync::Arc::clone(&pool);
                    tokio::task::spawn_blocking(move || {
                        let mut w = pool_clone.writer();
                        let first = fetch_proxy_test_target(
                            &w,
                            &format!("proxy-stress-{}", (t * 5 + iter) % 50),
                        )
                        .expect("candidate");
                        let second = fetch_proxy_test_target(
                            &w,
                            &format!("proxy-stress-{}", (t * 5 + iter + 1) % 50),
                        )
                        .expect("candidate");
                        let batch = [
                            (first, Ok(150 + iter as i64)),
                            (second, Err(ProbeFailure::Timeout)),
                        ];
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
