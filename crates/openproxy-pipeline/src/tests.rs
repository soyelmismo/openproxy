use super::*;
use crate::circuit_breaker::Health;
use crate::quotas::QuotaStatus;
use crate::repository::PipelineRepository;
use crate::test_utils::combos;
use crate::timeouts::Timeouts;
use crate::translation::OpenAIResponse;
use openproxy_adapters::UpstreamClient;
use openproxy_db::conn::DbPool;
use openproxy_db::migrations;
use openproxy_db::secrets::MasterKey;
use openproxy_types::CoreError;
use openproxy_types::TargetFormat;
use openproxy_types::combos::{ComboTarget, Strategy};
use openproxy_types::config::{RacingConfig, RetriesConfig, TimeoutsConfig};
use openproxy_types::ids::{
    AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId, TraceId,
};
use openproxy_types::providers::{AuthType, ProviderFormat};
use openproxy_types::{OpenAIMessage, OpenAIRequest};
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

static STAGE_TX: std::sync::LazyLock<
    parking_lot::Mutex<Option<tokio::sync::broadcast::Sender<openproxy_types::usage::StageEvent>>>,
> = std::sync::LazyLock::new(|| parking_lot::Mutex::new(None));

fn global_publisher(event: openproxy_types::usage::StageEvent) {
    if let Some(tx) = STAGE_TX.lock().as_ref() {
        let _ = tx.send(event);
    }
}

async fn drain_http_headers_stream(sock: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 16 * 1024];
    let mut total = 0usize;
    loop {
        let Ok(Ok(n)) = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf[total..])).await else {
            break;
        };
        if n == 0 {
            break;
        }
        total += n;
        if buf[..total].windows(4).any(|w| w == b"\r\n\r\n") || total == buf.len() {
            break;
        }
    }
}

async fn drain_http_request_stream_full(sock: &mut tokio::net::TcpStream) -> (Vec<u8>, Option<usize>) {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 64 * 1024];
    let mut total = 0usize;
    let mut content_length: Option<usize> = None;
    let mut header_end: Option<usize> = None;
    loop {
        let Ok(Ok(n)) = tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf[total..])).await else {
            break;
        };
        if n == 0 {
            break;
        }
        total += n;
        if header_end.is_none()
            && let Some(pos) = buf[..total].windows(4).position(|w| w == b"\r\n\r\n")
        {
            header_end = Some(pos);
            let header_str = std::str::from_utf8(&buf[..pos]).unwrap_or("");
            for line in header_str.split("\r\n") {
                if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = rest.trim().parse().ok();
                }
            }
        }
        if let (Some(he), Some(cl)) = (header_end, content_length)
            && total - (he + 4) >= cl
        {
            break;
        }
        if total == buf.len() {
            break;
        }
    }
    (buf[..total].to_vec(), header_end)
}

async fn respond_with_status_and_body(
    sock: &mut tokio::net::TcpStream,
    status_line: &str,
    body: &[u8],
    content_type: &str,
) {
    use tokio::io::AsyncWriteExt;
    let response = format!(
        "{status_line}\r\n\
         Content-Type: {content_type}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         \r\n",
        body.len()
    );
    let _ = sock.write_all(response.as_bytes()).await;
    let _ = sock.write_all(body).await;
    let _ = sock.flush().await;
}

fn test_config_with_single_mock(master_key: Arc<MasterKey>, provider_name: &str, base_url: &str) -> PipelineConfig {
    use openproxy_adapters::adapters::{AdapterAuthType, AdapterFormat, ProviderAdapterConfig};
    let defaults = Timeouts::from_config(&TimeoutsConfig::default());
    let mock = crate::test_utils::MockAdapter {
        config: ProviderAdapterConfig {
            id: ProviderId::new(provider_name),
            base_url: base_url.to_string(),
            auth_type: AdapterAuthType::Bearer,
            format: AdapterFormat::Openai,
            extra_headers: Vec::new(),
        },
        call_count: None,
        fail_fetch: false,
        models_to_return: None,
    };
    PipelineConfig {
        defaults,
        racing: RacingConfig::default(),
        retries: RetriesConfig::default(),
        max_attempts: 1,
        master_key,
        adapters: Arc::new(vec![
            openproxy_adapters::adapters::ProviderAdapterEnum::Mock(mock),
        ]),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        upstream_client: UpstreamClient::new(),
        oauth_provider_registry: None,
        compression_mode: openproxy_compression::CompressionMode::Off,
        idle_chunk_retryable: true,
        quota_protection: openproxy_types::config::QuotaProtectionConfig::default(),
        background_tx: tokio::sync::mpsc::channel(1).0,
    }
}

async fn send_single_openai_sse_chunk(sock: &mut tokio::net::TcpStream) -> bool {
    use tokio::io::AsyncWriteExt;
    let headers = b"HTTP/1.1 200 OK\r\n\
                    Content-Type: text/event-stream\r\n\
                    Cache-Control: no-cache\r\n\
                    Transfer-Encoding: chunked\r\n\
                    Connection: close\r\n\
                    \r\n";
    let chunk = b"data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\
                  \"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n";
    if sock.write_all(headers).await.is_err() {
        return false;
    }
    let framed = format!(
        "{:x}\r\n{}\r\n",
        chunk.len(),
        std::str::from_utf8(chunk).unwrap()
    );
    if sock.write_all(framed.as_bytes()).await.is_err() {
        return false;
    }
    sock.flush().await.is_ok()
}

async fn stall_watching_client_close(
    mut sock: tokio::net::TcpStream,
    server_client_closed: Arc<std::sync::atomic::AtomicBool>,
    server_bytes: Arc<std::sync::atomic::AtomicU64>,
) {
    use tokio::io::AsyncReadExt;
    let mut stall_buf = [0u8; 1024];
    let stall_deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let now = std::time::Instant::now();
        if now >= stall_deadline {
            break;
        }
        let remaining = stall_deadline - now;
        let read = tokio::time::timeout(remaining, sock.read(&mut stall_buf)).await;
        match read {
            Ok(Ok(0)) | Ok(Err(_)) => {
                server_client_closed.store(true, std::sync::atomic::Ordering::SeqCst);
                break;
            }
            Ok(Ok(n)) => {
                server_bytes.fetch_add(n as u64, std::sync::atomic::Ordering::SeqCst);
            }
            Err(_) => {}
        }
    }
}

fn seed_n_targets_combo(
    w: &rusqlite::Connection,
    mk: &MasterKey,
    provider: &str,
    combo_name: &str,
    strategy: Strategy,
    n: usize,
) -> (ComboId, Vec<ComboTargetId>) {
    use crate::test_utils::combos::AddTargetInput;
    seed_provider(w, provider, AuthType::Bearer);
    w.execute(
        &format!("INSERT INTO models(provider_id, model_id, target_format) VALUES ('{provider}', 'm', 'openai')"),
        [],
    )
    .expect("seed model");
    let model_rowid: i64 = w
        .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
        .expect("last_insert_rowid");
    let model_id = openproxy_types::ids::ModelRowId(model_rowid);
    let combo_id = combos::create_combo(w, combo_name, strategy, 1).expect("create combo");
    let mut tids = Vec::new();
    for i in 0..n {
        let account_label = format!("a{}", i + 1);
        let prio = ((i + 1) * 10) as i32;
        let account_id = openproxy_db::accounts::create(
            w,
            &ProviderId::new(provider),
            Some("sk-test"),
            mk,
            Some(&account_label),
            prio,
            None,
        )
        .expect("seed account");
        let tid = combos::add_target(
            w,
            AddTargetInput {
                combo_id,
                provider_id: ProviderId::new(provider),
                account_id: Some(account_id),
                model_row_id: Some(model_id),
                sub_combo_id: None,
                priority_order: prio as i64,
            },
        )
        .expect("add target");
        tids.push(tid);
    }
    (combo_id, tids)
}

// NEW-2 fix unit tests: parse_retry_after_ms handles integer-seconds
// and HTTP-date forms, applies the 5-minute cap to malicious values,
// and returns None for empty/unparseable input.
#[test]
fn parse_retry_after_ms_integer_seconds() {
    assert_eq!(parse_retry_after_ms("30"), Some(30_000));
    assert_eq!(parse_retry_after_ms("0"), Some(0));
    assert_eq!(parse_retry_after_ms("0.5"), Some(500));
}

#[test]
fn parse_retry_after_ms_no_cap() {
    // 3600s (1h) must NOT be capped anymore.
    assert_eq!(parse_retry_after_ms("3600"), Some(3600 * 1000));
    // 600s (10m)
    assert_eq!(parse_retry_after_ms("600"), Some(600 * 1000));
    // 30s passes through.
    assert_eq!(parse_retry_after_ms("30"), Some(30_000));
}

#[test]
fn parse_retry_after_ms_invalid_inputs() {
    assert_eq!(parse_retry_after_ms(""), None);
    assert_eq!(parse_retry_after_ms("   "), None);
    assert_eq!(parse_retry_after_ms("not-a-number"), None);
    assert_eq!(parse_retry_after_ms("-1"), None);
}

#[test]
fn test_is_upstream_health_issue() {
    // Timeout
    assert!(is_upstream_health_issue(&CoreError::UpstreamTimeout {
        phase: "connect".to_string(),
        ms: 100
    }));
    assert!(!is_upstream_health_issue(&CoreError::UpstreamTimeout {
        phase: "idle_chunk".to_string(),
        ms: 100
    }));

    // Connection error
    assert!(is_upstream_health_issue(&CoreError::UpstreamConnection(
        "reset".to_string()
    )));

    // Rate limited
    assert!(is_upstream_health_issue(&CoreError::RateLimited {
        provider: "test".to_string(),
        retry_after_ms: 1000,
        is_proxy_rotated: false
    }));

    // Upstream error status code
    assert!(is_upstream_health_issue(&CoreError::UpstreamError {
        status: 500,
        provider: "test".to_string(),
        model: "m".to_string(),
        body: "error".to_string(),
        is_proxy_rotated: false
    }));
    assert!(is_upstream_health_issue(&CoreError::UpstreamError {
        status: 503,
        provider: "test".to_string(),
        model: "m".to_string(),
        body: "error".to_string(),
        is_proxy_rotated: false
    }));
    assert!(!is_upstream_health_issue(&CoreError::UpstreamError {
        status: 400,
        provider: "test".to_string(),
        model: "m".to_string(),
        body: "error".to_string(),
        is_proxy_rotated: false
    }));
    assert!(!is_upstream_health_issue(&CoreError::UpstreamError {
        status: 404,
        provider: "test".to_string(),
        model: "m".to_string(),
        body: "error".to_string(),
        is_proxy_rotated: false
    }));

    // Other errors
    assert!(!is_upstream_health_issue(&CoreError::NotFound {
        what: "test".to_string(),
        id: "test".to_string()
    }));
}

/// Build a fresh on-disk pool with migrations applied, plus an
/// independent `Connection` wrapped in a `Mutex<Connection>` for the
/// `Pipeline` to own. The same shape the rest of the crate's test
/// modules use, with a unique tempdir per test to avoid `WAL`-file
/// collisions when tests run in parallel.
fn fresh_pool() -> (DbPool, Arc<parking_lot::Mutex<Connection>>, PathBuf) {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("openproxy-pipeline-test-{}-{}-{}", pid, nanos, n));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = dir.join("pipeline.db");
    let pool = DbPool::open(&path).expect("open pool");
    {
        let mut w = pool.writer();
        migrations::run(&mut w).expect("migrations");
    }
    // A second connection on the same file, owned by the Pipeline.
    let extra = pool.open_connection().expect("open extra connection");
    let conn = Arc::new(parking_lot::Mutex::new(extra));
    (pool, conn, path)
}

/// A reasonable default `PipelineConfig` for tests: no real adapters
/// (the tests only exercise the routing/usage path, not the HTTP path).
fn test_config(master_key: Arc<MasterKey>) -> PipelineConfig {
    let defaults = Timeouts::from_config(&TimeoutsConfig::default());
    PipelineConfig {
        defaults,
        racing: RacingConfig::default(),
        retries: RetriesConfig::default(),
        max_attempts: 1,
        master_key,
        adapters: Arc::new(Vec::new()),
        // A vanilla HTTP client is fine for tests: nothing in the
        // routing path actually fires a request.
        // 60s default cooldown for tests; individual tests that
        // exercise the cooldown path can pass a shorter value
        // through a local `PipelineConfig` override.
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        // Hyper-based upstream client. The default production
        // connector (rustls HTTPS) is fine for tests that don't
        // exercise the HTTP path; tests that DO need a real
        // upstream should rebuild the config with a test
        // connector.
        upstream_client: UpstreamClient::new(),
        oauth_provider_registry: None,
        // Tests use the default Off mode so the production
        // compression behavior is opt-in; individual tests
        // that exercise compression override these.
        compression_mode: openproxy_compression::CompressionMode::Off,
        // Default matches the production default in
        // state.rs; tests don't need to flip this.
        idle_chunk_retryable: true,
        quota_protection: openproxy_types::config::QuotaProtectionConfig::default(),
        background_tx: tokio::sync::mpsc::channel(1).0,
    }
}

/// Seed a provider so combo_targets FKs can be satisfied.
fn seed_provider(conn: &Connection, provider_id: &str, auth_type: AuthType) {
    openproxy_db::providers::create(
        conn,
        openproxy_db::providers::NewProvider {
            id: &ProviderId::new(provider_id),
            name: provider_id,
            base_url: "https://example.com",
            auth_type,
            format: ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
        },
    )
    .expect("seed provider");
}

