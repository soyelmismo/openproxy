//! Integration tests for the quota-aware Z.ai route preparation pipeline.
//!
//! These tests exercise the real `prepare_zai_route` / `prepare_zai_paid_fallback`
//! code paths against a `DbPool`-backed account whose quota pools are seeded
//! with fresh synthetic metadata, plus a mocked localhost upstream for the
//! dispatched inference request. No real Z.ai credentials or endpoints are
//! contacted: the Starter route is a fixed production constant asserted by
//! value, and the paid route is redirected to a loopback base URL.
//!
//! Credentials are synthetic sentinels; they are only ever compared for
//! equality/inequality and are never written to logs (tracing in the code
//! under test redacts them by construction).

use super::super::zai::{ZaiPreparedRoute, prepare_zai_paid_fallback, prepare_zai_route};
use crate::Pipeline;
use crate::context::ResolvedTarget;
use openproxy_adapters::ProviderAdapter;
use openproxy_adapters::adapters::zai::{ZCODE_STARTER_BASE_URL, is_zcode_entitlement_exhaustion};
use openproxy_db::MasterKey;
use openproxy_types::accounts::StoreOAuthTokensParams;
use openproxy_types::combos::ComboTarget;
use openproxy_types::context::CustomProviderMeta;
use openproxy_types::ids::{AccountId, ComboId, ComboTargetId, ModelId, ModelRowId, ProviderId};
use openproxy_types::models::Model;
use openproxy_types::providers::{AuthType, ProviderFormat, RateLimitScope};
use openproxy_types::quota::{AccountQuota, QuotaPool, QuotaPoolStatus, QuotaSource};
use openproxy_types::{CoreError, now_unix_secs_str};
use std::sync::Arc;

// Synthetic, non-secret sentinels. The concrete bytes are irrelevant; only the
// fact that the two credentials differ matters for isolation assertions.
const JWT_SENTINEL: &str = "synthetic-jwt.a.b";
const API_KEY_SENTINEL: &str = "synthetic-api-key";

const STARTER_MODEL: &str = "glm-5.3-flash";

fn starter_pool(status: QuotaPoolStatus, models: &[&str]) -> QuotaPool {
    let now = now_unix_secs_str();
    QuotaPool {
        id: "zcode-starter".into(),
        source: QuotaSource::ZcodeStarter,
        plan_name: Some("synthetic starter".into()),
        status,
        unit: "token".into(),
        used: Some(10),
        limit: Some(100),
        remaining: Some(90),
        reset_at: None,
        expires_at: Some("9999999999".into()),
        starts_at: None,
        model_ids: models.iter().map(|m| (*m).to_string()).collect(),
        model_details: None,
        fetch_error: None,
        last_fetched_at: now,
    }
}

fn paid_pool(status: QuotaPoolStatus) -> QuotaPool {
    let now = now_unix_secs_str();
    QuotaPool {
        id: "coding-plan".into(),
        source: QuotaSource::CodingPlan,
        plan_name: Some("synthetic coding plan".into()),
        status,
        unit: "percentage".into(),
        used: Some(10),
        limit: Some(100),
        remaining: Some(90),
        reset_at: None,
        expires_at: None,
        starts_at: None,
        model_ids: Vec::new(),
        model_details: None,
        fetch_error: None,
        last_fetched_at: now,
    }
}