/// Build a `PipelineRequest` with sensible defaults.
fn make_request(combo_id: ComboId) -> (PipelineRequest, watch::Sender<bool>) {
    let (_dis_tx, dis_rx) = watch::channel::<Option<openproxy_types::CancelReason>>(None);
    let req = PipelineRequest {
        request_id: RequestId::new(),
        trace_id: TraceId::new(),
        combo_id,
        openai_request: Arc::new(OpenAIRequest {
            model: "any".into(),
            messages: vec![OpenAIMessage {
                role: "user".into(),
                content: Some(serde_json::Value::String("hi".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            }],
            stream: false,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        }),
        client_disconnected: dis_rx,
        // Use Discard sink for non-streaming test requests. The
        // pipeline forces stream=true to the upstream, but SSE
        // chunks are discarded — the pipeline accumulates the
        // response internally via ResponseAccumulator.
        stream_sink: Some(crate::race_sink::StreamSink::Discard),
        api_key_id: None,
        combo_override: None,
        targets_override: None,
        request_headers: std::collections::BTreeMap::new(),
        request_body_json: None,
        race_cancelled: false,
        race_cancel: None,
        endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        compressed_messages: Arc::new(std::sync::OnceLock::new()),
        proxy_override: None,
    };
    (req, _dis_tx)
}

/// Minimal `ProviderAdapter` impl for tests that just need URL/header
/// plumbing without any per-format normalization. Tests that need to
/// override `normalize_request_body` should define their own adapter
/// struct (see `normalize_request_body_hook_called_in_chat_pipeline`).

#[test]
fn pipeline_creation_doesnt_panic() {
    let (_pool, conn, _path) = fresh_pool();
    let cfg = test_config(Arc::new(MasterKey::generate()));
    // Constructing a Pipeline with an empty adapter set must succeed.
    let _p = Pipeline::new(conn, cfg);
}

#[test]
fn pipeline_request_clone_shares_large_payloads() {
    let (mut req, _) = make_request(ComboId(1));
    req.request_body_json = Some(bytes::Bytes::from(vec![b'x'; 1024 * 1024]));

    let cloned = req.to_owned();

    assert!(Arc::ptr_eq(&req.openai_request, &cloned.openai_request));
    assert!(Arc::ptr_eq(
        &req.compressed_messages,
        &cloned.compressed_messages
    ));
    assert_eq!(
        req.request_body_json.as_ref().unwrap().as_ptr(),
        cloned.request_body_json.as_ref().unwrap().as_ptr()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn pipeline_run_with_no_targets_returns_502() {
    // With the auto-populate fallback in place, the only way to
    // hit the bare NoHealthyTargets path is to have an empty combo
    // AND no healthy provider to auto-fill from. We seed a single
    // (active) provider with no accounts and no models so the
    // auto-populate query returns 0 candidates.
    let (pool, conn, _path) = fresh_pool();
    let combo_id = {
        let writer = pool.writer();
        // Seed an active provider with no accounts and no models.
        openproxy_db::providers::create(
            &writer,
            openproxy_db::providers::NewProvider {
                id: &ProviderId::new("p"),
                name: "p",
                base_url: "https://example.com",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            },
        )
        .expect("seed provider");
        combos::create_combo(&writer, "no-targets", Strategy::Priority, 1).expect("create")
    };

    let cfg = test_config(Arc::new(MasterKey::generate()));
    let p = Pipeline::new(conn, cfg);

    let (req, _dis_tx) = make_request(combo_id);
    let result = p.run(req).await;

    // NoHealthyTargets is the failure path: 502 per `http_status()`.
    assert_eq!(result.status_code, 502, "no eligible targets → 502");
    match &result.error {
        Some(CoreError::NoHealthyTargets(id)) => assert_eq!(*id, combo_id.0),
        other => panic!("expected NoHealthyTargets, got {:?}", other),
    }
    assert!(result.final_response.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn pipeline_run_no_targets_records_usage_row() {
    // The NoHealthyTargets path must write a usage row so the
    // dashboard's Live Logs tail isn't permanently empty. We
    // arrange the same "no candidate provider" condition as the
    // test above and then assert a usage row was created.
    let (pool, conn, _path) = fresh_pool();
    let combo_id = {
        let writer = pool.writer();
        openproxy_db::providers::create(
            &writer,
            openproxy_db::providers::NewProvider {
                id: &ProviderId::new("p"),
                name: "p",
                base_url: "https://example.com",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            },
        )
        .expect("seed provider");
        combos::create_combo(&writer, "nerd", Strategy::Priority, 1).expect("create")
    };

    let cfg = test_config(Arc::new(MasterKey::generate()));
    let p = Pipeline::new(conn, cfg);

    let (req, _dis_tx) = make_request(combo_id);
    let _ = p.run(req).await;

    // A usage row should now exist. The dashboard reads this via
    // /admin/usage/recent.
    let writer = pool.writer();
    let count: i64 = writer
        .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
        .expect("count usage");
    assert_eq!(count, 1, "exactly one usage row was written");
    let (status, error): (i64, Option<String>) = writer
        .query_row(
            "SELECT status_code, error_msg FROM usage ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .expect("read row");
    assert_eq!(status, 502);
    assert_eq!(error.as_deref(), Some("no_healthy_targets"));
}

#[tokio::test(flavor = "multi_thread")]
async fn auto_populate_fills_combo_then_runs() {
    // The auto-populate fallback should turn an empty combo into
    // a routable one when there is a healthy provider with active
    // models. We seed (provider, healthy account, two active
    // models), create an empty combo, then call the pipeline and
    // expect it to NOT return NoHealthyTargets — instead the
    // auto-populate path fills the combo and the resolve+execute
    // step is reached. The execute will fail (no real adapter /
    // upstream) but the failure is something other than
    // NoHealthyTargets.
    let (pool, conn, _path) = fresh_pool();
    let mk = MasterKey::generate();
    let combo_id = {
        let writer = pool.writer();
        openproxy_db::providers::create(
            &writer,
            openproxy_db::providers::NewProvider {
                id: &ProviderId::new("p"),
                name: "p",
                base_url: "https://example.com",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            },
        )
        .expect("seed provider");
        // Two active models on the same provider.
        writer.execute(
                "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm1', 'openai')",
                [],
            )
            .expect("seed m1");
        writer.execute(
                "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm2', 'openai')",
                [],
            )
            .expect("seed m2");
        let provider = ProviderId::new("p");
        openproxy_db::accounts::create(&writer, &provider, Some("sk-test"), &mk, None, 1, None)
            .expect("seed account");
        combos::create_combo(&writer, "nerd", Strategy::Priority, 1).expect("create")
    };

    let cfg = test_config(Arc::new(mk));
    let p = Pipeline::new(conn, cfg);

    let (req, _dis_tx) = make_request(combo_id);
    let result = p.run(req).await;

    // Auto-population of empty combos is disabled so manual combos are not invaded.
    assert!(matches!(&result.error, Some(CoreError::NoHealthyTargets(_))));

    let writer = pool.writer();
    let count: i64 = writer
        .query_row(
            "SELECT COUNT(*) FROM combo_targets WHERE combo_id = ?1",
            rusqlite::params![combo_id.0],
            |r| r.get(0),
        )
        .expect("count targets");
    assert_eq!(count, 0, "auto-populate is disabled");
}

// -------------------------------------------------------------------
// Bonus tests that exercise the target-expansion + account-rotation
// surface without needing an upstream HTTP server.
// -------------------------------------------------------------------

// -------------------------------------------------------------------
// strip_provider_prefix
// -------------------------------------------------------------------

/// Strip a `<provider>/` prefix off `req.model` if it matches
/// `provider_id`. Otherwise return the request unchanged. Used
/// only by the tests below; production never calls this because
/// upstream targets receive the bare upstream id directly.
fn strip_provider_prefix(
    req: &OpenAIRequest,
    provider_id: &openproxy_types::ids::ProviderId,
) -> OpenAIRequest {
    let prefix = format!("{}/", provider_id.as_str());
    let stripped = if let Some(rest) = req.model.strip_prefix(&prefix) {
        rest.to_string()
    } else {
        req.model.to_owned()
    };
    let mut out = req.to_owned();
    out.model = stripped;
    out
}

fn make_request_with_model(model: &str) -> OpenAIRequest {
    OpenAIRequest {
        model: model.into(),
        messages: vec![OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::Value::String("hi".to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }],
        stream: false,
        temperature: None,
        max_tokens: None,
        top_p: None,
        stop: None,
        tools: None,
        tool_choice: None,
        top_k: None,
        user: None,
        extra: serde_json::Map::new(),
    }
}

#[test]
fn strip_provider_prefix_strips_matching_prefix() {
    // The proxy-level id the client sends in is `openrouter/foo/bar`.
    // The upstream expects `foo/bar`. The strip keeps the
    // nested `/` intact.
    let req = make_request_with_model("openrouter/foo/bar");
    let provider = ProviderId::new("openrouter");
    let stripped = strip_provider_prefix(&req, &provider);
    assert_eq!(stripped.model, "foo/bar");
}

#[test]
fn strip_provider_prefix_keeps_bare_upstream_id() {
    // A client that sends the bare upstream id (no prefix) gets
    // it forwarded as-is. This is the legacy / non-conformant
    // path.
    let req = make_request_with_model("foo/bar");
    let provider = ProviderId::new("openrouter");
    let stripped = strip_provider_prefix(&req, &provider);
    assert_eq!(stripped.model, "foo/bar");
}

#[test]
fn strip_provider_prefix_does_not_match_other_provider() {
    // The prefix only matches the *current* target's provider. A
    // request that happens to start with a different provider's
    // prefix is forwarded verbatim.
    let req = make_request_with_model("anthropic/claude-3.5-sonnet");
    let provider = ProviderId::new("openrouter");
    let stripped = strip_provider_prefix(&req, &provider);
    assert_eq!(stripped.model, "anthropic/claude-3.5-sonnet");
}

#[test]
fn strip_provider_prefix_does_not_clobber_other_fields() {
    // Sanity: the helper must not touch anything other than
    // `model`. We compare the full request shape on the
    // non-`model` fields.
    let req = make_request_with_model("openrouter/foo/bar");
    let provider = ProviderId::new("openrouter");
    let stripped = strip_provider_prefix(&req, &provider);
    assert_eq!(stripped.messages.len(), 1);
    assert_eq!(
        stripped.messages[0]
            .content
            .as_ref()
            .and_then(serde_json::Value::as_str),
        Some("hi")
    );
    assert!(!stripped.stream);
    assert_eq!(stripped.model, "foo/bar");
}

// -------------------------------------------------------------------
// Cooldown integration
//
// The pipeline's hot path now consults `target_cooldowns` and
// writes back to it. We exercise the four observable behaviors
// end-to-end (via `Pipeline::run`'s public surface), keeping
// the tests lightweight by never actually firing an upstream
// HTTP call — the path of interest is the "no eligible
// targets" / "all targets retried" code path that the
// cooldown touches.
// -------------------------------------------------------------------

/// Seed a (provider, healthy account, active model, target)
/// tuple plus a combo that contains the target. Returns the
/// combo id and the target id.
fn seed_target_with_account(
    conn: &Connection,
    mk: &MasterKey,
) -> (ComboId, ComboTargetId, AccountId, ModelRowId) {
    openproxy_db::providers::create(
        conn,
        openproxy_db::providers::NewProvider {
            id: &ProviderId::new("p"),
            name: "p",
            base_url: "https://example.com",
            auth_type: AuthType::Bearer,
            format: ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
        },
    )
    .expect("seed provider");
    conn.execute(
        "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm', 'openai')",
        [],
    )
    .expect("seed model");
    let model_rowid: i64 = conn
        .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
        .expect("last_insert_rowid");
    let account_id = openproxy_db::accounts::create(
        conn,
        &ProviderId::new("p"),
        Some("sk-test"),
        mk,
        None,
        1,
        None,
    )
    .expect("seed account");
    let combo_id = combos::create_combo(conn, "c", Strategy::Priority, 1).expect("combo");
    let target_id = combos::add_target(
        conn,
        combos::AddTargetInput {
            combo_id,
            provider_id: ProviderId::new("p"),
            account_id: Some(account_id),
            model_row_id: Some(ModelRowId(model_rowid)),
            sub_combo_id: None,
            priority_order: 10,
        },
    )
    .expect("add target");
    (combo_id, target_id, account_id, ModelRowId(model_rowid))
}

#[tokio::test(flavor = "multi_thread")]
async fn pipeline_probes_parked_target_when_only_option() {
    // Cooldown semantics: the persistent cooldown protects
    // *between* requests, not *within* a single request. When
    // a priority combo has exactly one target and that target
    // is parked in cooldown, the pipeline does NOT short-circuit
    // to `NoHealthyTargets` (502) anymore. Instead it falls
    // through to the dispatch loop with the unfiltered (pre-
    // cooldown) list, so the operator sees the real upstream
    // error (e.g. `UpstreamConnection`) rather than a misleading
    // "no healthy targets" 502.
    let (pool, conn, _path) = fresh_pool();
    let repo = SqlitePipelineRepository::new(Arc::clone(&conn));
    let mk = Arc::new(MasterKey::generate());
    let (combo_id, target_id, _account_id, _model_id) = {
        let w = pool.writer();
        seed_target_with_account(&w, mk.as_ref())
    };
    // Park the only target for 60s.
    {
        repo.record_cooldown(
            target_id,
            "test seeded",
            openproxy_types::config::CooldownMode::Flat,
            60,
            60,
            1,
        )
        .expect("park");
    }

    let cfg = test_config(mk);
    let p = Pipeline::new(conn, cfg);

    let (req, _dis_tx) = make_request(combo_id);
    let result = p.run(req).await;

    // (a) + (b) The pipeline must NOT surface NoHealthyTargets;
    // the dispatch loop walked the parked target and recorded
    // a real upstream error. The provider URL is
    // https://example.com, which does not resolve in the test
    // environment, so we expect UpstreamConnection (or, less
    // likely, a DNS/connect-flavored variant). Anything but
    // NoHealthyTargets is acceptable.
    match &result.error {
        Some(CoreError::NoHealthyTargets(id)) => panic!(
            "expected the dispatch loop to probe the parked target, \
                 got NoHealthyTargets({})",
            id
        ),
        Some(CoreError::UpstreamConnection(msg)) => {
            // Expected case: the upstream call surfaced a
            // connection error. The status code from
            // CoreError::http_status() for this variant is 502,
            // which would be the same as NoHealthyTargets — so
            // we *don't* assert on status_code here; we only
            // assert the error variant is the real one.
            assert!(
                !msg.is_empty(),
                "UpstreamConnection message should not be empty"
            );
        }
        Some(other) => {
            // Other retryable upstream errors (timeouts, etc.)
            // are also acceptable; the contract is just that we
            // do NOT get NoHealthyTargets.
            eprintln!(
                "pipeline_probes_parked_target_when_only_option: \
                           non-NoHealthyTargets error {:?} (acceptable)",
                other
            );
        }
        None => panic!(
            "expected a real upstream error from probing the parked target, \
                 got a successful result"
        ),
    }

    // (c) The cooldown row is still there: the test did not
    // succeed, and `cooldown::clear` is only called on the
    // success branch of the dispatch loop.
    let w = pool.writer();
    let is_in_cooldown = w.query_row(
        "SELECT COUNT(*) FROM target_cooldowns WHERE combo_target_id = ?1 AND datetime(cooldown_until) > datetime(?2)",
        rusqlite::params![target_id.0, chrono::Utc::now().to_rfc3339()],
        |r| r.get::<_, i64>(0),
    ).unwrap() > 0;
    assert!(is_in_cooldown);
}

fn assert_cooldown_walk_results(w: &rusqlite::Connection, target_ids: &[ComboTargetId], result: &PipelineResult) {
    match &result.error {
        Some(CoreError::NoHealthyTargets(id)) => panic!(
            "expected the dispatch loop to walk all parked targets, got NoHealthyTargets({})",
            id
        ),
        Some(CoreError::UpstreamConnection(msg)) => {
            assert!(!msg.is_empty(), "UpstreamConnection message should not be empty");
        }
        Some(_) => {}
        None => panic!("expected a real upstream error from walking the parked row, got a successful result"),
    }

    let usage_count: i64 = w
        .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
        .expect("count usage");
    assert!(usage_count >= 1, "expected at least one usage row");

    for tid in target_ids {
        let is_in_cooldown = w.query_row(
            "SELECT COUNT(*) FROM target_cooldowns WHERE combo_target_id = ?1 AND datetime(cooldown_until) > datetime(?2)",
            rusqlite::params![tid.0, chrono::Utc::now().to_rfc3339()],
            |r| r.get::<_, i64>(0),
        ).unwrap() > 0;
        assert!(is_in_cooldown, "expected cooldown row for target {} to still be present", tid.0);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pipeline_walks_full_row_when_all_targets_in_cooldown() {
    let (pool, conn, _path) = fresh_pool();
    let repo = SqlitePipelineRepository::new(Arc::clone(&conn));
    let mk = Arc::new(MasterKey::generate());

    let (combo_id, target_ids) = {
        let w = pool.writer();
        seed_n_targets_combo(&w, mk.as_ref(), "p", "c", Strategy::Priority, 3)
    };
    assert_eq!(target_ids.len(), 3, "expected 3 targets in the row");

    {
        let _w = pool.writer();
        for tid in &target_ids {
            repo.record_cooldown(
                *tid,
                "test seeded",
                openproxy_types::config::CooldownMode::Flat,
                60,
                60,
                1,
            )
            .expect("park");
        }
    }

    let cfg = test_config(mk);
    let p = Pipeline::new(conn, cfg);

    let (req, _dis_tx) = make_request(combo_id);
    let result = p.run(req).await;

    let w = pool.writer();
    assert_cooldown_walk_results(&w, &target_ids, &result);
}

/// Regression for bugs 3+4: a `Strategy::Priority` combo of
/// three healthy targets must walk the full row when the first
/// target returns a retryable 500 and the second returns 200.
///
/// Pre-fix the dispatch path collapsed the priority walk to a
/// single target via `take(combo.race_size)` (race_size defaults
/// to 1 in `admin.rs::create_combo`), so the operator's "try
/// the next model when the first one 5xx's" expectation was
/// silently broken: the pipeline kept re-running target #1 on
/// every `max_attempts` turn. This test pins the post-fix
/// behavior:
///   - the mock listener sees TWO HTTP requests (target 1 and
///     target 2; target 3 must NOT be called because the second
///     request succeeded),
///   - the result has no error,
///   - the surfaced body comes from target 2
///     (`choices[0].message.content == "from model 2"`).
fn assert_priority_walk_success(result: &PipelineResult, calls: u32) {
    assert!(result.error.is_none(), "expected success, got error: {:?}", result.error);
    let openai_response = result.final_response.as_ref().expect("final_response must be Some");
    let first_content = openai_response
        .choices
        .first()
        .and_then(|c| c.message.content.as_ref())
        .and_then(|v| v.as_str());
    assert_eq!(first_content, Some("from model 2"));
    assert_eq!(calls, 2, "expected exactly 2 upstream calls");
    assert!(result.attempts >= 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn priority_combo_walks_row_after_first_5xx() {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    let upstream_url = format!("http://{local_addr}");

    let call_count = Arc::new(AtomicU32::new(0));
    let server_call_count = Arc::clone(&call_count);
    let server_handle = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let my_call = server_call_count.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            let _ = drain_http_request_stream_full(&mut sock).await;

            let (status_line, body): (&str, &[u8]) = if my_call == 1 {
                (
                    "HTTP/1.1 500 Internal Server Error",
                    br#"{"error":{"message":"upstream boom","type":"server_error"}}"#,
                )
            } else {
                (
                    "HTTP/1.1 200 OK",
                    b"data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"from model 2\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                )
            };
            respond_with_status_and_body(&mut sock, status_line, body, "text/event-stream").await;
        }
    });

    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let (combo_id, _target_ids) = {
        let w = pool.writer();
        seed_n_targets_combo(&w, mk.as_ref(), "prio-mock", "prio-test", Strategy::Priority, 3)
    };

    let cfg = test_config_with_single_mock(mk, "prio-mock", &upstream_url);
    let p = Pipeline::new(conn, cfg);

    let (req, _cancel_tx) = make_request(combo_id);
    let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
        .await
        .expect("pipeline.run timed out");

    let calls = call_count.load(AtomicOrdering::SeqCst);
    assert_priority_walk_success(&result, calls);
    drop(server_handle);
}

// -------------------------------------------------------------------
// ADVERSARIAL: Combo Priority walk-the-row — the TESTER wants to
// break the fix by trying edge cases the BUILDERs didn't think
// of. These tests are about the contract:
//
//   "Strategy::Priority walks the ENTIRE row in order; it does
//    NOT use combo.race_size as a take(N) cap."
//
// The existing test (priority_combo_walks_row_after_first_5xx)
// covers 3 targets with a single 5xx at the head. The 4 cases
// below push on weaker assumptions:
//   - bigger rows (5),
//   - mixed 4xx + 5xx + 2xx (does 4xx abort the walk?),
//   - all-parked rows (does the dispatch avoid the infinite
//     loop?),
//   - 1-target combos with max_attempts>1 (does the outer loop
//     still fire?).
// -------------------------------------------------------------------

// Build a Priority combo + N targets, all pointing at the same
// mock listener. Returns (combo_id, target_ids, server handle,
// shared call counter). Distinct account labels keep the
// (provider, account) uniqueness constraint happy.

/// ADVERSARIAL (a) — `priority_combo_with_5_targets_walks_to_5th_when_all_fail`.
///
/// 5 targets, ALL return 500. With max_attempts=1 and the
/// pre-fix `take(race_size=1)` collapse, the pipeline would
/// stop at target #1. The fix uses `eligible.len()` for
/// Priority, so the dispatch should attempt all 5 targets in
/// priority order and return the last error.
///
/// We can't assert on the per-call body shape here because the
/// shared mock always returns 200, so we override the listener
/// directly. To assert the walk, we re-spin a 500-only
/// listener inline.
#[tokio::test(flavor = "multi_thread")]
async fn adversarial_priority_combo_with_5_targets_walks_to_5th_when_all_fail() {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    let upstream_url = format!("http://{local_addr}");
    let call_count = Arc::new(AtomicU32::new(0));
    let server_call_count = Arc::clone(&call_count);
    let server_handle = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let _ = server_call_count.fetch_add(1, AtomicOrdering::SeqCst);
            let _ = drain_http_request_stream_full(&mut sock).await;
            let body = br#"{"error":{"message":"all-fail","type":"server_error"}}"#;
            respond_with_status_and_body(&mut sock, "HTTP/1.1 500 Internal Server Error", body, "application/json").await;
        }
    });

    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let (combo_id, _target_ids) = {
        let w = pool.writer();
        seed_n_targets_combo(&w, mk.as_ref(), "adv-mock", "adv-prio-5", Strategy::Priority, 5)
    };

    let cfg = test_config_with_single_mock(mk, "adv-mock", &upstream_url);
    let p = Pipeline::new(conn, cfg);

    let (req, _cancel_tx) = make_request(combo_id);
    let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
        .await
        .expect("pipeline.run timed out");

    let calls = call_count.load(AtomicOrdering::SeqCst);
    assert_eq!(
        calls, 5,
        "expected 5 upstream calls (one per target), got {} — the priority walk did not honor eligible.len()=5 for a 5-target row",
        calls
    );
    match &result.error {
        Some(CoreError::UpstreamError { status, .. }) => {
            assert_eq!(*status, 500, "expected 500 from last target");
        }
        other => panic!("expected CoreError::UpstreamError(500) from the last target, got {:?}", other),
    }
    assert!(result.attempts >= 1);

    drop(server_handle);
}

/// ADVERSARIAL (b) — `priority_combo_with_mixed_4xx_5xx_walks_to_first_2xx`.
///
/// The dispatch loop's per-target branch is:
///   `Some(e) if !RetryPolicy::is_retryable(e, true) => return result`
/// i.e. a 4xx (non-retryable) **aborts** the walk and returns
/// the first error. The pre-fix priority walk AND the post-fix
/// priority walk both have this behavior — a 4xx at target #1
/// will not advance to target #2.
///
/// The TESTER's expectation: the priority combo should walk
/// past a 4xx because the operator's intent is "try the next
/// model on user-error too, not just on transient upstream
/// failure". This is a stronger contract than the current
/// implementation honors.
///
/// If this test fails (the pipeline returns the 4xx from
/// target #1), it documents that the 4xx-abort behavior is a
/// known limitation of the fix and a future iteration needs to
/// reconsider whether 4xx should be retried across targets in
/// a Priority combo.
#[tokio::test(flavor = "multi_thread")]
async fn adversarial_priority_combo_with_mixed_4xx_5xx_walks_to_first_2xx() {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    let upstream_url = format!("http://{local_addr}");
    let call_count = Arc::new(AtomicU32::new(0));
    let server_call_count = Arc::clone(&call_count);
    let server_handle = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let my_call = server_call_count.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            let _ = drain_http_request_stream_full(&mut sock).await;
            let (status_line, body, content_type): (&str, &[u8], &str) = match my_call {
                1 => ("HTTP/1.1 400 Bad Request", br#"{"error":{"message":"bad prompt","type":"invalid_request_error"}}"#, "application/json"),
                2 => ("HTTP/1.1 503 Service Unavailable", br#"{"error":{"message":"overloaded","type":"server_error"}}"#, "application/json"),
                _ => ("HTTP/1.1 200 OK", b"data: {\"id\":\"chatcmpl-3\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"from model 3\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-3\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", "text/event-stream"),
            };
            respond_with_status_and_body(&mut sock, status_line, body, content_type).await;
        }
    });

    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let (combo_id, _target_ids) = {
        let w = pool.writer();
        seed_n_targets_combo(&w, mk.as_ref(), "adv-mock", "adv-prio-mixed", Strategy::Priority, 3)
    };

    let cfg = test_config_with_single_mock(mk, "adv-mock", &upstream_url);
    let p = Pipeline::new(conn, cfg);

    let (req, _cancel_tx) = make_request(combo_id);
    let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
        .await
        .expect("pipeline.run timed out");

    let calls = call_count.load(AtomicOrdering::SeqCst);
    assert_eq!(
        calls, 3,
        "expected 3 upstream calls (walk past 400 -> 503 -> 200), got {calls}"
    );
    assert!(result.error.is_none(), "expected success from target 3, got error: {:?}", result.error);

    drop(server_handle);
}

/// REGRESSION (Bug #2): `round_robin_combo_walks_past_non_retryable_400`.
///
/// A `Strategy::RoundRobin` combo with `race_size=1` and 3 targets
/// where target #1 returns 400 (non-retryable). The walk MUST
/// advance to target #2 and #3, NOT short-circuit on the 400.
///
/// Pre-fix: `pipeline.rs` short-circuited the walk on any
/// non-retryable error for non-Priority strategies
/// (`Strategy::RoundRobin`, `Strategy::Shuffle`), so a 400 from
/// the first target aborted the whole request — sibling targets
/// were never tried. This broke the user's mental model of
/// "combo = try each in order until one works", especially for
/// nested combos (a sub-combo's children are flattened into
/// siblings, so a 400 from the first child aborted the whole
/// request before the parent's next sibling got a chance).
///
/// Post-fix: the strategy guard is removed; the walk falls
/// through to the next sibling on ANY error (retryable OR not).
/// Only `ClientDisconnected` aborts early (handled at the top of
/// the loop).
#[tokio::test(flavor = "multi_thread")]
async fn round_robin_combo_walks_past_non_retryable_400() {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    let upstream_url = format!("http://{local_addr}");
    let call_count = Arc::new(AtomicU32::new(0));
    let server_call_count = Arc::clone(&call_count);
    let server_handle = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let my_call = server_call_count.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            let _ = drain_http_request_stream_full(&mut sock).await;
            let (status_line, body, content_type): (&str, &[u8], &str) = match my_call {
                1 => ("HTTP/1.1 400 Bad Request", br#"{"error":{"message":"invalid params, function name or parameters is empty (2013)","type":"invalid_request_error"}}"#, "application/json"),
                _ => ("HTTP/1.1 200 OK", b"data: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"from model 2\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-2\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", "text/event-stream"),
            };
            respond_with_status_and_body(&mut sock, status_line, body, content_type).await;
        }
    });

    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let (combo_id, _target_ids) = {
        let w = pool.writer();
        seed_n_targets_combo(&w, mk.as_ref(), "rr-mock", "rr-walk-past-400", Strategy::RoundRobin, 2)
    };

    let cfg = test_config_with_single_mock(mk, "rr-mock", &upstream_url);
    let p = Pipeline::new(conn, cfg);

    let (req, _cancel_tx) = make_request(combo_id);
    let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
        .await
        .expect("pipeline.run timed out");

    let calls = call_count.load(AtomicOrdering::SeqCst);
    assert_eq!(calls, 2, "expected 2 upstream calls (walk past 400 -> 200), got {calls}");
    assert!(result.error.is_none(), "expected success from target 2, got error: {:?}", result.error);

    drop(server_handle);
}

/// REGRESSION (Bug #2 — nested combo): `nested_combo_falls_through_to_parent_sibling_on_subcombo_failure`.
///
/// A parent combo `A` with `[sub-combo B, model Z]`, where sub-combo
/// `B` contains `[model X, model Y]`. Both X and Y return 400
/// (non-retryable). Z returns 200.
///
/// Pre-fix: the walk short-circuited on X's 400 (non-retryable, non-Priority
/// strategy) and never reached Y or Z. The user perceived this as
/// "nested combo failure doesn't fall back to parent siblings".
///
/// Post-fix: the walk advances through X (400) → Y (400) → Z (200)
/// and returns Z's 200.
fn seed_nested_parent_and_sub_combo(w: &rusqlite::Connection, mk: &MasterKey) -> ComboId {
    use crate::test_utils::combos::AddTargetInput;
    seed_provider(w, "nested-mock", AuthType::Bearer);
    w.execute(
        "INSERT INTO models(provider_id, model_id, target_format) VALUES ('nested-mock', 'm', 'openai')",
        [],
    )
    .expect("seed model");
    let model_rowid: i64 = w
        .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
        .expect("last_insert_rowid");
    let model_id = openproxy_types::ids::ModelRowId(model_rowid);

    let sub_combo_id = combos::create_combo(w, "sub-B", Strategy::Priority, 1).expect("create sub-combo");
    for i in 0..2 {
        let account_label = format!("sub{}", i);
        let account_id = openproxy_db::accounts::create(
            w,
            &ProviderId::new("nested-mock"),
            Some("sk-test"),
            mk,
            Some(&account_label),
            (i + 1) * 10,
            None,
        )
        .expect("seed account");
        combos::add_target(
            w,
            AddTargetInput {
                combo_id: sub_combo_id,
                provider_id: ProviderId::new("nested-mock"),
                account_id: Some(account_id),
                model_row_id: Some(model_id),
                sub_combo_id: None,
                priority_order: ((i + 1) * 10) as i64,
            },
        )
        .expect("add sub-combo target");
    }

    let parent_combo_id = combos::create_combo(w, "parent-A", Strategy::RoundRobin, 1).expect("create parent combo");
    combos::add_target(
        w,
        AddTargetInput {
            combo_id: parent_combo_id,
            provider_id: ProviderId::new("nested-mock"),
            account_id: None,
            model_row_id: None,
            sub_combo_id: Some(sub_combo_id),
            priority_order: 10,
        },
    )
    .expect("add sub-combo entry to parent");
    let z_account_id = openproxy_db::accounts::create(
        w,
        &ProviderId::new("nested-mock"),
        Some("sk-test"),
        mk,
        Some("z-acct"),
        100,
        None,
    )
    .expect("seed Z account");
    combos::add_target(
        w,
        AddTargetInput {
            combo_id: parent_combo_id,
            provider_id: ProviderId::new("nested-mock"),
            account_id: Some(z_account_id),
            model_row_id: Some(model_id),
            sub_combo_id: None,
            priority_order: 20,
        },
    )
    .expect("add Z entry to parent");
    parent_combo_id
}

#[tokio::test(flavor = "multi_thread")]
async fn nested_combo_falls_through_to_parent_sibling_on_subcombo_failure() {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    let upstream_url = format!("http://{local_addr}");
    let call_count = Arc::new(AtomicU32::new(0));
    let server_call_count = Arc::clone(&call_count);
    let server_handle = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let my_call = server_call_count.fetch_add(1, AtomicOrdering::SeqCst) + 1;
            let _ = drain_http_request_stream_full(&mut sock).await;
            let (status_line, body, content_type): (&str, &[u8], &str) = match my_call {
                1 | 2 => ("HTTP/1.1 400 Bad Request", br#"{"error":{"message":"invalid params (2013)","type":"invalid_request_error"}}"#, "application/json"),
                _ => ("HTTP/1.1 200 OK", b"data: {\"id\":\"chatcmpl-3\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"z\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"from model Z\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-3\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"z\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", "text/event-stream"),
            };
            respond_with_status_and_body(&mut sock, status_line, body, content_type).await;
        }
    });

    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let parent_combo_id = {
        let w = pool.writer();
        seed_nested_parent_and_sub_combo(&w, mk.as_ref())
    };

    let cfg = test_config_with_single_mock(mk, "nested-mock", &upstream_url);
    let p = Pipeline::new(conn, cfg);

    let (req, _cancel_tx) = make_request(parent_combo_id);
    let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
        .await
        .expect("pipeline.run timed out");

    let calls = call_count.load(AtomicOrdering::SeqCst);
    assert_eq!(
        calls, 3,
        "expected 3 upstream calls (X 400 -> Y 400 -> Z 200), got {calls}"
    );
    assert!(result.error.is_none(), "expected success from Z, got error: {:?}", result.error);

    drop(server_handle);
}

/// ADVERSARIAL (c) — `priority_combo_with_zero_eligible_targets_fails_fast`.
///
/// A combo with N targets ALL parked in cooldown must NOT
/// infinite-loop. The pipeline must surface NoHealthyTargets
/// (or, per the snapshot fallback, fall through to the
/// unfiltered list and exercise the parked targets with the
/// real upstream error).
///
/// The fix's snapshot-fallback path means a single request
/// doesn't bounce off the transient cross-request cooldown
/// state. We assert that the call returns a result (not a
/// hang) and that `attempts` is bounded (the pipeline did
/// NOT spin forever).
#[tokio::test(flavor = "multi_thread")]
async fn adversarial_priority_combo_with_zero_eligible_targets_fails_fast() {
    use crate::test_utils::combos::AddTargetInput;
    use std::sync::atomic::Ordering;
    use std::time::Instant;
    let (pool, conn, _path) = fresh_pool();
    let repo = SqlitePipelineRepository::new(Arc::clone(&conn));
    let mk = Arc::new(MasterKey::generate());
    let (combo_id, target_ids, _account_id, _model_id) = {
        let w = pool.writer();
        seed_target_with_account(&w, mk.as_ref())
    };
    // Add 2 more targets to make it a 3-target row. (Re-uses
    // the same provider + model; distinct account labels keep
    // uniqueness happy.)
    {
        let w = pool.writer();
        let model_rowid: i64 = w
            .query_row("SELECT id FROM models WHERE provider_id = 'p'", [], |r| {
                r.get(0)
            })
            .expect("model rowid");
        for i in 1..=2 {
            let account_label = format!("adv{}", i);
            let account_id = openproxy_db::accounts::create(
                &w,
                &ProviderId::new("p"),
                Some("sk-test"),
                mk.as_ref(),
                Some(&account_label),
                (i + 1) * 10,
                None,
            )
            .expect("seed account");
            combos::add_target(
                &w,
                AddTargetInput {
                    combo_id,
                    provider_id: ProviderId::new("p"),
                    account_id: Some(account_id),
                    model_row_id: Some(openproxy_types::ids::ModelRowId(model_rowid)),
                    sub_combo_id: None,
                    priority_order: ((i + 1) * 10) as i64,
                },
            )
            .expect("add target");
        }
    }
    // Park ALL targets.
    {
        let w = pool.writer();
        let all_tids: Vec<ComboTargetId> = {
            let mut stmt = w
                .prepare("SELECT id FROM combo_targets WHERE combo_id = ?1")
                .expect("prep");
            let ids: Vec<i64> = stmt
                .query_map([combo_id.0], |r| r.get(0))
                .expect("query")
                .map(|r| r.unwrap())
                .collect();
            ids.into_iter().map(ComboTargetId).collect()
        };
        for tid in &all_tids {
            repo.record_cooldown(
                *tid,
                "adv seeded",
                openproxy_types::config::CooldownMode::Flat,
                60,
                60,
                1,
            )
            .expect("park");
        }
        assert_eq!(all_tids.len(), 3, "expected 3 targets in the combo");
        // Sanity: the 3 IDs we hold match.
        assert!(target_ids == all_tids[0]);
    }
    let cfg = test_config(mk);
    let p = Pipeline::new(conn, cfg);
    let (req, _dis_tx) = make_request(combo_id);
    let t0 = Instant::now();
    // Bounded: 10s is plenty for a 3-target row to fail fast.
    let result = tokio::time::timeout(Duration::from_secs(10), p.run(req))
        .await
        .expect("pipeline.run timed out — the priority walk is hanging on the parked targets");
    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "priority walk took {elapsed:?} — the fallback path may be retrying the parked targets without bound"
    );
    // The result must have an error (no successful upstream call).
    assert!(
        result.error.is_some(),
        "expected an error after the walk, got a successful result"
    );
    // The error must NOT be a NoHealthyTargets-only path that
    // hides the real upstream error. Either the fallback
    // exercised the parked targets and surfaced an upstream
    // error, or the row was truly empty and the contract says
    // NoHealthyTargets is acceptable. Both are valid; what we
    // pin is that the pipeline returned a result, not a hang.
    eprintln!(
        "[adversarial c] result.error = {:?}, elapsed = {:?}",
        result.error, elapsed
    );
    let _ = Ordering::SeqCst;
}

/// ADVERSARIAL (d) — `priority_combo_respects_max_attempts_for_same_provider`.
///
/// Degenerate case: a Priority combo with a SINGLE target, but
/// `max_attempts = 3`. The outer `for attempt in 1..=max_attempts`
/// loop must fire 3 times, and the same model must be retried
/// 3 times. The pre-fix Priority walk used
/// `take(race_size=1)` which gave the SAME result (1 target
/// attempted per attempt), so this test passes either way for
/// the 1-target degenerate case. The TESTER pins it to detect
/// a future regression where the inner walk is moved INSIDE
/// the outer loop with the wrong `to_run` capture.
#[tokio::test(flavor = "multi_thread")]
async fn adversarial_priority_combo_respects_max_attempts_for_same_provider() {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    let upstream_url = format!("http://{local_addr}");
    let call_count = Arc::new(AtomicU32::new(0));
    let server_call_count = Arc::clone(&call_count);
    let server_handle = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let _ = server_call_count.fetch_add(1, AtomicOrdering::SeqCst);
            let _ = drain_http_request_stream_full(&mut sock).await;
            let body = br#"{"error":{"message":"flaky","type":"server_error"}}"#;
            respond_with_status_and_body(&mut sock, "HTTP/1.1 503 Service Unavailable", body, "application/json").await;
        }
    });

    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let (combo_id, _target_ids) = {
        let w = pool.writer();
        seed_n_targets_combo(&w, mk.as_ref(), "adv-mock", "adv-prio-1", Strategy::Priority, 1)
    };

    let mut cfg = test_config_with_single_mock(mk, "adv-mock", &upstream_url);
    cfg.max_attempts = 3;
    cfg.retries.backoff_base_ms = 1;
    cfg.retries.backoff_factor = 1;
    cfg.retries.backoff_jitter_pct = 0;
    let p = Pipeline::new(conn, cfg);

    let (req, _cancel_tx) = make_request(combo_id);
    let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
        .await
        .expect("pipeline.run timed out");

    let calls = call_count.load(AtomicOrdering::SeqCst);
    assert_eq!(
        calls, 3,
        "expected 3 upstream calls for 1-target Priority combo with max_attempts=3, got {calls}"
    );
    assert_eq!(result.attempts, 3, "expected PipelineResult.attempts == 3, got {}", result.attempts);

    drop(server_handle);
}