/// Build a fresh pipeline whose single account is a `zai` account with
/// synthetic OAuth metadata (JWT + API key) and the supplied quota pools.
/// The pools carry a fresh `last_fetched_at`, so the routing refresh treats
/// the snapshot as within its 60s freshness window and performs no network
/// call to the real Z.ai quota endpoints.
fn pipeline_with_zai_account(tag: &str, pools: Vec<QuotaPool>) -> (Pipeline, AccountId) {
    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }
    let (pool, _conn, _path) = crate::test_utils::fresh_pool();
    let master_key = Arc::new(MasterKey::generate().expect("generate master key"));

    let provider = ProviderId::new("zai");
    let account_id;
    {
        let conn = pool.writer();
        openproxy_db::providers::create(
            &conn,
            openproxy_db::providers::NewProvider {
                id: &provider,
                name: "Z.ai (synthetic)",
                base_url: "https://api.z.ai/api/anthropic",
                auth_type: AuthType::OAuth,
                format: ProviderFormat::Anthropic,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: RateLimitScope::Account,
            },
        )
        .expect("seed zai provider");

        account_id = openproxy_db::accounts::create(
            &conn,
            &provider,
            None,
            &master_key,
            Some(tag),
            10,
            None,
        )
        .expect("seed zai account");

        let provider_specific = serde_json::json!({
            "zcode_jwt_token": JWT_SENTINEL,
            "api_key": API_KEY_SENTINEL,
        })
        .to_string();
        openproxy_db::accounts::store_oauth_tokens(
            &conn,
            account_id,
            &master_key,
            StoreOAuthTokensParams {
                access_token: JWT_SENTINEL,
                refresh_token: None,
                token_type: "Bearer",
                expires_at: Some("9999999999"),
                scope: None,
                provider_specific: Some(&provider_specific),
                email: None,
            },
        )
        .expect("store synthetic oauth metadata");

        openproxy_db::accounts::set_quota(
            &conn,
            account_id,
            &AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: None,
                last_fetched_at: now_unix_secs_str(),
                fetch_error: None,
                model_details: None,
                pools: Some(pools.into_boxed_slice()),
            },
        )
        .expect("seed synthetic quota pools");
    }

    let config = crate::test_utils::test_config(master_key);
    let pipeline = crate::test_utils::test_pipeline_with_pool(pool, config);
    (pipeline, account_id)
}

fn resolved_target(account_id: AccountId, model: &str) -> ResolvedTarget {
    ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(1),
            combo_id: ComboId(1),
            provider_id: ProviderId::new("zai"),
            account_id: Some(account_id),
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 1,
            weight: 1,
            active: true,
            rate_limit_scope: RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
            description: None,
        },
        model: Model {
            row_id: ModelRowId(1),
            provider_id: ProviderId::new("zai"),
            model_id: ModelId::new(model),
            target_format: openproxy_types::TargetFormat::Anthropic,
            active: true,
            ..Default::default()
        },
        api_key: API_KEY_SENTINEL.into(),
        api_key_label: None,
        custom_meta: Some(CustomProviderMeta {
            access_token: JWT_SENTINEL.into(),
            maybe_refresh: None,
            kiro_region: None,
            kiro_profile_arn: None,
            antigravity_project: None,
            antigravity_metadata: None,
            codex_workspace_id: None,
            claude_account_uuid: None,
            claude_metadata: None,
        }),
    }
}

// ---------------------------------------------------------------------------
// Pure route selection: Starter vs paid and credential isolation.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn starter_route_uses_fixed_production_url_and_jwt_not_paid_key() {
    let pools = vec![
        starter_pool(QuotaPoolStatus::Active, &[STARTER_MODEL]),
        paid_pool(QuotaPoolStatus::Active),
    ];
    let (pipeline, account_id) = pipeline_with_zai_account("starter-route", pools);
    let target = resolved_target(account_id, STARTER_MODEL);

    let prepared = prepare_zai_route(&pipeline, &target, "http://127.0.0.1:1", None)
        .await
        .expect("starter route prepared");

    assert_eq!(prepared.route.source, QuotaSource::ZcodeStarter);
    assert_eq!(
        prepared.route.url,
        format!("{ZCODE_STARTER_BASE_URL}/v1/messages"),
        "Starter must use the fixed production zcode endpoint"
    );
    assert_eq!(
        prepared.route.credential, JWT_SENTINEL,
        "Starter must carry the ZCode OAuth JWT"
    );
    assert_ne!(
        prepared.route.credential, API_KEY_SENTINEL,
        "Starter must never leak the paid Coding Plan API key"
    );
    assert!(
        !prepared.route.url.contains("127.0.0.1"),
        "Starter must not be redirected to the paid loopback base"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn paid_route_when_starter_absent_uses_local_base_and_api_key_only() {
    // Starter entitlement is absent, the paid plan is Active: the paid
    // adapter base is overridden to a loopback URL so no production request
    // is made, and the credential must be the Coding Plan API key alone.
    let pools = vec![
        starter_pool(QuotaPoolStatus::Absent, &[STARTER_MODEL]),
        paid_pool(QuotaPoolStatus::Active),
    ];
    let (pipeline, account_id) = pipeline_with_zai_account("paid-route", pools);
    let target = resolved_target(account_id, STARTER_MODEL);
    let paid_base = "http://127.0.0.1:9";

    let prepared = prepare_zai_route(&pipeline, &target, paid_base, None)
        .await
        .expect("paid route prepared");

    assert_eq!(prepared.route.source, QuotaSource::CodingPlan);
    assert_eq!(
        prepared.route.url,
        format!("{paid_base}/v1/messages"),
        "paid route must resolve against the supplied (loopback) base URL"
    );
    assert_eq!(
        prepared.route.credential, API_KEY_SENTINEL,
        "paid route must carry the Coding Plan API key"
    );
    assert_ne!(
        prepared.route.credential, JWT_SENTINEL,
        "paid route must never reuse the ZCode OAuth JWT"
    );
}

// ---------------------------------------------------------------------------
// Fail-closed routing: missing / unavailable Starter never spends paid quota.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn unavailable_starter_never_falls_back_to_paid() {
    let pools = vec![
        starter_pool(QuotaPoolStatus::Unavailable, &[]),
        paid_pool(QuotaPoolStatus::Active),
    ];
    let (pipeline, account_id) = pipeline_with_zai_account("starter-unavailable", pools);
    let target = resolved_target(account_id, STARTER_MODEL);

    let err = prepare_zai_route(&pipeline, &target, "http://127.0.0.1:9", None)
        .await
        .err()
        .expect("unavailable Starter must not authorize a paid charge");
    assert!(
        matches!(err, CoreError::ServiceUnavailable(_)),
        "expected ServiceUnavailable, got {err:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn missing_starter_pool_never_falls_back_to_paid() {
    // Only a Coding Plan pool exists; there is no verified Starter snapshot,
    // so spending paid quota would be an unverifiable side effect.
    let pools = vec![paid_pool(QuotaPoolStatus::Active)];
    let (pipeline, account_id) = pipeline_with_zai_account("starter-missing", pools);
    // A partial snapshot intentionally triggers a refresh in prepare_zai_route.
    // Test its fail-closed verdict without sending synthetic auth to the web.
    let key = Arc::clone(&pipeline.config.master_key);
    let account = pipeline
        .async_repo()
        .run(move |r| r.get_account(account_id, &key))
        .await
        .unwrap()
        .unwrap();
    let err = openproxy_adapters::adapters::zai::resolve_zai_inference_route(
        &account,
        JWT_SENTINEL,
        STARTER_MODEL,
        now_unix_secs_str().parse().unwrap(),
        "http://127.0.0.1:9",
    )
    .err()
    .expect("missing Starter snapshot must not authorize a paid charge");
    assert!(
        matches!(err, CoreError::ServiceUnavailable(_)),
        "expected ServiceUnavailable, got {err:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn stale_starter_fetch_error_never_falls_back_to_paid() {
    // The Starter pool carries a fetch error (stale/unreadable snapshot).
    // Unknown data must never read as "exhausted" and silently spend paid.
    let mut starter = starter_pool(QuotaPoolStatus::Unavailable, &[STARTER_MODEL]);
    starter.fetch_error = Some("synthetic stale snapshot".into());
    let pools = vec![starter, paid_pool(QuotaPoolStatus::Active)];
    let (pipeline, account_id) = pipeline_with_zai_account("starter-stale", pools);
    let target = resolved_target(account_id, STARTER_MODEL);

    let err = prepare_zai_route(&pipeline, &target, "http://127.0.0.1:9", None)
        .await
        .err()
        .expect("stale Starter snapshot must not authorize a paid charge");
    assert!(
        matches!(err, CoreError::ServiceUnavailable(_)),
        "expected ServiceUnavailable, got {err:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn exhausted_starter_with_usable_paid_routes_to_paid() {
    // A readable, definitive negative on Starter IS the one case that may
    // spend the independent paid plan.
    let pools = vec![
        starter_pool(QuotaPoolStatus::Exhausted, &[STARTER_MODEL]),
        paid_pool(QuotaPoolStatus::Active),
    ];
    let (pipeline, account_id) = pipeline_with_zai_account("starter-exhausted", pools);
    let target = resolved_target(account_id, STARTER_MODEL);

    let prepared = prepare_zai_route(&pipeline, &target, "http://127.0.0.1:9", None)
        .await
        .expect("exhausted Starter may route to paid");
    assert_eq!(prepared.route.source, QuotaSource::CodingPlan);
}

// ---------------------------------------------------------------------------
// Paid failure never blocks a usable Starter entitlement.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn paid_unavailable_does_not_block_usable_starter() {
    let pools = vec![
        starter_pool(QuotaPoolStatus::Active, &[STARTER_MODEL]),
        paid_pool(QuotaPoolStatus::Unavailable),
    ];
    let (pipeline, account_id) = pipeline_with_zai_account("paid-down", pools);
    let target = resolved_target(account_id, STARTER_MODEL);

    let prepared = prepare_zai_route(&pipeline, &target, "http://127.0.0.1:9", None)
        .await
        .expect("a broken paid pool must not take a healthy Starter offline");
    assert_eq!(prepared.route.source, QuotaSource::ZcodeStarter);
}

// ---------------------------------------------------------------------------
// Request-local paid fallback: only explicit depletion is eligible.
// ---------------------------------------------------------------------------

#[test]
fn waf_auth_and_generic_rate_limits_are_not_entitlement_exhaustion() {
    // 405 "unusual activity" WAF block, a bare auth 401, and a generic
    // 429 rate limit must all be ineligible for consuming paid quota.
    let waf = CoreError::upstream_error(
        405,
        "zai",
        STARTER_MODEL,
        r#"{"code":3012,"msg":"request has been blocked due to unusual activity."}"#,
        false,
    );
    let auth401 = CoreError::upstream_error(
        401,
        "zai",
        STARTER_MODEL,
        r#"{"error":{"code":"unauthorized"}}"#,
        false,
    );
    let generic429 = CoreError::upstream_error(
        429,
        "zai",
        STARTER_MODEL,
        r#"{"error":{"code":"rate_limit_exceeded"}}"#,
        false,
    );
    assert!(!is_zcode_entitlement_exhaustion(&waf));
    assert!(!is_zcode_entitlement_exhaustion(&auth401));
    assert!(!is_zcode_entitlement_exhaustion(&generic429));
}

#[tokio::test(flavor = "current_thread")]
async fn paid_fallback_requires_a_depletion_and_upgrades_a_starter_route() {
    // Positive control: with an Active paid pool and a Starter route already
    // selected, the request-local fallback re-resolves to Coding Plan only
    // after `exhaust_zcode_model` marks the Starter bucket exhausted.
    let mut other = starter_pool(QuotaPoolStatus::Active, &["glm-5.3"]);
    other.id = "other-starter-bucket".into();
    let paid = paid_pool(QuotaPoolStatus::Active);
    let pools = vec![
        starter_pool(QuotaPoolStatus::Active, &[STARTER_MODEL]),
        other.clone(),
        paid.clone(),
    ];
    let (pipeline, account_id) = pipeline_with_zai_account("fallback-upgrade", pools);
    let target = resolved_target(account_id, STARTER_MODEL);

    let mut prepared: ZaiPreparedRoute =
        prepare_zai_route(&pipeline, &target, "http://127.0.0.1:9", None)
            .await
            .expect("initial starter route");
    assert_eq!(prepared.route.source, QuotaSource::ZcodeStarter);

    prepare_zai_paid_fallback(
        &pipeline,
        &mut prepared,
        STARTER_MODEL,
        "http://127.0.0.1:9",
    )
    .await
    .expect("explicit depletion upgrades to paid");

    assert_eq!(prepared.route.source, QuotaSource::CodingPlan);
    assert_eq!(prepared.route.credential, API_KEY_SENTINEL);
    assert_eq!(prepared.route.url, "http://127.0.0.1:9/v1/messages");
    let key = Arc::clone(&pipeline.config.master_key);
    let account = pipeline
        .async_repo()
        .run(move |r| r.get_account(account_id, &key))
        .await
        .unwrap()
        .unwrap();
    let persisted = account.quota_pools.unwrap();
    assert_eq!(persisted[0].status, QuotaPoolStatus::Exhausted);
    assert_eq!(persisted[0].remaining, Some(0));
    assert_eq!(persisted[1], other);
    assert_eq!(persisted[2], paid);
}

// ---------------------------------------------------------------------------
// End-to-end: mocked localhost upstream, unary + streaming, auth header.
// ---------------------------------------------------------------------------

/// Minimal one-shot HTTP/1.1 mock server. Accepts a single connection,
/// captures the raw request text, and answers with `response`. Returns the
/// loopback base URL and a join handle yielding the captured request.
async fn spawn_mock(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("mock addr");
    let base = format!("http://127.0.0.1:{}", addr.port());
    let handle = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.expect("accept");
        let mut buf = vec![0u8; 8192];
        let n = socket.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;
        let _ = socket.shutdown().await;
        request
    });
    (base, handle)
}