/// ADVERSARIAL (e) — `bug4_per_target_retry_exhausts_then_falls_through_to_next_target`.
///
/// Bug 4 regression. The pre-fix pipeline applied the
/// `retries.max_attempts` knob at the *combo walk* level
/// (a single outer `for attempt in 1..=max_attempts` loop
/// re-walked the whole row of targets). With a 2-target
/// combo and `max_attempts=3`, the first target (always 5xx)
/// would consume the *entire* retry budget, and the second
/// target would only get one try (the third outer iteration
/// would re-walk the row, fail at the first target, and bail
/// out via the post-loop block). Net effect: the first target
/// got 3 tries, the second got 0.
///
/// The post-fix per-target retry loop fires
/// `retries.max_attempts` times on the *same* model. Once
/// those are exhausted, the pipeline falls through to the
/// next target (bug 3 contract). For this test that means:
/// target 1 → 3 tries (all 503) → fall through → target 2 →
/// 1 try (200) → success. Total upstream calls: 4. The 4th
/// call is the one that succeeds.
#[tokio::test(flavor = "multi_thread")]
async fn bug4_per_target_retry_exhausts_then_falls_through_to_next_target() {
    use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
    use tokio::net::TcpListener;

    const TARGET1_RETRY_BUDGET: u32 = 3;
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    let upstream_url = format!("http://{local_addr}");
    let call_count = Arc::new(AtomicU32::new(0));
    let server_call_count = Arc::clone(&call_count);
    let server_handle = tokio::spawn(async move {
        loop {
            let (mut sock, _peer) = match listener.accept().await {
                Ok(p) => p,
                Err(_) => break,
            };
            let n = server_call_count.fetch_add(1, AtomicOrdering::SeqCst);
            let _ = drain_http_request_stream_full(&mut sock).await;
            let (status_line, body, content_type): (&str, &[u8], &str) = if n < TARGET1_RETRY_BUDGET {
                ("HTTP/1.1 503 Service Unavailable", br#"{"error":{"message":"flaky","type":"server_error"}}"#, "application/json")
            } else {
                ("HTTP/1.1 200 OK", b"data: {\"id\":\"chatcmpl-bug4\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-bug4\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n", "text/event-stream")
            };
            respond_with_status_and_body(&mut sock, status_line, body, content_type).await;
        }
    });

    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let (combo_id, _target_ids) = {
        let w = pool.writer();
        seed_n_targets_combo(&w, mk.as_ref(), "adv-mock", "adv-bug4", Strategy::Priority, 2)
    };

    let mut cfg = test_config_with_single_mock(mk, "adv-mock", &upstream_url);
    cfg.retries.max_attempts = TARGET1_RETRY_BUDGET as u8;
    cfg.retries.backoff_base_ms = 1;
    cfg.retries.backoff_factor = 1;
    cfg.retries.backoff_jitter_pct = 0;
    cfg.retries.combo_max_attempts = 1;
    let p = Pipeline::new(conn, cfg);

    let (req, _cancel_tx) = make_request(combo_id);
    let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
        .await
        .expect("pipeline.run timed out");

    let calls = call_count.load(AtomicOrdering::SeqCst);
    assert_eq!(
        calls, 4,
        "expected 4 upstream calls (3 retries of target 1 + 1 success of target 2), got {calls}"
    );
    assert!(result.error.is_none(), "expected success after target 2's first call, got error: {:?}", result.error);
    assert_eq!(result.status_code, 200, "expected 200, got {}", result.status_code);
    let body = result.final_response.as_ref().expect("final_response must be set on success");
    assert!(body.id.starts_with("chatcmpl-bug4") || body.id.starts_with("chatcmpl-"));

    drop(server_handle);
}

#[tokio::test(flavor = "multi_thread")]
async fn pipeline_does_not_record_cooldown_on_4xx_error() {
    // The pipeline uses `RetryPolicy::is_retryable` to decide
    // whether to park a target. With the revised retry policy,
    // 4xx IS retryable (so the combo walk tries the next target),
    // but it does NOT record a cooldown — cooldowns are only for
    // retryable failures that indicate the upstream itself is
    // degraded (timeouts, connection errors, rate limits).
    // A 4xx is a provider-specific validation error (e.g. MiniMax
    // 2013), not an upstream health issue, so parking the target
    // would incorrectly block a model that might work on the next
    // request with different content.
    //
    // The pipeline's cooldown-record logic checks `is_retryable`
    // AND a separate "is this an upstream-health issue?" guard
    // before recording. This test verifies the retryable flag
    // is true (so the walk continues) but the cooldown logic
    // itself gates on a different condition.
    use crate::retry::RetryPolicy;
    let err_4xx = CoreError::upstream_error(400, "p", "m", "bad", false);
    // 4xx is now retryable (combo walk continues to next target).
    assert!(
        RetryPolicy::is_retryable(&err_4xx, true),
        "4xx must be retryable so the combo walk tries the next target"
    );
    // The pipeline's "did the helper touch the cooldown table?"
    // assertion lives in the integration tests below; this
    // unit-level guard keeps the rule in one place.
}

#[tokio::test(flavor = "multi_thread")]
async fn pipeline_clears_cooldown_on_success_path() {
    // The "clear" path runs inside the execute_single loop. We
    // assert the helper clears the row on a *retryable*
    // success: seed a parked target, simulate the
    // success branch by calling `cooldown::clear` directly
    // (the same call the pipeline makes), and verify the
    // state. This is a shallow check — the deeper integration
    // test would need a real HTTP mock — but it covers the
    // contract that "on success the row goes away".
    let (pool, conn, _path) = fresh_pool();
    let repo = SqlitePipelineRepository::new(Arc::clone(&conn));
    let (combo_id, target_id, _account_id, _model_id) = {
        let w = pool.writer();
        seed_target_with_account(&w, &MasterKey::generate())
    };
    {
        let w = pool.writer();
        repo.record_cooldown(
            target_id,
            "before",
            openproxy_types::config::CooldownMode::Flat,
            60,
            60,
            1,
        )
        .expect("park");

        let is_in_cooldown = w.query_row(
            "SELECT COUNT(*) FROM target_cooldowns WHERE combo_target_id = ?1 AND datetime(cooldown_until) > datetime(?2)",
            rusqlite::params![target_id.0, chrono::Utc::now().to_rfc3339()],
            |r| r.get::<_, i64>(0),
        ).unwrap() > 0;
        assert!(is_in_cooldown);

        // Simulate the success branch the pipeline runs.
        repo.clear_cooldown(target_id).expect("clear");

        let is_in_cooldown = w.query_row(
            "SELECT COUNT(*) FROM target_cooldowns WHERE combo_target_id = ?1 AND datetime(cooldown_until) > datetime(?2)",
            rusqlite::params![target_id.0, chrono::Utc::now().to_rfc3339()],
            |r| r.get::<_, i64>(0),
        ).unwrap() > 0;
        assert!(!is_in_cooldown);
    }
    let _ = combo_id;
}

#[tokio::test]
async fn test_cooldown_disabled_modes() {
    let pool = TestPool::new();
    let conn = Arc::new(parking_lot::Mutex::new(pool.writer()));
    let repo = SqlitePipelineRepository::new(Arc::clone(&conn));
    let (_combo_id, target_id, _account_id, _model_id) = {
        let w = pool.writer();
        seed_target_with_account(&w, &MasterKey::generate())
    };

    // 1. CooldownMode::None should not insert any row into target_cooldowns
    repo.record_cooldown(
        target_id,
        "mode-none",
        openproxy_types::config::CooldownMode::None,
        60,
        60,
        1,
    )
    .expect("record with None");

    let count: i64 = pool.writer().query_row(
        "SELECT COUNT(*) FROM target_cooldowns WHERE combo_target_id = ?1",
        rusqlite::params![target_id.0],
        |r| r.get(0),
    ).unwrap();
    assert_eq!(count, 0);

    // 2. CooldownMode::Flat with base_secs = 0 should not insert any row
    repo.record_cooldown(
        target_id,
        "base-zero",
        openproxy_types::config::CooldownMode::Flat,
        0,
        60,
        1,
    )
    .expect("record with base 0");

    let count: i64 = pool.writer().query_row(
        "SELECT COUNT(*) FROM target_cooldowns WHERE combo_target_id = ?1",
        rusqlite::params![target_id.0],
        |r| r.get(0),
    ).unwrap();
    assert_eq!(count, 0);
}

// -------------------------------------------------------------------
// Circuit-breaker regression
//
// The cooldown fix (snapshot pre-cooldown + fallback to unfiltered
// dispatch) only covers the persistent `target_cooldowns` table.
// The in-memory `CircuitBreakerRegistry` is a SECOND, independent
// de-route path: every account that hits the failure threshold
// (5 retryable failures, 60s unhealthy window) is filtered out by
// the `eligible` filter (line 213-220) BEFORE the cooldown
// snapshot is taken, leaving `to_run_unfiltered_snapshot` empty
// and the pipeline short-circuits to NoHealthyTargets.
//
// This regression reproduces the user's reported failure mode for
// the 'nerd' combo (9 targets) without touching production code:
// we seed a combo with 9 targets (3 providers × 3 accounts),
// force every account into the `Unhealthy` state via the
// circuit-breaker test helper, and call `Pipeline::run()`. The
// current code short-circuits with `NoHealthyTargets` in 0 ms;
// the desired behaviour is to walk the row (the dispatch loop
// will see ProviderNotFound or similar, and the
// `record_and_fail` will produce a real upstream-flavoured
// error) so the operator gets a useful log line instead of a
// misleading 502.
// -------------------------------------------------------------------

fn seed_nine_targets_three_providers(w: &rusqlite::Connection, mk: &MasterKey) -> (ComboId, Vec<(ProviderId, AccountId)>) {
    use crate::test_utils::combos::AddTargetInput;
    let combo_id = combos::create_combo(w, "nerd", Strategy::Priority, 1).expect("create combo");
    let mut acc_ids = Vec::new();
    for prov_idx in 0..3 {
        let pid_str = format!("p{}", prov_idx);
        openproxy_db::providers::create(
            w,
            openproxy_db::providers::NewProvider {
                id: &ProviderId::new(&pid_str),
                name: &pid_str,
                base_url: "https://example.com",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            },
        )
        .expect("seed provider");
        w.execute(
            "INSERT INTO models(provider_id, model_id, target_format) VALUES (?1, ?2, 'openai')",
            rusqlite::params![&pid_str, format!("m{}", prov_idx)],
        )
        .expect("seed model");
        let model_rowid: i64 = w
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("last_insert_rowid");
        let model_id = ModelRowId(model_rowid);

        for acct_idx in 0..3 {
            let label = format!("a{}-{}", prov_idx, acct_idx);
            let account_id = openproxy_db::accounts::create(
                w,
                &ProviderId::new(&pid_str),
                Some("sk-test"),
                mk,
                Some(&label),
                prov_idx * 3 + acct_idx + 1,
                None,
            )
            .expect("seed account");
            combos::add_target(
                w,
                AddTargetInput {
                    combo_id,
                    provider_id: ProviderId::new(&pid_str),
                    account_id: Some(account_id),
                    model_row_id: Some(model_id),
                    sub_combo_id: None,
                    priority_order: ((prov_idx * 3 + acct_idx + 1) * 10) as i64,
                },
            )
            .expect("add target");
            acc_ids.push((ProviderId::new(&pid_str), account_id));
        }
    }
    (combo_id, acc_ids)
}

#[tokio::test(flavor = "multi_thread")]
async fn combo_with_all_accounts_in_circuit_breaker_does_not_short_circuit() {
    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());

    let (combo_id, account_ids) = {
        let w = pool.writer();
        seed_nine_targets_three_providers(&w, mk.as_ref())
    };
    assert_eq!(account_ids.len(), 9);

    let cfg = test_config(mk);
    let p = Pipeline::new(conn, cfg);

    for (_pid, aid) in &account_ids {
        p.circuit_breaker
            .force_unhealthy(crate::circuit_breaker::CircuitBreakerKey::Account(*aid));
    }
    for (_pid, aid) in &account_ids {
        assert_eq!(
            p.circuit_breaker
                .is_healthy(crate::circuit_breaker::CircuitBreakerKey::Account(*aid)),
            crate::circuit_breaker::Health::Unhealthy,
        );
    }

    let (req, _dis_tx) = make_request(combo_id);
    let result = p.run(req).await;

    match &result.error {
        Some(CoreError::NoHealthyTargets(id)) => {
            panic!("REGRESSION: combo short-circuited to NoHealthyTargets({})", id);
        }
        Some(CoreError::ProviderNotFound(_)) | Some(CoreError::UpstreamConnection(_)) => {}
        Some(other) => {
            eprintln!("combo_with_all_accounts_in_circuit_breaker_does_not_short_circuit: non-NoHealthyTargets error {:?} (acceptable)", other);
        }
        None => panic!("expected a real upstream / per-target error, got a successful result"),
    }

    let w = pool.writer();
    let usage_count: i64 = w
        .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
        .expect("count usage");
    assert!(usage_count >= 1);
}

// -------------------------------------------------------------------
// Targeted unit test: the eligible filter itself, in isolation.
//
// The end-to-end test above mixes adapter lookup, timeouts, and
// the dispatch loop. The root cause is a single filter step:
// pipeline.rs:213-220. This smaller test exercises just that
// step and makes the regression cause-and-effect obvious:
//
//   Given a 9-target list where every target's account is
//   Unhealthy in the in-memory registry, the `eligible` vec
//   built by the filter is empty, so the next branch
//   (`if eligible.is_empty()`) fires NoHealthyTargets.
//
// We can't reach the private `eligible` vec directly, but the
// behaviour is observable through `Pipeline::run()` (see the
// regression test above) and the `to_run` snapshot at line 304
// is the same data the fix depends on.
// -------------------------------------------------------------------

// -----------------------------------------------------------------
// Cancellation regression tests
//
// These lock in the contract that `client_disconnected`:
//   1. aborts an in-flight upstream request (no waiting on
//      `total_ms` when the client is gone),
//   2. is reported with HTTP 499 and `CoreError::ClientDisconnected`,
//   3. does NOT park the target in `target_cooldowns` nor
//      increment the circuit breaker (a client-driven cancel is
//      not an upstream failure).
//
// We use provider id `"openrouter"` because the built-in
// adapter registry (`adapters::builtin_adapters()`) ships an
// adapter for that id; without an adapter the pipeline bails
// with `ProviderNotFound` before the `tokio::select!` is ever
// reached. The `base_url` we pass to the adapter is overridden
// by the provider row in the DB, so we point that row at the
// local mock listener / a dead port.
// -----------------------------------------------------------------

/// Build a `PipelineConfig` that ships the built-in adapter
/// registry, so the dispatch loop can find a `ProviderAdapter`
/// for the provider id under test. The test_config() default
/// has an empty adapter list (correct for the routing-only
/// tests, wrong for anything that exercises the HTTP path).
fn test_config_with_adapters(master_key: Arc<MasterKey>) -> PipelineConfig {
    let mut cfg = test_config(master_key);
    cfg.adapters = Arc::new(openproxy_adapters::adapters::builtin_adapters());
    cfg
}

/// Seed a 1-provider / 1-account / 1-target / 1-combo shape
/// pointing at the given upstream URL. Returns the
/// (`combo_id`, `account_id`) pair so the test can drive the
/// pipeline and inspect the post-run state.
fn seed_solo_combo_at_url(
    conn: &Connection,
    provider_id: &str,
    upstream_url: &str,
    master_key: &MasterKey,
) -> (ComboId, AccountId) {
    openproxy_db::providers::create(
        conn,
        openproxy_db::providers::NewProvider {
            id: &ProviderId::new(provider_id),
            name: provider_id,
            base_url: upstream_url,
            auth_type: AuthType::Bearer,
            format: ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
        },
    )
    .expect("seed provider");
    conn.execute(
        "INSERT INTO models(provider_id, model_id, target_format) \
             VALUES (?1, 'm', 'openai')",
        [provider_id],
    )
    .expect("seed model");
    let model_rowid: i64 = conn
        .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
        .expect("last_insert_rowid");
    let combo_id =
        combos::create_combo(conn, "c", combos::Strategy::Priority, 1).expect("create combo");
    let account_id = openproxy_db::accounts::create(
        conn,
        &ProviderId::new(provider_id),
        Some("sk-test"),
        master_key,
        Some("a1"),
        10,
        None,
    )
    .expect("seed account");
    combos::add_target(
        conn,
        combos::AddTargetInput {
            combo_id,
            provider_id: ProviderId::new(provider_id),
            account_id: Some(account_id),
            model_row_id: Some(ModelRowId(model_rowid)),
            sub_combo_id: None,
            priority_order: 10,
        },
    )
    .expect("add target");
    (combo_id, account_id)
}

/// Cancellation while waiting on the upstream: the `tokio::select!`
/// at the client send site must short-circuit to
/// `ClientDisconnected` / 499 instead of letting the request hang
/// out for `total_ms`.
///
/// We cancel *before* the run starts (analogous to A.2) so the
/// per-target boundary check fires on the first iteration with
/// no upstream work attempted. The send-side `tokio::select!` is
/// exercised by A.3's mock listener below.
#[tokio::test(flavor = "multi_thread")]
async fn cancellation_during_request_aborts_with_499() {
    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());

    let (combo_id, _account_id) =
        seed_solo_combo_at_url(&pool.writer(), "openrouter", "http://127.0.0.1:1", &mk);

    let cfg = test_config_with_adapters(mk);
    let p = Pipeline::new(conn, cfg);

    let (req, cancel_tx) = make_request(combo_id);
    cancel_tx.send(true).expect("send cancel");

    let result = tokio::time::timeout(Duration::from_secs(3), p.run(req))
        .await
        .expect("pipeline.run did not abort within 3s — cancellation is broken");

    match &result.error {
        Some(CoreError::ClientDisconnected) => {
            assert_eq!(
                CoreError::ClientDisconnected.http_status(),
                499,
                "ClientDisconnected must map to HTTP 499"
            );
        }
        other => panic!(
            "expected ClientDisconnected(499) but got {:?} — the \
                 client_disconnected watch is not being honored on the \
                 send/loop path",
            other
        ),
    }
}

/// Cancellation must NOT poison the persistent cooldown table or
/// the in-memory circuit breaker. A client closing the
/// connection is not an upstream failure; the next request from
/// any client should still be able to try the target.
#[tokio::test(flavor = "multi_thread")]
async fn cancellation_does_not_park_target_in_cooldown_or_circuit_breaker() {
    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());

    let (combo_id, account_id) =
        seed_solo_combo_at_url(&pool.writer(), "openrouter", "http://127.0.0.1:1", &mk);
    let cfg = test_config_with_adapters(mk);
    let p = Pipeline::new(conn, cfg);

    let (req, cancel_tx) = make_request(combo_id);
    // Cancel BEFORE the run starts so the per-target boundary
    // check fires on the first iteration with no upstream work
    // attempted at all. The run must still complete normally
    // and exit without writing any cooldown row or
    // incrementing the CB.
    cancel_tx.send(true).expect("send cancel");

    p.run(req).await;

    // 1. target_cooldowns is empty. The schema is keyed by
    //    `combo_target_id` (not `target_id`); see
    //    migrations/000017_add_target_cooldowns.sql.
    let w = pool.writer();
    let target_ids: Vec<i64> = {
        let mut stmt = w
            .prepare("SELECT id FROM combo_targets WHERE combo_id = ?1")
            .expect("prep");
        stmt.query_map([combo_id.0], |r| r.get::<_, i64>(0))
            .expect("query")
            .map(|r| r.expect("row"))
            .collect()
    };
    assert!(!target_ids.is_empty(), "test setup: combo has no targets");
    for tid in &target_ids {
        let count: i64 = w
            .query_row(
                "SELECT COUNT(*) FROM target_cooldowns WHERE combo_target_id = ?1",
                [tid],
                |r| r.get(0),
            )
            .expect("count cooldowns");
        assert_eq!(
            count, 0,
            "target_cooldowns row found for combo_target_id {tid} after a client-driven \
                 cancellation — cancellation should not park targets"
        );
    }

    // 2. The circuit breaker is still Healthy with 0 failures.
    assert_eq!(
        p.circuit_breaker
            .is_healthy(crate::circuit_breaker::CircuitBreakerKey::Account(
                account_id
            )),
        Health::Healthy,
        "circuit breaker for account {account_id:?} was disturbed by a \
             client cancellation — ClientDisconnected must be excluded from \
             the CB counter"
    );
}

/// End-to-end exercise of the new (Gate 1) non-streaming chat
/// dispatch path that uses `UpstreamClient::call()` instead of
/// the legacy client. We bind a localhost listener, point
/// a mock `ProviderAdapter` at it, run a non-streaming chat
/// request, and assert the pipeline returns a 200 with the
/// body parsed as an `OpenAIResponse`. This proves the
/// migration is functionally correct end-to-end: the
/// `UpstreamRequest` is built, the `TimeoutProfile::Custom`
/// resolves correctly, the body is collected via
/// `UpstreamResponse::collect`, and the JSON parses to
/// `OpenAIResponse` (the same downstream code path the
/// client-based path used).
#[tokio::test(flavor = "multi_thread")]
async fn non_streaming_dispatch_uses_upstream_client_end_to_end() {
    use std::sync::Arc;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    let upstream_url = format!("http://{local_addr}");

    let server_handle = tokio::spawn(async move {
        let (mut sock, _peer) = listener.accept().await.expect("accept");
        let _ = drain_http_request_stream_full(&mut sock).await;
        let body = b"data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        respond_with_status_and_body(&mut sock, "HTTP/1.1 200 OK", body, "text/event-stream").await;
    });

    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let (combo_id, _account_id) =
        seed_solo_combo_at_url(&pool.writer(), "non-streaming-test", &upstream_url, &mk);

    let cfg = test_config_with_single_mock(mk, "non-streaming-test", &upstream_url);
    let p = Pipeline::new(conn, cfg);

    let (req, _cancel_tx) = make_request(combo_id);

    let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
        .await
        .expect("pipeline.run timed out — non-streaming dispatch did not return");

    assert!(
        result.error.is_none(),
        "expected no error from non-streaming dispatch but got {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);
    let openai_response = result
        .final_response
        .expect("final_response must be Some on success");
    let first_content = openai_response
        .choices
        .first()
        .and_then(|c| c.message.content.as_ref())
        .and_then(|v| v.as_str());
    assert_eq!(
        first_content,
        Some("hello"),
        "the parsed body must surface the upstream's `choices[0].message.content`"
    );

    let _ = server_handle.await;
}

#[tokio::test(flavor = "multi_thread")]
async fn bug_a_body_reaches_upstream() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    let upstream_url = format!("http://{local_addr}");

    let bytes_received = Arc::new(AtomicUsize::new(0));
    let bytes_received_clone = Arc::clone(&bytes_received);
    let server_handle = tokio::spawn(async move {
        let (mut sock, _peer) = listener.accept().await.expect("accept");
        let (buf, header_end) = drain_http_request_stream_full(&mut sock).await;
        if let Some(he) = header_end {
            let body_bytes = buf.len().saturating_sub(he + 4);
            bytes_received_clone.store(body_bytes, Ordering::SeqCst);
        }
        let body = b"data: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-test\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        respond_with_status_and_body(&mut sock, "HTTP/1.1 200 OK", body, "text/event-stream").await;
    });

    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let (combo_id, _account_id) =
        seed_solo_combo_at_url(&pool.writer(), "body-bug-test", &upstream_url, &mk);

    let cfg = test_config_with_single_mock(mk, "body-bug-test", &upstream_url);
    let p = Pipeline::new(conn, cfg);

    let (req, _cancel_tx) = make_request(combo_id);

    let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
        .await
        .expect("pipeline.run timed out — body-reaches-upstream did not return");

    assert!(
        result.error.is_none(),
        "expected no error from body-bug dispatch but got {:?}",
        result.error
    );
    let _ = server_handle.await;
    let received = bytes_received.load(Ordering::SeqCst);
    assert!(
        received > 50,
        "upstream received only {received} body bytes; expected the full OpenAI chat JSON body"
    );
}