#[tokio::test(flavor = "multi_thread")]
async fn full_pipeline_paid_dispatch_uses_selected_credential_and_local_route() {
    let body = serde_json::json!({
        "id":"msg_pipeline", "type":"message", "role":"assistant", "model":STARTER_MODEL,
        "content":[{"type":"text","text":"pipeline ok"}], "stop_reason":"end_turn",
        "usage":{"input_tokens":2,"output_tokens":3}
    })
    .to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut wire = Vec::new();
        loop {
            let mut chunk = [0; 4096];
            let n = socket.read(&mut chunk).await.unwrap();
            assert!(n > 0, "request ended early");
            wire.extend_from_slice(&chunk[..n]);
            if let Some(end) = wire.windows(4).position(|v| v == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&wire[..end]);
                let length = head
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if wire.len() >= end + 4 + length {
                    break;
                }
            }
        }
        socket.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8(wire).unwrap()
    });
    let pools = vec![
        starter_pool(QuotaPoolStatus::Absent, &[]),
        paid_pool(QuotaPoolStatus::Active),
    ];
    let (mut pipeline, account_id) = pipeline_with_zai_account("pipeline-wire", pools);
    pipeline.config.adapters = Arc::new(vec![
        openproxy_adapters::adapters::ProviderAdapterEnum::Zai(Box::new(
            openproxy_adapters::adapters::zai::ZaiAdapter::with_base_url(&base),
        )),
    ]);
    let mut target = resolved_target(account_id, STARTER_MODEL);
    target.model.capabilities_json =
        Some(serde_json::json!({"streaming":false}).to_string().into());
    let (mut request, _disconnect_guard) = crate::test_utils::make_request(ComboId(1));
    request.stream_sink = None;
    let result = pipeline.execute_test_target(request, target).await;
    assert_eq!(
        result.status_code, 200,
        "pipeline error: {:?}",
        result.error
    );
    assert!(result.error.is_none());
    let wire = tokio::time::timeout(std::time::Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert!(wire.starts_with("POST /v1/messages "));
    assert!(wire.contains(&format!("Bearer {API_KEY_SENTINEL}")));
    assert!(!wire.contains(JWT_SENTINEL));
    assert!(wire.contains("anthropic-version"));
    assert!(wire.contains(STARTER_MODEL));
}