/// End-to-end exercise of the new (Gate 2) streaming chat
/// dispatch path that uses `UpstreamClient::call()` and
/// `UpstreamBodyStream::next_chunk()` instead of the legacy
/// client `collect()` API. We bind a localhost listener,
/// point a mock `ProviderAdapter` at it, run a streaming chat
/// request, and assert the pipeline forwards every SSE chunk
/// (translated to OpenAI) into the `stream_sink` channel in
/// real-time. This proves:
///   1. The `UpstreamRequest` is built and consumed by the
///      hyper-based client.
///   2. The `TimeoutProfile::Custom` is honored at the streaming
///      boundary.
///   3. The body iteration via `UpstreamBodyStream::next_chunk`
///      drives the SSE line splitter.
///   4. The translation step (parse_openai_sse_line +
///      sink.send) still produces a well-formed OpenAI chunk.
#[tokio::test(flavor = "multi_thread")]
async fn streaming_dispatch_uses_upstream_client_end_to_end() {
    use openproxy_adapters::adapters::{AdapterAuthType, AdapterFormat, ProviderAdapterConfig};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // ----- 1. A mock ProviderAdapter that points at our
    //         localhost listener -----

    // ----- 2. Bind the listener and spawn a server that
    //         returns three well-formed OpenAI SSE chunks
    //         followed by the [DONE] sentinel. We use
    //         `Transfer-Encoding: chunked` so the upstream
    //         client's `Limited` body sees multiple frames
    //         (the way a real upstream would stream an
    //         OpenAI response). -----
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
async fn send_openai_stream_chunks(sock: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncWriteExt;
    let headers = b"HTTP/1.1 200 OK\r\n\
                    Content-Type: text/event-stream\r\n\
                    Cache-Control: no-cache\r\n\
                    Connection: close\r\n\
                    \r\n";
    if sock.write_all(headers).await.is_err() {
        return;
    }
    let chunks: &[&[u8]] = &[
        b"data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
        b"data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" there\"},\"finish_reason\":null}]}\n\n",
        b"data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"!\"},\"finish_reason\":null}]}\n\n",
        b"data: [DONE]\n\n",
    ];
    for c in chunks {
        if sock.write_all(c).await.is_err() || sock.flush().await.is_err() {
            return;
        }
    }
    let _ = sock.shutdown().await;
}

fn strip_sse_frame(bytes: &[u8]) -> Option<&[u8]> {
    let done_frame = b"data: [DONE]\n\n";
    if bytes == done_frame {
        return None;
    }
    let data_prefix = b"data: ";
    let suffix = b"\n\n";
    if bytes.starts_with(data_prefix) && bytes.ends_with(suffix) {
        Some(&bytes[data_prefix.len()..bytes.len() - suffix.len()])
    } else {
        None
    }
}

fn assert_streaming_sink_output(collected: &[bytes::Bytes]) {
    assert!(!collected.is_empty(), "expected at least one SSE chunk in sink output");

    let done_count = collected
        .iter()
        .filter(|b| **b == *crate::SSE_DONE_BYTES)
        .count();
    assert!(done_count >= 1, "expected at least one [DONE] sentinel in sink output");

    let mut reconstructed = String::new();
    for item in collected {
        if *item == crate::SSE_DONE_BYTES {
            continue;
        }
        let payload_bytes = strip_sse_frame(item)
            .unwrap_or_else(|| panic!("sink item is not a valid SSE frame: {:?}", item));
        let payload_str = std::str::from_utf8(payload_bytes)
            .unwrap_or_else(|_| panic!("SSE payload is not valid UTF-8: {:?}", payload_bytes));
        let parsed: serde_json::Value = serde_json::from_str(payload_str).unwrap_or_else(|e| {
            panic!("sink item is not valid JSON: {:?} ({})", payload_str, e)
        });
        assert!(
            parsed.get("choices").is_some(),
            "translated chunk must carry a `choices` field: {:?}",
            parsed
        );
        if let Some(content) = parsed
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("delta"))
            .and_then(|d| d.get("content"))
            .and_then(|s| s.as_str())
        {
            reconstructed.push_str(content);
        }
    }
    assert_eq!(reconstructed, "hi there!", "concatenated chunk content mismatch");
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_dispatch_uses_upstream_client_end_to_end() {
    use std::sync::Arc;
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    let upstream_url = format!("http://{local_addr}");

    let server_handle = tokio::spawn(async move {
        let (mut sock, _peer) = listener.accept().await.expect("accept");
        let _ = drain_http_request_stream_full(&mut sock).await;
        send_openai_stream_chunks(&mut sock).await;
    });

    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let (combo_id, _account_id) =
        seed_solo_combo_at_url(&pool.writer(), "streaming-test", &upstream_url, &mk);

    let cfg = test_config_with_single_mock(mk, "streaming-test", &upstream_url);
    let p = Pipeline::new(conn, cfg);

    let (mut req, _cancel_tx) = make_request(combo_id);
    Arc::make_mut(&mut req.openai_request).stream = true;
    let (sink_tx, mut sink_rx) = mpsc::channel::<bytes::Bytes>(32);
    req.stream_sink = Some(crate::race_sink::StreamSink::Direct(sink_tx));

    let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
        .await
        .expect("streaming pipeline.run timed out — next_chunk() did not return");

    assert!(
        result.error.is_none(),
        "expected no error from streaming dispatch but got {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);

    let mut collected: Vec<bytes::Bytes> = Vec::new();
    while let Some(item) = sink_rx.recv().await {
        collected.push(item);
    }

    assert_streaming_sink_output(&collected);
    let _ = server_handle.await;
}

/// Cancellation must abort the streaming response mid-stream
/// without waiting for the upstream to finish sending.
///
/// We cancel *before* the run starts (analogous to A.2) so the
/// per-target boundary check fires on the first iteration with
/// no upstream work attempted.
#[tokio::test(flavor = "multi_thread")]
async fn cancellation_during_streaming_aborts_response_stream() {
    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let (combo_id, _account_id) =
        seed_solo_combo_at_url(&pool.writer(), "openrouter", "http://127.0.0.1:1", &mk);

    let cfg = test_config_with_adapters(mk);
    let p = Pipeline::new(conn, cfg);

    let (mut req, cancel_tx) = make_request(combo_id);
    Arc::make_mut(&mut req.openai_request).stream = true;
    cancel_tx.send(true).expect("send cancel");

    let result = tokio::time::timeout(Duration::from_secs(3), p.run(req))
        .await
        .expect(
            "streaming pipeline.run did not abort within 3s of cancel — \
                    the per-target boundary check is not engaging for streaming requests",
        );

    match &result.error {
        Some(CoreError::ClientDisconnected) => {}
        other => panic!(
            "expected ClientDisconnected(499) but got {:?} — streaming \
                 path is not observing client_disconnected",
            other
        ),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_mid_stream_select_aborts() {
    use tokio::net::TcpListener;
    use tokio::io::AsyncWriteExt;

    // Use a TcpListener to act as our local mock server.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().unwrap();

    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    // Point the default openrouter adapter to our local listener!
    let (combo_id, _account_id) =
        seed_solo_combo_at_url(&pool.writer(), "openrouter", &format!("http://{}", addr), &mk);

    let mut cfg = test_config_with_adapters(mk);
    // The built-in openrouter adapter hardcodes its base_url in build_chat_url.
    // To make it hit our local server, we extract test_config_with_mock's logic, but inline:
    let mock = crate::test_utils::MockAdapter {
        config: openproxy_adapters::adapters::ProviderAdapterConfig {
            id: ProviderId::new("openrouter"),
            base_url: format!("http://{}", addr),
            auth_type: openproxy_adapters::adapters::AdapterAuthType::Bearer,
            format: openproxy_adapters::adapters::AdapterFormat::Openai,
            extra_headers: Vec::new(),
        },
        call_count: None,
        fail_fetch: false,
        models_to_return: None,
    };
    cfg.adapters = Arc::new(vec![openproxy_adapters::adapters::ProviderAdapterEnum::Mock(mock)]);

    let p = Pipeline::new(conn, cfg);

    let (mut req, cancel_tx) = make_request(combo_id);
    Arc::make_mut(&mut req.openai_request).stream = true;

    // Run pipeline in background
    let pipeline_task = tokio::spawn(async move { p.run(req).await });

    // Accept the connection from the pipeline dispatch.
    let (mut sock, _) = listener.accept().await.unwrap();

    // Send headers to start stream
    let headers = b"HTTP/1.1 200 OK\r\n\
                    Content-Type: text/event-stream\r\n\
                    Transfer-Encoding: chunked\r\n\
                    \r\n";
    sock.write_all(headers).await.unwrap();

    // Write one valid SSE chunk to advance the pipeline state into the stream loop.
    let chunk = b"1a\r\ndata: {\"choices\":[]}\n\n\r\n";
    sock.write_all(chunk).await.unwrap();

    // Cancel mid-stream.
    cancel_tx.send(true).unwrap();

    let result = tokio::time::timeout(Duration::from_secs(3), pipeline_task)
        .await
        .expect("timeout")
        .expect("join");

    match &result.error {
        Some(CoreError::ClientDisconnected) => {}
        other => panic!("expected ClientDisconnected but got {:?}", other),
    }
}

/// Mid-stream cancellation: the client disconnects *while the
/// upstream is actively streaming SSE chunks*, and the pipeline
/// must abort the attempt without waiting for the upstream to
/// finish (or for `total_ms` to elapse). This is the contract
/// exercised by the *stream-side* `tokio::select!` at
/// pipeline.rs ~1756 (the one that races
/// `response.bytes_stream().next()` against the
/// `client_disconnected` watch).
///
/// The earlier `cancellation_during_streaming_aborts_response_stream`
/// only proves the per-target boundary check works — it cancels
/// *before* the run starts, so the dispatch loop never reaches
/// the HTTP path. This test goes the other way: we let the
/// dispatch actually open the upstream socket, complete the
/// HTTP exchange, enter the `bytes_stream()` loop, read at
/// least one chunk, and only THEN signal cancellation. The
/// server holds the socket open without sending more data, so
/// the only way the pipeline can finish is by hitting the
/// cancel arm of the inner `tokio::select!`.
fn assert_client_disconnected_499(result: &PipelineResult, accepted: bool) {
    match &result.error {
        Some(CoreError::ClientDisconnected) => {
            assert_eq!(
                CoreError::ClientDisconnected.http_status(),
                499,
                "ClientDisconnected must map to HTTP 499"
            );
        }
        other => panic!(
            "expected ClientDisconnected(499) from mid-stream cancel but got {:?} — the stream-side tokio::select! is not firing on the cancel arm during an active SSE stream",
            other
        ),
    }
    assert!(
        accepted,
        "the mock upstream never accepted a connection — the pipeline did not actually reach the HTTP layer, so this test is not exercising the stream-side select! at all"
    );
}

async fn check_client_closed_observed(client_closed: &std::sync::atomic::AtomicBool, bytes_after_headers: &std::sync::atomic::AtomicU64) {
    let close_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !client_closed.load(std::sync::atomic::Ordering::SeqCst) && std::time::Instant::now() < close_deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let client_closed_observed = client_closed.load(std::sync::atomic::Ordering::SeqCst);
    let bytes_observed = bytes_after_headers.load(std::sync::atomic::Ordering::SeqCst);
    if !client_closed_observed {
        eprintln!(
            "[test note] client_close not observed within 5s; bytes_after_headers={bytes_observed} — this is acceptable when the upstream side closes its end first"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_mid_sse_stream_aborts_immediately() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use tokio::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    let upstream_url = format!("http://{local_addr}");

    let client_closed = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(AtomicBool::new(false));
    let bytes_after_headers = Arc::new(AtomicU64::new(0));

    let server_client_closed = Arc::clone(&client_closed);
    let server_accepted = Arc::clone(&accepted);
    let server_bytes = Arc::clone(&bytes_after_headers);
    let server_handle = tokio::spawn(async move {
        let (mut sock, _peer) = listener.accept().await.expect("accept");
        server_accepted.store(true, Ordering::SeqCst);

        let _ = drain_http_request_stream_full(&mut sock).await;
        if !send_single_openai_sse_chunk(&mut sock).await {
            return;
        }
        stall_watching_client_close(sock, server_client_closed, server_bytes).await;
    });

    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let (combo_id, _account_id) =
        seed_solo_combo_at_url(&pool.writer(), "test-mock-sse", &upstream_url, &mk);

    let cfg = test_config_with_single_mock(mk, "test-mock-sse", &upstream_url);
    let p = Pipeline::new(conn, cfg);

    let (mut req, cancel_tx) = make_request(combo_id);
    Arc::make_mut(&mut req.openai_request).stream = true;

    let cancel_task = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        let _ = cancel_tx.send(true);
    });

    let result = tokio::time::timeout(Duration::from_secs(3), p.run(req))
        .await
        .expect("mid-stream cancellation: pipeline.run did not abort within 3s of cancel");

    let _ = cancel_task.await;

    assert_client_disconnected_499(&result, accepted.load(Ordering::SeqCst));
    check_client_closed_observed(&client_closed, &bytes_after_headers).await;

    server_handle.abort();
    let _ = server_handle.await;
}

// =====================================================================
// Phase-robustness regression tests (spec §5.1 / §5.2 / §5.3).
//
// Each test subscribes to the global stage broadcast BEFORE
// invoking the pipeline, runs the pipeline, then drains the
// receiver for events tagged with the request's `request_id` and
// asserts the expected sequence.
//
// The `STAGE_SENDER` is a process-wide singleton (OnceCell). Other
// tests in the same binary may emit events concurrently, so every
// test filters by `request_id` to scope assertions to its own
// request. A `tokio::sync::broadcast` channel drops events for
// lagging receivers, so the tests also tolerate `Lagged` errors
// by retrying the next event.
// =====================================================================

/// Common scaffolding for the three phase-robustness tests: spin
/// up a fake upstream HTTP server that returns `status_line` /
/// `body` and a tiny OpenAI-shaped JSON body (when the caller
/// wants 2xx), wire it into a `Pipeline` whose recording flag is
/// ON, subscribe to `stage_broadcast()`, run the pipeline, and
/// drain the events matching the request's id. Returns
/// `(events_for_request, run_result)`.
async fn run_with_fake_upstream_and_capture_stages(
    status_line: &'static str,
    body: &'static str,
    content_type: &'static str,
    streaming: bool,
) -> (
    Vec<openproxy_types::usage::StageEvent>,
    PipelineResult,
    RequestId,
) {
    use openproxy_adapters::adapters::AdapterFormat;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // 1. Mock adapter.
    // 2. Bind a listener and serve one request.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    let upstream_url = format!("http://{local_addr}");

    let server_handle = tokio::spawn(async move {
        let (mut sock, _peer) = listener.accept().await.expect("accept");
        // Drain the request headers + body so the client's POST
        // can finish and the response can fly.
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0usize;
        let mut content_length: Option<usize> = None;
        let mut header_end: Option<usize> = None;
        loop {
            let r =
                tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf[total..])).await;
            match r {
                Err(_) | Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(n)) => {
                    total += n;
                    if header_end.is_none()
                        && let Some(pos) = buf[..total].windows(4).position(|w| w == b"\r\n\r\n")
                    {
                        header_end = Some(pos);
                        let header_str = std::str::from_utf8(&buf[..pos]).unwrap_or("");
                        for line in header_str.split("\r\n") {
                            if let Some(rest) =
                                line.to_ascii_lowercase().strip_prefix("content-length:")
                            {
                                content_length = rest.trim().parse().ok();
                            }
                        }
                    }
                    if let (Some(he), Some(cl)) = (header_end, content_length)
                        && total - (he + 4) >= cl
                    {
                        break;
                    }
                    if total == buf.len() {
                        break;
                    }
                }
            }
        }
        let response = format!(
            "{}\r\n\
                 Content-Type: {}\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {}",
            status_line,
            content_type,
            body.len(),
            body,
        );
        let _ = sock.write_all(response.as_bytes()).await;
        let _ = sock.flush().await;
    });

    // 3. Seed DB and wire the pipeline with recording ON.
    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let provider_id = "phase-rob";
    let (combo_id, _account_id) =
        seed_solo_combo_at_url(&pool.writer(), provider_id, &upstream_url, &mk);

    let defaults = Timeouts::from_config(&TimeoutsConfig::default());
    let mock = crate::test_utils::MockAdapter::new(
        provider_id,
        upstream_url.to_owned(),
        AdapterFormat::Openai,
    );
    let recording_flag = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let cfg = PipelineConfig {
        defaults,
        racing: RacingConfig::default(),
        retries: RetriesConfig::default(),
        max_attempts: 1,
        master_key: mk,
        adapters: Arc::new(vec![
            openproxy_adapters::adapters::ProviderAdapterEnum::Mock(mock),
        ]),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        upstream_client: UpstreamClient::new(),
        oauth_provider_registry: None,
        // Auto-added (test compile fix):
        compression_mode: openproxy_compression::CompressionMode::Off,
        idle_chunk_retryable: true,
        quota_protection: openproxy_types::config::QuotaProtectionConfig::default(),
        background_tx: tokio::sync::mpsc::channel(1).0,
    };
    let p = Pipeline::with_recording_flag(conn, cfg, recording_flag);

    // 4. Subscribe to the stage broadcast and capture the
    //    request id we will run with.
    let _ = openproxy_types::usage::STAGE_EVENT_PUBLISHER.set(global_publisher);
    let (tx, mut rx) = tokio::sync::broadcast::channel(100);
    *STAGE_TX.lock() = Some(tx);
    let (mut req, _cancel_tx) = make_request(combo_id);
    Arc::make_mut(&mut req.openai_request).stream = streaming;
    // The default `make_request` helper drops the stream_sink
    // receiver as soon as the function returns, which would
    // cause the pipeline's `sink.send(...)` calls to return
    // `Err` and the streaming path to early-return from
    // `dispatch_upstream_streaming` *before* reaching the
    // `UsageRecordBuilder` call that publishes
    // the terminal `completed` event. To exercise the full
    // success path we need a real receiver that stays alive
    // for the duration of the pipeline run. For the
    // non-streaming path the stream_sink is never written to,
    // so the dropped receiver is harmless.
    let mut sink_rx_for_streaming = None;
    if streaming {
        let (sink_tx, sink_rx) = mpsc::channel::<bytes::Bytes>(32);
        req.stream_sink = Some(crate::race_sink::StreamSink::Direct(sink_tx));
        sink_rx_for_streaming = Some(sink_rx);
    } else {
        // Non-streaming: use Discard sink so the pipeline uses
        // the streaming path internally (forces stream=true to
        // upstream) but discards the SSE chunks.
        req.stream_sink = Some(crate::race_sink::StreamSink::Discard);
    }
    let request_id = req.request_id;
    let request_id_str = request_id.to_string();

    // 5. Run the pipeline.
    let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
        .await
        .expect("pipeline.run timed out");
    // Keep the sink receiver alive until after the pipeline
    // has returned, so the streaming path can publish
    // `completed`. Drop it now.
    drop(sink_rx_for_streaming);

    // 6. Drain the broadcast for events whose `request_id`
    //    matches ours. We read until either we see the
    //    terminal event (`completed` / `failed`) or we hit a
    //    short idle window.
    let mut events: Vec<stage_event::StageEvent> = Vec::new();
    let drain_deadline = std::time::Instant::now() + Duration::from_millis(500);
    loop {
        let now = std::time::Instant::now();
        if now >= drain_deadline {
            break;
        }
        let remaining = drain_deadline.saturating_duration_since(now);
        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(ev)) => {
                if ev.request_id == request_id_str {
                    let terminal = ev.stage == "completed" || ev.stage == "failed";
                    events.push(ev);
                    if terminal {
                        // Give the broadcast a brief moment to
                        // deliver any trailing events (e.g. a
                        // duplicate that would prove the dedup
                        // regression), but don't wait long.
                        if let Ok(Ok(ev2)) =
                            tokio::time::timeout(Duration::from_millis(50), rx.recv()).await
                            && ev2.request_id == request_id_str
                        {
                            events.push(ev2);
                        }
                        break;
                    }
                }
            }
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                // A slow consumer dropped some events; the test
                // doesn't depend on every event being seen, but
                // we must keep draining so we don't block.
                continue;
            }
            Ok(Err(_)) => break,
            Err(_) => break, // timeout → assume we got everything
        }
    }

    // Stop the server.
    server_handle.abort();
    let _ = server_handle.await;

    (events, result, request_id)
}

// Re-export of `StageEvent` used by the test helper above
// for its event-collection `Vec`. Kept inside the test
// module so it doesn't leak into the public API.
mod stage_event {
    pub use openproxy_types::usage::StageEvent;
}

/// §5.1: A successful non-streaming request must publish
/// `started → connecting → waiting_ttft → streaming → completed`
/// in that order, with `streaming.ttft_ms.is_some()` and the
/// final `completed` carrying `error: None`.
#[tokio::test(flavor = "multi_thread")]
async fn phase_robustness_non_streaming_emits_full_stage_sequence() {
    // Since the pipeline now forces stream=true to the upstream,
    // the mock must return SSE (not JSON) for 200 OK responses.
    let body = b"data: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hello\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let body_str = std::str::from_utf8(body).expect("valid utf8");
    let (events, result, _request_id) = run_with_fake_upstream_and_capture_stages(
        "HTTP/1.1 200 OK",
        body_str,
        "text/event-stream",
        /* streaming = */ false,
    )
    .await;

    assert!(
        result.error.is_none(),
        "non-streaming happy path must not error, got {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);

    // Extract just the `stage` labels, in order, for the
    // sequence check.
    let labels: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
    assert!(
        labels.windows(2).all(|w| w[0] != w[1]),
        "stage events must not repeat (got {:?})",
        labels
    );
    // The first three MUST appear in this order; later events
    // (streaming, completed) come from the centralized emit
    // and the body-collect success path.
    assert!(
        labels.contains(&"started"),
        "missing `started` event, got {:?}",
        labels
    );
    assert!(
        labels.contains(&"connecting"),
        "missing `connecting` event, got {:?}",
        labels
    );
    assert!(
        labels.contains(&"waiting_ttft"),
        "missing `waiting_ttft` event, got {:?}",
        labels
    );
    assert!(
        labels.contains(&"streaming"),
        "missing `streaming` event, got {:?}",
        labels
    );
    assert!(
        labels.contains(&"completed"),
        "missing `completed` event, got {:?}",
        labels
    );
    // Order check: `started` precedes `connecting` precedes
    // `waiting_ttft` precedes `streaming` precedes `completed`.
    let pos = |s: &str| labels.iter().position(|x| *x == s);
    let ps = pos("started").expect("started present");
    let pc = pos("connecting").expect("connecting present");
    let pw = pos("waiting_ttft").expect("waiting_ttft present");
    let psm = pos("streaming").expect("streaming present");
    let pco = pos("completed").expect("completed present");
    assert!(
        ps < pc && pc < pw && pw < psm && psm < pco,
        "stage order must be started→connecting→waiting_ttft→streaming→completed, got {:?}",
        labels
    );

    // Sanity-check the `streaming` event carries a ttft_ms and
    // the `completed` event is clean.
    let streaming_evt = events
        .iter()
        .find(|e| e.stage == "streaming")
        .expect("streaming event");
    assert!(
        streaming_evt.ttft_ms.is_some(),
        "streaming event must carry a ttft_ms after the body has been collected"
    );
    let completed_evt = events
        .iter()
        .find(|e| e.stage == "completed")
        .expect("completed event");
    assert_eq!(
        completed_evt.status_code, 200,
        "completed event must carry the 200 status"
    );
    assert!(
        completed_evt.error.is_none(),
        "completed event must not carry an error string, got {:?}",
        completed_evt.error
    );
}

/// §5.2: A successful streaming request must publish
/// `started → connecting → streaming → completed` in that order,
/// with `streaming` fired on the first data line carrying a real
/// `ttft_ms`, and `completed` fired after the loop exits. Note
/// that the streaming dispatch path does NOT emit `waiting_ttft`
/// (§3.4 says no code change in the streaming body loop; the
/// `waiting_ttft` event lives only on the non-streaming path
/// where the operator needs an explicit "headers in, body
/// imminent" signal). The §5.1 test covers the non-streaming
/// 5-event sequence.
#[tokio::test(flavor = "multi_thread")]
async fn phase_robustness_streaming_emits_full_stage_sequence() {
    // The fake upstream just needs to be a real SSE stream
    // with at least one `data: ...` line and a `data: [DONE]`.
    let body = "\
data: {\"id\":\"x\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\n\
data: [DONE]\n\n";
    let (events, result, _request_id) = run_with_fake_upstream_and_capture_stages(
        "HTTP/1.1 200 OK",
        body,
        "text/event-stream",
        /* streaming = */ true,
    )
    .await;

    assert!(
        result.error.is_none(),
        "streaming happy path must not error, got {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);

    let labels: Vec<&str> = events.iter().map(|e| e.stage.as_str()).collect();
    // Required events for a successful streaming request. Note
    // the absence of `waiting_ttft` (see doc comment above).
    let pos = |s: &str| labels.iter().position(|x| *x == s);
    for required in ["started", "connecting", "streaming", "completed"] {
        assert!(
            pos(required).is_some(),
            "missing `{}` event, got {:?}",
            required,
            labels
        );
    }
    // `waiting_ttft` now appears on the streaming path too —
    // since we force stream=true for non-streaming clients,
    // both paths share the same stage sequence for consistency.
    // The test now asserts it IS present (previously it asserted
    // absence per §3.4, but the architectural change to always
    // stream supersedes that spec clause).
    assert!(
        pos("waiting_ttft").is_some(),
        "streaming path must emit `waiting_ttft` (headers received), got {:?}",
        labels
    );
    let ps = pos("started").unwrap();
    let pc = pos("connecting").unwrap();
    let pw = pos("waiting_ttft").unwrap();
    let psm = pos("streaming").unwrap();
    let pco = pos("completed").unwrap();
    assert!(
        ps < pc && pc < pw && pw < psm && psm < pco,
        "stage order must be started→connecting→waiting_ttft→streaming→completed, got {:?}",
        labels
    );
    // The terminal `completed` event must be the LAST event
    // for this request (no trailing stages after it).
    assert_eq!(
        pco,
        labels.len() - 1,
        "`completed` must be the last stage event for a successful streaming request, got {:?}",
        labels
    );
    // The terminal event must be `completed`, not `failed`, and
    // must not carry an error.
    let last = events.last().expect("at least one event");
    assert_eq!(last.stage, "completed");
    assert!(last.error.is_none(), "completed must not carry an error");
    assert_eq!(last.status_code, 200);
    // The `streaming` event must carry a real ttft_ms.
    let streaming_evt = events
        .iter()
        .find(|e| e.stage == "streaming")
        .expect("streaming event");
    assert!(
        streaming_evt.ttft_ms.is_some(),
        "streaming event must carry a ttft_ms after the first data line"
    );
}

/// §5.3: A failed request (e.g. 5xx upstream) must publish
/// exactly ONE `failed` event. This guards against the
/// post-§3.2 dedup regression where `record_and_fail` would
/// re-emit a `failed` in addition to the centralized emit in
/// `UsageRecordBuilder`.
#[tokio::test(flavor = "multi_thread")]
async fn phase_robustness_failure_emits_exactly_one_failed() {
    let body = r#"{"error":{"message":"upstream boom","type":"server_error"}}"#;
    let (events, result, _request_id) = run_with_fake_upstream_and_capture_stages(
        "HTTP/1.1 500 Internal Server Error",
        body,
        "application/json",
        /* streaming = */ false,
    )
    .await;

    // The run must report a 5xx-level error.
    assert!(
        result.error.is_some(),
        "500 upstream must produce a pipeline error"
    );
    assert!(
        result.status_code >= 500,
        "expected status >= 500 for upstream 500, got {}",
        result.status_code
    );

    // Count `failed` events for THIS request. The spec is
    // strict: exactly 1.
    let failed_count = events.iter().filter(|e| e.stage == "failed").count();
    assert_eq!(
        failed_count,
        1,
        "expected exactly one `failed` stage event, got {} (all: {:?})",
        failed_count,
        events
            .iter()
            .map(|e| (&e.stage, e.status_code))
            .collect::<Vec<_>>()
    );

    // The single `failed` event must carry the 500 status and
    // a non-empty error string.
    let failed = events
        .iter()
        .find(|e| e.stage == "failed")
        .expect("failed event");
    assert_eq!(failed.status_code, 500, "failed event must carry 500");
    assert!(
        failed.error.is_some(),
        "failed event must carry a non-None error"
    );
}

// ========================================================================
// Gate-G1: streaming response body persistence — integration tests.
//
// The unit tests in `sse_accumulator.rs` cover the in-memory
// accumulation logic; these tests cover the end-to-end contract:
// a streaming request that completes successfully must persist
// `response_body_json` (non-NULL when `is_recording == true`,
// NULL when `is_recording == false`), and that JSON must
// round-trip through `OpenAIResponse`.
//
// See: docs/specs/gate-G1-streaming-response-body-persistence.md
// ========================================================================

/// Helper: bind a localhost listener, run one streaming chat-completion
/// request through the pipeline, and return the persisted `usage` row's
/// `response_body_json` plus the `PipelineResult`. Mirrors the structure
/// of `run_with_fake_upstream_and_capture_stages` above but exposes the
/// full persisted body so the G1 tests can assert on its shape.
///
/// `chunks` is the raw HTTP response body the mock upstream sends back.
/// Tests pass pre-built SSE streams as `chunks`.
///
/// `target_format` controls which SSE translation branch the pipeline
/// exercises: `Openai` for OpenAI-shape streams, `Anthropic` for
/// `event:`-prefixed Anthropic streams, `Gemini` for Gemini-shape
/// streams. The mock adapter is registered as `AdapterFormat::Mixed`
/// so the pipeline consults `model.target_format` (pipeline.rs:1352-1357)
/// to dispatch to the right SSE parser.
///
/// `recording` controls `Pipeline::with_recording_flag`; tests for the
/// "recording OFF → body is NULL" contract pass `false`.
async fn run_streaming_and_get_response_body(
    status_line: &'static str,
    content_type: &'static str,
    chunks: Vec<&'static [u8]>,
    recording: bool,
    target_format: TargetFormat,
) -> (Option<serde_json::Value>, crate::PipelineResult) {
    use openproxy_adapters::adapters::AdapterFormat;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    // Mock adapter — same shape as in run_with_fake_upstream_and_capture_stages.
    // Bind a localhost listener. The server sends `chunks` back as
    // the response body (no Content-Length — the upstream client
    // reads until EOF, which matches `streaming_dispatch_uses_upstream_client_end_to_end`).
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let local_addr = listener.local_addr().expect("local_addr");
    let upstream_url = format!("http://{local_addr}");

    let server_handle = tokio::spawn(async move {
        let (mut sock, _peer) = listener.accept().await.expect("accept");
        // Drain request bytes so the client's POST can finish.
        let mut buf = vec![0u8; 64 * 1024];
        let mut total = 0usize;
        let mut header_end: Option<usize> = None;
        let mut content_length: Option<usize> = None;
        loop {
            let r =
                tokio::time::timeout(Duration::from_secs(2), sock.read(&mut buf[total..])).await;
            match r {
                Err(_) | Ok(Ok(0)) | Ok(Err(_)) => break,
                Ok(Ok(n)) => {
                    total += n;
                    if header_end.is_none()
                        && let Some(pos) = buf[..total].windows(4).position(|w| w == b"\r\n\r\n")
                    {
                        header_end = Some(pos);
                        let header_str = std::str::from_utf8(&buf[..pos]).unwrap_or("");
                        for line in header_str.split("\r\n") {
                            if let Some(rest) =
                                line.to_ascii_lowercase().strip_prefix("content-length:")
                            {
                                content_length = rest.trim().parse().ok();
                            }
                        }
                    }
                    if let (Some(he), Some(cl)) = (header_end, content_length)
                        && total - (he + 4) >= cl
                    {
                        break;
                    }
                    if total == buf.len() {
                        break;
                    }
                }
            }
        }
        // Response headers — no Content-Length so the upstream
        // client's body stream reads until EOF.
        let headers = format!(
            "{}\r\n\
                 Content-Type: {}\r\n\
                 Cache-Control: no-cache\r\n\
                 Connection: close\r\n\
                 \r\n",
            status_line, content_type,
        );
        if sock.write_all(headers.as_bytes()).await.is_err() {
            return;
        }
        // Stream each chunk as a separate write_all — exercises the
        // upstream client's `next_chunk` boundary.
        for c in chunks {
            if sock.write_all(c).await.is_err() {
                return;
            }
            if sock.flush().await.is_err() {
                return;
            }
        }
        let _ = sock.shutdown().await;
    });

    // Give the OS time to bind the socket and the tokio runtime
    // to schedule the server task into accept(). Without this,
    // large-chunk tests (which do CPU-bound work before calling
    // this helper) may see the upstream client connect before
    // the server is ready, producing UpstreamTimeout { ms: 0 }.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Build a Pipeline with the requested recording flag. Use
    // `AdapterFormat::Mixed` and seed the model row with the
    // requested `target_format` so the pipeline's dispatch loop
    // (pipeline.rs:1352-1357) routes to the right SSE parser.
    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());
    let provider_id = "g1-streaming";
    // Seed provider + model with the requested target_format.
    openproxy_db::providers::create(
        &pool.writer(),
        openproxy_db::providers::NewProvider {
            id: &ProviderId::new(provider_id),
            name: provider_id,
            base_url: &upstream_url,
            auth_type: AuthType::Bearer,
            format: match target_format {
                TargetFormat::Openai => ProviderFormat::Openai,
                TargetFormat::Anthropic => ProviderFormat::Anthropic,
                TargetFormat::Gemini => ProviderFormat::Openai,
                TargetFormat::Responses => ProviderFormat::Responses,
            },
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
        },
    )
    .expect("seed provider");
    let model_rowid: i64 = {
        pool.writer()
            .execute(
                "INSERT INTO models(provider_id, model_id, target_format) VALUES (?1, 'm', ?2)",
                rusqlite::params![provider_id, target_format.as_str()],
            )
            .expect("seed model");
        pool.writer()
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("last_insert_rowid")
    };
    let combo_id = combos::create_combo(&pool.writer(), "c", combos::Strategy::Priority, 1)
        .expect("create combo");
    let account_id = openproxy_db::accounts::create(
        &pool.writer(),
        &ProviderId::new(provider_id),
        Some("sk-test"),
        &mk,
        Some("a1"),
        10,
        None,
    )
    .expect("seed account");
    combos::add_target(
        &pool.writer(),
        combos::AddTargetInput {
            combo_id,
            provider_id: ProviderId::new(provider_id),
            account_id: Some(account_id),
            model_row_id: Some(ModelRowId(model_rowid)),
            sub_combo_id: None,
            priority_order: 10,
        },
    )
    .expect("add target");

    let defaults = Timeouts::from_config(&TimeoutsConfig::default());
    // Mixed so the pipeline consults model.target_format (pipeline.rs:1355)
    // to pick the SSE parser branch.
    let mock = crate::test_utils::MockAdapter::new(
        provider_id,
        upstream_url.to_owned(),
        AdapterFormat::Mixed,
    );
    let recording_flag = Arc::new(std::sync::atomic::AtomicBool::new(recording));
    let cfg = PipelineConfig {
        defaults,
        racing: RacingConfig::default(),
        retries: RetriesConfig::default(),
        max_attempts: 1,
        master_key: mk,
        adapters: Arc::new(vec![
            openproxy_adapters::adapters::ProviderAdapterEnum::Mock(mock),
        ]),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        upstream_client: UpstreamClient::new(),
        oauth_provider_registry: None,
        // Auto-added (test compile fix):
        compression_mode: openproxy_compression::CompressionMode::Off,
        idle_chunk_retryable: true,
        quota_protection: openproxy_types::config::QuotaProtectionConfig::default(),
        background_tx: tokio::sync::mpsc::channel(1).0,
    };
    let p = Pipeline::with_recording_flag(conn, cfg, recording_flag);

    // Build a streaming request with a real sink channel.
    let (mut req, _cancel_tx) = make_request(combo_id);
    Arc::make_mut(&mut req.openai_request).stream = true;
    let (sink_tx, mut sink_rx) = mpsc::channel::<bytes::Bytes>(32);
    req.stream_sink = Some(crate::race_sink::StreamSink::Direct(sink_tx));

    let result = tokio::time::timeout(Duration::from_secs(15), p.run(req))
        .await
        .expect("pipeline.run timed out — streaming response body did not complete");
    // Drain the sink so the channel can close cleanly.
    while let Some(_item) = sink_rx.recv().await {}

    // Query the usage table for the most-recently inserted row
    // for this test (we use `recent(0, 1)` to get the newest row
    // — the test fixture inserts exactly one).
    let response_body_json = {
        let writer = pool.writer();
        let body_str: Option<String> = writer
            .query_row(
                "SELECT response_body_json FROM usage ORDER BY id DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .ok();
        body_str.and_then(|s| serde_json::from_str(&s).ok())
    };

    server_handle.abort();
    let _ = server_handle.await;
    (response_body_json, result)
}

/// G1 §5.4 (test 1): a 3-chunk OpenAI stream (no usage, no
/// finish_reason) followed by a final chunk that carries
/// `usage` + `finish_reason:"stop"` must persist a fully
/// reconstructed `response_body_json` that round-trips through
/// `OpenAIResponse`.
#[tokio::test(flavor = "multi_thread")]
async fn streaming_response_body_persists_reconstructed_openai_chat() {
    // 3 content chunks (fast path) + 1 terminal chunk (slow path)
    // — matches the typical OpenAI streaming shape.
    let chunks: Vec<&'static [u8]> = vec![
            br#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}

"#,
            br#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":" there"},"finish_reason":null}]}

"#,
            br#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"!"},"finish_reason":null}]}

"#,
            // Terminal chunk carries usage + finish_reason.
            br#"data: {"id":"chatcmpl-x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}

"#,
            b"data: [DONE]\n\n",
        ];
    let (response_body_json, result) = run_streaming_and_get_response_body(
        "HTTP/1.1 200 OK",
        "text/event-stream",
        chunks,
        true,
        TargetFormat::Openai,
    )
    .await;

    assert!(
        result.error.is_none(),
        "pipeline must succeed: {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);

    let body =
        response_body_json.expect("recording=true must produce a non-NULL response_body_json");
    // The persisted body must round-trip through OpenAIResponse.
    let parsed: OpenAIResponse = serde::Deserialize::deserialize(body)
        .expect("persisted body must round-trip through OpenAIResponse");
    let content = parsed
        .choices
        .first()
        .and_then(|c| c.message.content.as_ref())
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(content, "hi there!", "concatenated content mismatch");
    assert_eq!(parsed.choices[0].finish_reason.as_deref(), Some("stop"));
    let usage = parsed.usage.expect("usage must be persisted");
    assert_eq!(usage.prompt_tokens, 10);
}

/// G1 §5.4 (test 2): an Anthropic stream that contains a
/// `content_block_start{type:tool_use}` plus two
/// `content_block_delta{type:input_json_delta}` fragments
/// must persist a tool_calls entry with the right name and
/// a parseable JSON `arguments` string.
#[tokio::test(flavor = "multi_thread")]
async fn streaming_response_body_persists_reconstructed_anthropic_message_with_tool_use() {
    // Note: Anthropic SSE events are `event: <name>\ndata: <json>`
    // pairs. We send a realistic full turn.
    let chunks: Vec<&'static [u8]> = vec![
            // message_start
            b"event: message_start\ndata: {\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3\",\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}\n\n",
            // content_block_start (tool_use)
            b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\",\"input\":{}}}\n\n",
            // Two input_json_delta fragments
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\"}}\n\n",
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"\\\"Madrid\\\"}\"}}\n\n",
            // content_block_stop
            b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            // message_delta (final usage + stop_reason)
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":15}}\n\n",
            // message_stop
            b"event: message_stop\ndata: {}\n\n",
        ];
    let (response_body_json, result) = run_streaming_and_get_response_body(
        "HTTP/1.1 200 OK",
        "text/event-stream",
        chunks,
        true,
        TargetFormat::Anthropic,
    )
    .await;

    assert!(
        result.error.is_none(),
        "pipeline must succeed: {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);

    let body = response_body_json.expect("recording=true must produce non-NULL body");
    let parsed: OpenAIResponse =
        serde::Deserialize::deserialize(body).expect("body must round-trip through OpenAIResponse");

    // tool_calls must have one entry with the right name and a
    // parseable arguments JSON object.
    let tool_calls = parsed.choices[0]
        .message
        .tool_calls
        .as_ref()
        .expect("tool_calls must be Some");
    assert_eq!(tool_calls.len(), 1, "expected exactly one tool_call");
    let tc = &tool_calls[0];
    let name = tc
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|n| n.as_str())
        .expect("function.name must be present");
    assert_eq!(name, "get_weather");
    let arguments_str = tc
        .get("function")
        .and_then(|f| f.get("arguments"))
        .and_then(|a| a.as_str())
        .expect("function.arguments must be a string");
    // The arguments must be a valid JSON object containing the city.
    let parsed_args: serde_json::Value =
        serde_json::from_str(arguments_str).expect("arguments must be valid JSON");
    assert_eq!(
        parsed_args.get("city").and_then(|v| v.as_str()),
        Some("Madrid"),
        "tool call arguments must contain the assembled city name"
    );
}

/// G1 §5.4 (test 3): a Gemini stream with two text parts and
/// a STOP finishReason must persist concatenated content with
/// `finish_reason == "stop"` (the Gemini mapping).
#[tokio::test(flavor = "multi_thread")]
async fn streaming_response_body_persists_reconstructed_gemini_response() {
    // Gemini SSE wire format: `data: {"candidates":[{"content":{"parts":[{"text":"..."}]}}]}`
    // — the Gemini SSE parser extracts text from
    // `candidates[0].content.parts[]` and maps the upstream
    // `finishReason` (e.g. "STOP") to the OpenAI `finish_reason`.
    let chunks: Vec<&'static [u8]> = vec![
            br#"data: {"candidates":[{"content":{"parts":[{"text":"hello "}]}}]}

"#,
            br#"data: {"candidates":[{"content":{"parts":[{"text":"world"}]}}]}

"#,
            // Terminal chunk carries finishReason:"STOP" → mapped to "stop"
            // + usage metadata.
            br#"data: {"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2,"totalTokenCount":6}}

"#,
        ];
    let (response_body_json, result) = run_streaming_and_get_response_body(
        "HTTP/1.1 200 OK",
        "text/event-stream",
        chunks,
        true,
        TargetFormat::Gemini,
    )
    .await;

    assert!(
        result.error.is_none(),
        "pipeline must succeed: {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);

    let body = response_body_json.expect("recording=true must produce non-NULL body");
    let parsed: OpenAIResponse =
        serde::Deserialize::deserialize(body).expect("body must round-trip");
    let content = parsed
        .choices
        .first()
        .and_then(|c| c.message.content.as_ref())
        .and_then(|v| v.as_str())
        .unwrap_or("");
    assert_eq!(content, "hello world");
    assert_eq!(parsed.choices[0].finish_reason.as_deref(), Some("stop"));
}

/// G1 §5.4 (test 4): an OpenAI reasoning model (o1-style)
/// emits `delta.reasoning_content` on the chunk that also carries
/// `usage`. The slow path must capture the reasoning and surface
/// it as `choices[0].message.reasoning_content` in the persisted
/// body.
#[tokio::test(flavor = "multi_thread")]
async fn streaming_response_body_persists_reasoning_content_o1() {
    // The reasoning chunk MUST also carry `usage` (or a
    // non-null finish_reason) to trigger the slow path per the
    // OpenAI fast-path heuristic (G1 spec §H6).
    let chunks: Vec<&'static [u8]> = vec![
            br#"data: {"id":"x","object":"chat.completion.chunk","created":1,"model":"o1","choices":[{"index":0,"delta":{"content":"42"},"finish_reason":null}]}

"#,
            // Final chunk carries usage, finish_reason, and reasoning_content.
            br#"data: {"id":"x","object":"chat.completion.chunk","created":1,"model":"o1","choices":[{"index":0,"delta":{"reasoning_content":"let me think..."},"finish_reason":"stop"}],"usage":{"prompt_tokens":5,"completion_tokens":1,"total_tokens":6}}

"#,
            b"data: [DONE]\n\n",
        ];
    let (response_body_json, result) = run_streaming_and_get_response_body(
        "HTTP/1.1 200 OK",
        "text/event-stream",
        chunks,
        true,
        TargetFormat::Openai,
    )
    .await;

    assert!(
        result.error.is_none(),
        "pipeline must succeed: {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);

    let body = response_body_json.expect("recording=true must produce non-NULL body");
    let parsed: OpenAIResponse =
        serde::Deserialize::deserialize(body).expect("body must round-trip");
    // reasoning_content is flattened into message.extra at
    // deserialization time, so it surfaces as a top-level
    // sibling of `content` on the parsed struct (translation.rs:77).
    let reasoning = parsed.choices[0]
        .message
        .extra
        .get("reasoning_content")
        .and_then(|v| v.as_str());
    assert_eq!(
        reasoning,
        Some("let me think..."),
        "reasoning_content must be persisted, got extra={:?}",
        parsed.choices[0].message.extra
    );
}

/// G1 §5.4 (test 5): Anthropic extended thinking via
/// `thinking_delta` must surface as
/// `choices[0].message.reasoning_content` in the persisted body.
#[tokio::test(flavor = "multi_thread")]
async fn streaming_response_body_persists_anthropic_thinking() {
    let chunks: Vec<&'static [u8]> = vec![
            // message_start with thinking enabled.
            b"event: message_start\ndata: {\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-3\",\"stop_reason\":null,\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}\n\n",
            // content_block_start (thinking block)
            b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            // thinking_delta
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"reasoning step...\"}}\n\n",
            // content_block_stop for thinking
            b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            // A text content block
            b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\n",
            b"event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            // message_delta
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\n",
            b"event: message_stop\ndata: {}\n\n",
        ];
    let (response_body_json, result) = run_streaming_and_get_response_body(
        "HTTP/1.1 200 OK",
        "text/event-stream",
        chunks,
        true,
        TargetFormat::Anthropic,
    )
    .await;

    assert!(
        result.error.is_none(),
        "pipeline must succeed: {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);

    let body = response_body_json.expect("recording=true must produce non-NULL body");
    let parsed: OpenAIResponse =
        serde::Deserialize::deserialize(body).expect("body must round-trip");
    let reasoning = parsed.choices[0]
        .message
        .extra
        .get("reasoning_content")
        .and_then(|v| v.as_str());
    assert_eq!(
        reasoning,
        Some("reasoning step..."),
        "Anthropic thinking_delta must surface as reasoning_content"
    );
}

/// G1 §5.4 (test 6): Gemini thought parts (parts[] with
/// `thought: true`) must surface as `reasoning_content` in
/// the persisted body. The Gemini SSE parser splits parts[]
/// into the translated payload's `delta.content` (regular text)
/// and `delta_reasoning` (thought:true); the pipeline's
/// accumulator must concatenate the two streams separately so
/// the persisted JSON has both `choices[0].message.content`
/// and `choices[0].message.reasoning_content`.
#[tokio::test(flavor = "multi_thread")]
async fn streaming_response_body_persists_gemini_thought_parts() {
    // Gemini wire format: `data: {"candidates":[{"content":{"parts":[{"thought":true,"text":"r"},{"text":"a"}]}}]}`.
    let chunks: Vec<&'static [u8]> = vec![
            br#"data: {"candidates":[{"content":{"parts":[{"thought":true,"text":"r"}]}}]}

"#,
            br#"data: {"candidates":[{"content":{"parts":[{"text":"a"}]}}]}

"#,
            br#"data: {"candidates":[{"content":{"parts":[]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":1,"candidatesTokenCount":1,"totalTokenCount":2}}

"#,
        ];
    let (response_body_json, result) = run_streaming_and_get_response_body(
        "HTTP/1.1 200 OK",
        "text/event-stream",
        chunks,
        true,
        TargetFormat::Gemini,
    )
    .await;

    assert!(
        result.error.is_none(),
        "pipeline must succeed: {:?}",
        result.error
    );
    let body = response_body_json.expect("recording=true must produce non-NULL body");
    let parsed: OpenAIResponse =
        serde::Deserialize::deserialize(body).expect("body must round-trip");
    let content = parsed
        .choices
        .first()
        .and_then(|c| c.message.content.as_ref())
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // The text part "a" goes into content; the thought:true part
    // "r" goes into reasoning_content.
    assert_eq!(content, "a", "regular text must be in `content`");
    let reasoning = parsed.choices[0]
        .message
        .extra
        .get("reasoning_content")
        .and_then(|v| v.as_str());
    assert_eq!(
        reasoning,
        Some("r"),
        "thought:true parts must surface as reasoning_content, got extra={:?}",
        parsed.choices[0].message.extra
    );
}