#[tokio::test(flavor = "multi_thread")]
async fn unary_dispatch_sends_paid_api_key_header_and_reads_json_response() {
    let response = "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 89\r\nconnection: close\r\n\r\n\
        {\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}";
    let (base, handle) = spawn_mock(response).await;

    let pools = vec![
        starter_pool(QuotaPoolStatus::Absent, &[STARTER_MODEL]),
        paid_pool(QuotaPoolStatus::Active),
    ];
    let (pipeline, account_id) = pipeline_with_zai_account("e2e-unary", pools);
    let target = resolved_target(account_id, STARTER_MODEL);

    let prepared = prepare_zai_route(&pipeline, &target, &base, None)
        .await
        .expect("paid route prepared");
    assert_eq!(prepared.route.source, QuotaSource::CodingPlan);
    assert_eq!(prepared.route.url, format!("{base}/v1/messages"));

    // Dispatch the prepared route against the mocked upstream using the same
    // adapter header builder the pipeline uses, then assert on the wire.
    let adapter = openproxy_adapters::adapters::zai::ZaiAdapter::with_base_url(&base);
    let headers = adapter.build_headers(
        &prepared.route.credential,
        openproxy_types::TargetFormat::Anthropic,
        &ModelId::new(STARTER_MODEL),
    );
    let client = openproxy_adapters::upstream::UpstreamClient::new();
    let mut request = openproxy_adapters::upstream::UpstreamRequest::post_json(
        prepared.route.url.clone(),
        bytes::Bytes::from_static(b"{\"model\":\"glm-5.3-flash\"}"),
    );
    request.is_streaming = false;
    for (k, v) in &headers {
        request.headers.insert(
            http::HeaderName::from_bytes(k.as_bytes()).expect("header name"),
            http::HeaderValue::from_str(v).expect("header value"),
        );
    }
    let resp = client
        .call(
            request,
            openproxy_adapters::upstream::TimeoutProfile::Chat,
            openproxy_adapters::upstream::CancellationToken::new(),
        )
        .await
        .expect("mock call");
    assert_eq!(resp.status, http::StatusCode::OK);
    let body = resp.collect().await.expect("collect body");
    assert!(
        String::from_utf8_lossy(&body).contains("\"msg_1\""),
        "unary JSON body must round-trip from the mock"
    );

    let captured = handle.await.expect("mock join");
    assert!(
        captured.contains(&format!("Bearer {API_KEY_SENTINEL}")),
        "paid dispatch must authenticate with the Coding Plan API key; request was: {captured}"
    );
    assert!(
        !captured.contains(JWT_SENTINEL),
        "paid dispatch must not send the ZCode JWT"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn streaming_dispatch_sends_paid_key_and_reads_sse_frames() {
    let response = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n\
        event: message_start\n\
        data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\"}}\n\n\
        event: content_block_delta\n\
        data: {\"type\":\"content_block_delta\",\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\n\n\
        event: message_stop\n\
        data: {\"type\":\"message_stop\"}\n\n";
    let (base, handle) = spawn_mock(response).await;

    let pools = vec![
        starter_pool(QuotaPoolStatus::Absent, &[STARTER_MODEL]),
        paid_pool(QuotaPoolStatus::Active),
    ];
    let (pipeline, account_id) = pipeline_with_zai_account("e2e-stream", pools);
    let target = resolved_target(account_id, STARTER_MODEL);

    let prepared = prepare_zai_route(&pipeline, &target, &base, None)
        .await
        .expect("paid route prepared");

    let adapter = openproxy_adapters::adapters::zai::ZaiAdapter::with_base_url(&base);
    let headers = adapter.build_headers(
        &prepared.route.credential,
        openproxy_types::TargetFormat::Anthropic,
        &ModelId::new(STARTER_MODEL),
    );
    let client = openproxy_adapters::upstream::UpstreamClient::new();
    let mut request = openproxy_adapters::upstream::UpstreamRequest::post_json(
        prepared.route.url.clone(),
        bytes::Bytes::from_static(b"{\"model\":\"glm-5.3-flash\",\"stream\":true}"),
    );
    request.is_streaming = true;
    for (k, v) in &headers {
        request.headers.insert(
            http::HeaderName::from_bytes(k.as_bytes()).expect("header name"),
            http::HeaderValue::from_str(v).expect("header value"),
        );
    }
    let resp = client
        .call(
            request,
            openproxy_adapters::upstream::TimeoutProfile::Chat,
            openproxy_adapters::upstream::CancellationToken::new(),
        )
        .await
        .expect("mock call");
    let body = resp.collect().await.expect("collect streaming body");
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("message_start"), "stream must carry frames");
    assert!(text.contains("message_stop"), "stream must terminate");

    let captured = handle.await.expect("mock join");
    assert!(
        captured.contains(&format!("Bearer {API_KEY_SENTINEL}")),
        "streaming paid dispatch must authenticate with the Coding Plan API key"
    );
    assert!(!captured.contains(JWT_SENTINEL));
}