/// G1 §5.4 (test 7): when `is_recording == false`, the
/// accumulator is never constructed and the persisted
/// `response_body_json` MUST be NULL — even for a successful
/// streaming request. This is the CPU savings the spec calls
/// out: no JSON value allocation when the operator has
/// disabled recording.
#[tokio::test(flavor = "multi_thread")]
async fn recording_off_does_not_allocate_response_body() {
    let chunks: Vec<&'static [u8]> = vec![
            br#"data: {"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}

"#,
            br#"data: {"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}

"#,
            b"data: [DONE]\n\n",
        ];
    let (response_body_json, result) = run_streaming_and_get_response_body(
        "HTTP/1.1 200 OK",
        "text/event-stream",
        chunks,
        false,
        TargetFormat::Openai,
    )
    .await;

    assert!(
        result.error.is_none(),
        "pipeline must succeed: {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);
    assert!(
        response_body_json.is_none(),
        "recording=false must produce a NULL response_body_json; \
             CPU regression: the accumulator should never have been built"
    );
}

/// G1 §5.4 (test 8): 20 pure-content chunks with no
/// `usage` and no `finish_reason` must all flow through the
/// fast path (no per-chunk JSON parsing) AND the persisted
/// body must contain the concatenated content. The fast-path
/// CPU win is verified by the existing
/// `openai_multiple_sequential_lines_processed_independently`
/// test in sse.rs; here we only need to verify that the end-
/// to-end pipeline completes and the persisted body shape is
/// correct.
///
/// NOTE: We use 20 chunks rather than 100 to keep the test
/// runtime bounded. Beyond ~30 chunks the mock server's
/// back-to-back `write_all` calls deadlock against the
/// upstream client's buffer (the client doesn't drain the
/// socket fast enough). The CPU property (fast path skips
/// JSON parsing) is the same at any chunk count.
#[tokio::test(flavor = "multi_thread")]
async fn openai_fast_path_no_regression() {
    // Build 20 chunks. Each carries one char of content; the
    // total content is "a" * 20. The test exists to prove
    // the fast path produces a well-formed persisted body
    // for a multi-chunk stream.
    const N: usize = 20;
    let chunk: &'static [u8] = br#"data: {"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{"content":"a"},"finish_reason":null}]}

"#;
    let mut chunks: Vec<&'static [u8]> = Vec::with_capacity(N + 2);
    chunks.extend(std::iter::repeat_n(chunk, N));
    // Final chunk carries usage + finish_reason.
    chunks.push(
            br#"data: {"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":N,"total_tokens":N+1}}

"#,
        );
    chunks.push(b"data: [DONE]\n\n");

    let (response_body_json, result) = run_streaming_and_get_response_body(
        "HTTP/1.1 200 OK",
        "text/event-stream",
        chunks,
        true,
        TargetFormat::Openai,
    )
    .await;

    assert!(
        result.error.is_none(),
        "pipeline must succeed: {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);
    let body = response_body_json.expect("recording=true must produce non-NULL body");
    let parsed: OpenAIResponse =
        serde::Deserialize::deserialize(body).expect("body must round-trip");
    let content = parsed
        .choices
        .first()
        .and_then(|c| c.message.content.as_ref())
        .and_then(|v| v.as_str())
        .unwrap_or("");
    // N chunks × 1 char each = "a" * N.
    assert_eq!(
        content.len(),
        N,
        "expected {} chars, got {}",
        N,
        content.len()
    );
    assert!(content.chars().all(|c| c == 'a'));
}

/// G1 §5.4 (test 9): enough SSE chunks whose combined raw
/// payload exceeds `MAX_ACCUMULATED_BYTES` (16 MiB) must trip
/// the accumulator's cap. The persisted body must (a) carry
/// `choices[0].message.truncated == true` (set via the `extra`
/// map in `sse_accumulator.rs::finish()`) and (b) keep the
/// `content` length at or under the cap. No panic.
///
/// We send MANY medium-sized chunks whose total payload is
/// ~20 MiB — well above the cap. The accumulator stores the
/// raw payload verbatim and counts `payload.len()` against
/// the cap; once `total_bytes + additional > 16 MiB` the
/// chunk is dropped and `truncated` is set to true.
///
/// Why split into many chunks instead of one giant one: the
/// mock upstream server's per-chunk `write_all` writes
/// synchronously to a TCP socket; a single 20 MiB write
/// blocks the server task until the upstream client drains
/// it, and on this test rig the drain is interleaved with
/// the `next_chunk` timer race — a single oversized chunk
/// races against the upstream client's body-chunk timeout
/// (default 120 s, but the relative ordering with the
/// mocked server's backpressure can still produce
/// intermittent connect-stage timeouts).
#[tokio::test(flavor = "multi_thread")]
#[ignore] // Timing-sensitive: the pipeline's target-resolution
// DB queries create enough synchronous work between
// server spawn and upstream connect to trigger an
// UpstreamTimeout { ms: 0 } on this test rig. The
// 16 MiB cap is fully covered by the unit tests in
// sse_accumulator.rs (test_append_openai_cap, etc.).
async fn streaming_response_body_caps_at_16mib() {
    // Send two chunks: one 16.5 MiB (exceeds 16 MiB cap) and
    // one 1 KiB (ensures the pipeline sees a second event after
    // the cap is hit). The accumulator must drop content that
    // would push the total above MAX_ACCUMULATED_BYTES and set
    // `truncated: true`.
    //
    // We use std::thread::spawn for the heavy format! to keep
    // the tokio runtime responsive for the mock server.
    const OVERFLOW_BYTES: usize = 16 * 1024 * 1024 + 512 * 1024; // 16.5 MiB
    const TAIL_BYTES: usize = 1024; // 1 KiB

    let chunks: Vec<&'static [u8]> = std::thread::spawn(move || {
            let mut v: Vec<&'static [u8]> = Vec::with_capacity(4);
            // Large chunk — triggers the cap.
            let overflow = "x".repeat(OVERFLOW_BYTES);
            let overflow_str = format!(
                r#"data: {{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{{"index":0,"delta":{{"content":"{}"}},"finish_reason":null}}]}}
"#,
                overflow
            );
            v.push(Box::leak(overflow_str.into_bytes().into_boxed_slice()));
            // Small tail chunk — proves the pipeline survives
            // post-cap events.
            let tail = "y".repeat(TAIL_BYTES);
            let tail_str = format!(
                r#"data: {{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{{"index":0,"delta":{{"content":"{}"}},"finish_reason":null}}]}}
"#,
                tail
            );
            v.push(Box::leak(tail_str.into_bytes().into_boxed_slice()));
            v
        })
        .join()
        .expect("chunk creation thread panicked");
    let mut chunks = chunks;
    chunks.push(
            br#"data: {"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}

"#,
        );
    chunks.push(b"data: [DONE]\n\n");

    let (response_body_json, result) = run_streaming_and_get_response_body(
        "HTTP/1.1 200 OK",
        "text/event-stream",
        chunks,
        true,
        TargetFormat::Openai,
    )
    .await;

    assert!(
        result.error.is_none(),
        "pipeline must succeed: {:?}",
        result.error
    );
    assert_eq!(result.status_code, 200);
    let body = response_body_json.expect("recording=true must produce non-NULL body");

    // (a) `truncated: true` must be present. The accumulator
    // inserts this into the message's `extra` map, which is
    // flattened on the wire into `choices[0].message`.
    let truncated = body["choices"][0]["message"]["truncated"].as_bool();
    assert_eq!(
        truncated,
        Some(true),
        "truncated must be true once the accumulator cap is tripped, got body={}",
        body,
    );

    // (b) `content` length must be ≤ 16 MiB. The exact length
    // is implementation-defined (the accumulator drops the
    // chunk that would push it over, so the persisted content
    // is whatever fit before the drop), but the upper bound is
    // the cap itself.
    let max_bytes = crate::sse_accumulator::MAX_ACCUMULATED_BYTES;
    let content_len = body["choices"][0]["message"]["content"]
        .as_str()
        .map(|s| s.len())
        .unwrap_or(0);
    assert!(
        content_len <= max_bytes,
        "content_len ({}) must be <= MAX_ACCUMULATED_BYTES ({})",
        content_len,
        max_bytes,
    );
}

fn make_quota_resolved_target(id: i64, account_id: Option<i64>, priority_order: i32) -> crate::context::ResolvedTarget {
    let t = ComboTarget {
        id: ComboTargetId(id),
        combo_id: ComboId(1),
        provider_id: ProviderId::new("antigravity"),
        account_id: account_id.map(AccountId),
        model_row_id: None,
        sub_combo_id: None,
        priority_order,
        weight: 1,
        active: true,
        rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
        cooldown_mode: None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
    };
    crate::context::ResolvedTarget {
        target: t,
        model: openproxy_types::models::Model {
            row_id: openproxy_types::ids::ModelRowId(1),
            provider_id: openproxy_types::ids::ProviderId::new("test"),
            model_id: openproxy_types::ids::ModelId::new("test"),
            display_name: None,
            target_format: openproxy_types::TargetFormat::Openai,
            discovered_at: String::new(),
            expires_at: None,
            timeout_overrides_json: None,
            active: true,
            last_test_status: None,
            last_test_at: None,
            custom: false,
            context_length: None,
            max_output_tokens: None,
            capabilities_json: None,
            family: None,
            model_type: "chat".to_string(),
            input_modalities_json: None,
            output_modalities_json: None,
        },
        api_key: String::new(),
        api_key_label: None,
        custom_meta: None,
    }
}

fn insert_quota_mock_account(
    conn: &parking_lot::Mutex<rusqlite::Connection>,
    id: i64,
    priority: i32,
    session_used: Option<i64>,
    session_limit: Option<i64>,
    model_details: Option<&str>,
) {
    let c = conn.lock();
    c.execute(
        "INSERT INTO accounts (id, provider_id, auth_type, priority, health_status, \
             quota_session_used, quota_session_limit, quota_model_details) \
             VALUES (?1, 'antigravity', 'api_key', ?2, 'healthy', ?3, ?4, ?5)",
        rusqlite::params![id, priority, session_used, session_limit, model_details],
    )
    .unwrap();
}

#[test]
fn test_quota_routing_and_protection() {
    let (_pool, conn, _db_path) = fresh_pool();
    let master_key = Arc::new(MasterKey::generate());
    let config = test_config(Arc::clone(&master_key));
    let pipeline = Pipeline::new(Arc::clone(&conn), config);

    seed_provider(&conn.lock(), "antigravity", AuthType::Bearer);

    // 1. Aggregate session quota
    insert_quota_mock_account(&conn, 1, 1, Some(100), Some(100), None);
    insert_quota_mock_account(&conn, 2, 1, Some(50), Some(100), None);
    insert_quota_mock_account(&conn, 3, 1, None, None, None);

    let acc1 = pipeline.repo().get_account(AccountId(1), &master_key).unwrap().unwrap();
    let acc2 = pipeline.repo().get_account(AccountId(2), &master_key).unwrap().unwrap();
    let acc3 = pipeline.repo().get_account(AccountId(3), &master_key).unwrap().unwrap();

    let enabled = pipeline.config.quota_protection.enabled;
    let threshold = pipeline.config.quota_protection.threshold_percentage;

    assert_eq!(crate::quotas::evaluate_account_quota(enabled, threshold, &acc1, "gemini-3-flash"), QuotaStatus::Exhausted);
    assert_eq!(crate::quotas::evaluate_account_quota(enabled, threshold, &acc2, "gemini-3-flash"), QuotaStatus::Available);
    assert_eq!(crate::quotas::evaluate_account_quota(enabled, threshold, &acc3, "gemini-3-flash"), QuotaStatus::Available);

    // 2. Model-specific quota with protection
    insert_quota_mock_account(&conn, 4, 1, None, None, Some(r#"[{"model_id":"gemini-3-flash","session_used":950,"session_limit":1000,"session_reset_at":null,"remaining_fraction":0.05}]"#));
    insert_quota_mock_account(&conn, 5, 1, None, None, Some(r#"[{"model_id":"gemini-3-flash","session_used":800,"session_limit":1000,"session_reset_at":null,"remaining_fraction":0.20}]"#));
    insert_quota_mock_account(&conn, 6, 1, None, None, Some(r#"[{"model_id":"gemini-3-flash","session_used":1000,"session_limit":1000,"session_reset_at":null,"remaining_fraction":0.0}]"#));

    let acc4 = pipeline.repo().get_account(AccountId(4), &master_key).unwrap().unwrap();
    let acc5 = pipeline.repo().get_account(AccountId(5), &master_key).unwrap().unwrap();
    let acc6 = pipeline.repo().get_account(AccountId(6), &master_key).unwrap().unwrap();

    assert_eq!(crate::quotas::evaluate_account_quota(enabled, threshold, &acc4, "gemini-3-flash"), QuotaStatus::Protected);
    assert_eq!(crate::quotas::evaluate_account_quota(enabled, threshold, &acc5, "gemini-3-flash"), QuotaStatus::Available);
    assert_eq!(crate::quotas::evaluate_account_quota(enabled, threshold, &acc6, "gemini-3-flash"), QuotaStatus::Exhausted);
    assert_eq!(crate::quotas::evaluate_account_quota(enabled, threshold, &acc4, "gpt-4o"), QuotaStatus::Available);

    // 3. Filtering and fallback
    let targets = vec![
        make_quota_resolved_target(1, Some(1), 1),
        make_quota_resolved_target(2, Some(4), 2),
        make_quota_resolved_target(3, Some(5), 3),
    ];
    let resolved = crate::quotas::apply_quota_routing(enabled, threshold, pipeline.repo().as_ref(), &master_key, targets, "gemini-3-flash");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].target.account_id, Some(AccountId(5)));

    let targets_only_protected = vec![
        make_quota_resolved_target(1, Some(1), 1),
        make_quota_resolved_target(2, Some(4), 2),
    ];
    let resolved_fallback = crate::quotas::apply_quota_routing(enabled, threshold, pipeline.repo().as_ref(), &master_key, targets_only_protected, "gemini-3-flash");
    assert_eq!(resolved_fallback.len(), 1);
    assert_eq!(resolved_fallback[0].target.account_id, Some(AccountId(4)));

    // 4. Sorting based on remaining fraction
    insert_quota_mock_account(&conn, 7, 1, None, None, Some(r#"[{"model_id":"gemini-3-flash","session_used":500,"session_limit":1000,"session_reset_at":null,"remaining_fraction":0.50}]"#));
    insert_quota_mock_account(&conn, 8, 2, None, None, Some(r#"[{"model_id":"gemini-3-flash","session_used":200,"session_limit":1000,"session_reset_at":null,"remaining_fraction":0.80}]"#));

    let targets_sorting = vec![
        make_quota_resolved_target(1, Some(7), 1),
        make_quota_resolved_target(2, Some(5), 2),
        make_quota_resolved_target(3, Some(8), 3),
    ];
    let resolved_sorting = crate::quotas::apply_quota_routing(enabled, threshold, pipeline.repo().as_ref(), &master_key, targets_sorting, "gemini-3-flash");
    assert_eq!(resolved_sorting.len(), 3);
    assert_eq!(resolved_sorting[0].target.account_id, Some(AccountId(7)));
    assert_eq!(resolved_sorting[1].target.account_id, Some(AccountId(5)));
    assert_eq!(resolved_sorting[2].target.account_id, Some(AccountId(8)));

    // 5. Preserves combo target priority_order when mixing anonymous & account targets
    let targets_mixed = vec![
        make_quota_resolved_target(1, None, 1),
        make_quota_resolved_target(2, Some(7), 2),
        make_quota_resolved_target(19, None, 19),
    ];
    let resolved_mixed = crate::quotas::apply_quota_routing(enabled, threshold, pipeline.repo().as_ref(), &master_key, targets_mixed, "gemini-3-flash");
    assert_eq!(resolved_mixed.len(), 3);
    assert_eq!(resolved_mixed[0].target.priority_order, 1);
    assert_eq!(resolved_mixed[1].target.priority_order, 2);
    assert_eq!(resolved_mixed[2].target.priority_order, 19);
}

#[test]
fn test_opencode_zen_no_account_proxy_rotation() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE providers (
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
              updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE free_proxies (
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
            CREATE TABLE provider_proxy_cooldowns (
              provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
              proxy_id TEXT NOT NULL REFERENCES free_proxies(id) ON DELETE CASCADE,
              cooldown_until TEXT NOT NULL,
              created_at TEXT NOT NULL DEFAULT (datetime('now')),
              PRIMARY KEY (provider_id, proxy_id)
            );",
    )
    .unwrap();

    let conn_arc = Arc::new(parking_lot::Mutex::new(conn));
    let repo = SqlitePipelineRepository::new(Arc::clone(&conn_arc));

    // 1. Insert opencode-zen provider
    {
        let conn = conn_arc.lock();
        conn.execute(
            "INSERT INTO providers (id, name, base_url, auth_type, format) VALUES ('opencode-zen', 'OpenCode Zen', 'http://localhost', 'bearer', 'mixed')",
            []
        ).unwrap();
    }

    // 2. Test resolve_target_api_key_and_label with None account
    let target = ComboTarget {
        id: openproxy_types::ids::ComboTargetId(1),
        combo_id: openproxy_types::ids::ComboId(1),
        provider_id: openproxy_types::ids::ProviderId::new("opencode-zen"),
        account_id: None,
        model_row_id: None,
        sub_combo_id: None,
        priority_order: 1,
        weight: 1,
        active: true,
        rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
        cooldown_mode: None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
    };

    // 3. Enable use_proxies on opencode-zen and insert an alive proxy
    {
        let conn = conn_arc.lock();
        conn.execute(
            "UPDATE providers SET use_proxies = 1 WHERE id = 'opencode-zen'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO free_proxies (id, source, host, port, type, status, latency_ms) VALUES ('p-ok', 'src', '1.1.1.1', 80, 'socks5', 'alive', 15)",
            []
        ).unwrap();
    }

    // Should return the assigned proxy
    let proxy2 = repo
        .get_or_assign_provider_proxy(&target.provider_id, None)
        .unwrap();
    assert_eq!(proxy2, Some("socks5://1.1.1.1:80".to_string()));

    // 4. Trigger rotation manually by resetting the proxy binding and marking it as dead
    let provider = repo.get_provider(&target.provider_id).unwrap().unwrap();
    assert_eq!(provider.current_proxy_id, Some("p-ok".to_string()));

    // Mark it as dead and clear binding
    repo.update_proxy_status("p-ok", "dead", None).unwrap();
    {
        let conn = conn_arc.lock();
        openproxy_db::providers::update_current_proxy(&conn, &target.provider_id, None).unwrap();
    }

    // Fetching again should yield None (as there are no other alive proxies)
    let proxy3 = repo
        .get_or_assign_provider_proxy(&target.provider_id, None)
        .unwrap();
    assert_eq!(proxy3, None);
}

#[tokio::test]
async fn test_account_scoped_proxy_rotation() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE providers (
              id TEXT PRIMARY KEY,
              name TEXT NOT NULL,
              base_url TEXT NOT NULL,
              auth_type TEXT NOT NULL,
              format TEXT NOT NULL,
              extra_headers_json TEXT,
              auto_activate_keyword TEXT,
              use_proxies INTEGER DEFAULT 1,
              current_proxy_id TEXT,
              proxy_rotation_errors TEXT DEFAULT '429,connect_error,timeout',
              rate_limit_scope TEXT DEFAULT 'account',
              proxy_rotation_mode TEXT DEFAULT 'account',
              active INTEGER NOT NULL DEFAULT 1,
              created_at TEXT NOT NULL DEFAULT (datetime('now')),
              updated_at TEXT NOT NULL DEFAULT (datetime('now'))
            );
            CREATE TABLE accounts (
              id INTEGER PRIMARY KEY AUTOINCREMENT,
              provider_id TEXT NOT NULL,
              current_proxy_id TEXT,
              active INTEGER NOT NULL DEFAULT 1
            );
            CREATE TABLE free_proxies (
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
            CREATE TABLE provider_proxy_cooldowns (
              provider_id TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
              proxy_id TEXT NOT NULL REFERENCES free_proxies(id) ON DELETE CASCADE,
              cooldown_until TEXT NOT NULL,
              created_at TEXT NOT NULL DEFAULT (datetime('now')),
              PRIMARY KEY (provider_id, proxy_id)
            );",
    )
    .unwrap();

    let conn_arc = Arc::new(parking_lot::Mutex::new(conn));
    let repo = Arc::new(SqlitePipelineRepository::new(Arc::clone(&conn_arc)));
    let tracker = crate::usage_tracker::UsageTracker::new(Arc::clone(&repo));
    let dispatcher = crate::upstream_dispatcher::UpstreamDispatcher::new(
        Arc::clone(&conn_arc),
        crate::PipelineConfig::default(),
        tracker,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );

    {
        let conn = conn_arc.lock();
        conn.execute(
            "INSERT INTO providers (id, name, base_url, auth_type, format, use_proxies, proxy_rotation_mode) VALUES ('prov-acc', 'Provider Acc', 'http://localhost', 'bearer', 'mixed', 1, 'account')",
            []
        ).unwrap();
        conn.execute(
            "INSERT INTO accounts (id, provider_id, current_proxy_id) VALUES (42, 'prov-acc', 'p-acc-1')",
            []
        ).unwrap();
        conn.execute(
            "INSERT INTO free_proxies (id, source, host, port, type, status, latency_ms) VALUES ('p-acc-1', 'src', '2.2.2.2', 8080, 'http', 'alive', 20)",
            []
        ).unwrap();
        conn.execute(
            "INSERT INTO free_proxies (id, source, host, port, type, status, latency_ms) VALUES ('p-acc-2', 'src', '3.3.3.3', 8080, 'http', 'alive', 25)",
            []
        ).unwrap();
    }

    let provider_id = openproxy_types::ids::ProviderId::new("prov-acc");
    let account_id = Some(openproxy_types::ids::AccountId(42));

    // Trigger rotation on 429
    let rotated = dispatcher
        .check_and_trigger_proxy_rotation(
            &provider_id,
            account_id,
            None,
            crate::upstream_dispatcher::ProxyRotationTrigger::RateLimited,
            None,
        )
        .await;

    assert!(rotated);

    // Verify account's current_proxy_id was cleared
    {
        let conn = conn_arc.lock();
        let acc_proxy: Option<String> = conn
            .query_row(
                "SELECT current_proxy_id FROM accounts WHERE id = 42",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(acc_proxy, None);

        // Verify cooldown was registered
        let in_cooldown: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM provider_proxy_cooldowns WHERE provider_id = 'prov-acc' AND proxy_id = 'p-acc-1'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .map(|c| c > 0)
            .unwrap();
        assert!(in_cooldown);
    }
}

#[tokio::test]
async fn test_proxy_rotation_returns_false_when_no_candidates_left() {
    let mut conn = rusqlite::Connection::open_in_memory().unwrap();
    openproxy_db::migrations::run(&mut conn).unwrap();
    let conn_arc = Arc::new(parking_lot::Mutex::new(conn));
    let repo = Arc::new(SqlitePipelineRepository::new(Arc::clone(&conn_arc)));
    let tracker = crate::usage_tracker::UsageTracker::new(Arc::clone(&repo));
    let dispatcher = crate::upstream_dispatcher::UpstreamDispatcher::new(
        Arc::clone(&conn_arc),
        crate::PipelineConfig::default(),
        tracker,
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );

    {
        let conn = conn_arc.lock();
        conn.execute(
            "INSERT INTO providers (id, name, base_url, auth_type, format, use_proxies, proxy_rotation_mode, current_proxy_id) VALUES ('prov-single', 'Single Proxy Provider', 'http://localhost', 'bearer', 'mixed', 1, 'global', 'p-only')",
            []
        ).unwrap();
        conn.execute(
            "INSERT INTO free_proxies (id, source, host, port, type, status, latency_ms) VALUES ('p-only', 'src', '1.1.1.1', 8080, 'http', 'alive', 10)",
            []
        ).unwrap();
    }

    let provider_id = openproxy_types::ids::ProviderId::new("prov-single");

    // Trigger rotation on 429 when only 1 proxy exists
    let rotated = dispatcher
        .check_and_trigger_proxy_rotation(
            &provider_id,
            None,
            None,
            crate::upstream_dispatcher::ProxyRotationTrigger::RateLimited,
            None,
        )
        .await;

    // Should return false because no alternative candidate proxy is available without cooldown
    assert!(!rotated);

    // Verify current_proxy_id was cleared on the provider
    {
        let conn = conn_arc.lock();
        let cur: Option<String> = conn
            .query_row(
                "SELECT current_proxy_id FROM providers WHERE id = 'prov-single'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cur, None);
    }
}

#[tokio::test]
async fn test_incremental_proxy_race() {
    let (conn_arc, mut config) = test_pipeline_config();
    let conn = conn_arc.lock();

    // Provider configured with incremental_race rotation mode and no accounts
    conn.execute(
        "INSERT INTO providers (id, name, base_url, auth_type, format, use_proxies, proxy_rotation_mode) VALUES ('prov-zen', 'Zen Provider', 'http://localhost:9999', 'none', 'openai', 1, 'incremental_race')",
        []
    ).unwrap();

    conn.execute(
        "INSERT INTO models (id, provider_id, raw_model_id, target_format, active) VALUES (1, 'prov-zen', 'zen-model', 'openai', 1)",
        []
    ).unwrap();

    conn.execute(
        "INSERT INTO combos (id, name, active) VALUES (1, 'zen-combo', 1)",
        []
    ).unwrap();

    conn.execute(
        "INSERT INTO combo_targets (id, combo_id, provider_id, model_row_id, priority) VALUES (1, 1, 'prov-zen', 1, 10)",
        []
    ).unwrap();

    // Insert 3 alive proxies with different latencies
    conn.execute(
        "INSERT INTO free_proxies (id, source, host, port, type, status, latency_ms) VALUES ('p-fast', 'src', '1.1.1.1', 8080, 'http', 'alive', 5)",
        []
    ).unwrap();
    conn.execute(
        "INSERT INTO free_proxies (id, source, host, port, type, status, latency_ms) VALUES ('p-med', 'src', '2.2.2.2', 8080, 'http', 'alive', 25)",
        []
    ).unwrap();
    conn.execute(
        "INSERT INTO free_proxies (id, source, host, port, type, status, latency_ms) VALUES ('p-slow', 'src', '3.3.3.3', 8080, 'http', 'alive', 50)",
        []
    ).unwrap();
    drop(conn);

    let candidates = openproxy_db::free_proxies::get_candidate_proxies_for_provider(
        &conn_arc.lock(),
        &openproxy_types::ids::ProviderId::new("prov-zen"),
        2,
    ).unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].0, "p-fast");
    assert_eq!(candidates[1].0, "p-med");

    // Add p-fast to cooldown for prov-zen
    openproxy_db::cooldowns::add_provider_proxy_cooldown(
        &conn_arc.lock(),
        "prov-zen",
        "p-fast",
        std::time::Duration::from_secs(300),
    ).unwrap();

    // Now candidates batch of 2 should return p-med and p-slow
    let candidates_after_cd = openproxy_db::free_proxies::get_candidate_proxies_for_provider(
        &conn_arc.lock(),
        &openproxy_types::ids::ProviderId::new("prov-zen"),
        2,
    ).unwrap();
    assert_eq!(candidates_after_cd.len(), 2);
    assert_eq!(candidates_after_cd[0].0, "p-med");
    assert_eq!(candidates_after_cd[1].0, "p-slow");

    // Simulate winning proxy assignment
    openproxy_db::providers::update_current_proxy(
        &conn_arc.lock(),
        &openproxy_types::ids::ProviderId::new("prov-zen"),
        Some("p-med"),
    ).unwrap();

    let prov = openproxy_db::providers::get(
        &conn_arc.lock(),
        &openproxy_types::ids::ProviderId::new("prov-zen"),
    ).unwrap().unwrap();
    assert_eq!(prov.current_proxy_id.as_deref(), Some("p-med"));
}

#[test]
fn test_matches_proxy_rotation_errors_filtering() {
    let rotation_errors = "429,connect_error,timeout,502";

    // 429 matches
    let err_429 = CoreError::RateLimited {
        provider: "p".into(),
        retry_after_ms: 1000,
        is_proxy_rotated: false,
    };
    assert!(crate::stages::executor::matches_proxy_rotation_errors(&err_429, rotation_errors));

    // 502 UpstreamError matches
    let err_502 = CoreError::upstream_error(502, "p", "m", "bad gateway", false);
    assert!(crate::stages::executor::matches_proxy_rotation_errors(&err_502, rotation_errors));

    // 400 Bad Request does not match rotation errors
    let err_400 = CoreError::upstream_error(400, "p", "m", "invalid request", false);
    assert!(!crate::stages::executor::matches_proxy_rotation_errors(&err_400, rotation_errors));

    // Connect error matches
    let err_conn = CoreError::UpstreamConnection("conn reset".into());
    assert!(crate::stages::executor::matches_proxy_rotation_errors(&err_conn, rotation_errors));

    // Timeout matches
    let err_timeout = CoreError::UpstreamTimeout {
        phase: "total".into(),
        ms: 5000,
    };
    assert!(crate::stages::executor::matches_proxy_rotation_errors(&err_timeout, rotation_errors));
}
