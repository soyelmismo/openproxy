//! What actually leaves the proxy when a request routed through a combo fails.
//!
//! The rule under test is the HTTP status, not the internal error variant: any
//! `4xx`/`5xx` a combo would answer is reduced to a class the caller can act
//! on, and the only family that keeps its message is the caller's own
//! quota/plan rejection (produced by this proxy, never by a target). The real
//! cause stays in `usage` and in the logs.

use openproxy_types::error::CoreError;
use openproxy_types::ids::ComboId;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Upstream detail that must never reach the client.
const SECRET_BODY: &str =
    r#"{"error":{"message":"account=acme-key-7 quota_window_exhausted provider=anthropic"}}"#;

struct FailingUpstream {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl FailingUpstream {
    async fn start(status: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let _ = socket.read(&mut buf).await;
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\n\
                         content-length: {}\r\nconnection: close\r\n\r\n{SECRET_BODY}",
                        SECRET_BODY.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.flush().await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { url, task }
    }
}

impl Drop for FailingUpstream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct Harness {
    pipeline: crate::Pipeline,
    combo_id: ComboId,
    /// Kept alive so `background_tx` never sees a closed channel.
    _jobs: tokio::sync::mpsc::Receiver<crate::worker::BackgroundJob>,
}

impl Harness {
    fn start(upstream: &FailingUpstream) -> Self {
        let (pool, seed_conn, _path) = crate::test_utils::fresh_pool();
        let master_key =
            Arc::new(openproxy_db::secrets::MasterKey::generate().expect("master key"));
        let combo_id = {
            let conn = seed_conn.lock();
            let (combo_id, _account_id) = crate::test_utils::seed_solo_combo_at_url(
                &conn,
                "mock-openai",
                &upstream.url,
                master_key.as_ref(),
            );
            combo_id
        };
        let mut config = crate::test_utils::test_config_with_mock(master_key, &upstream.url);
        config.max_attempts = 1;
        config.retries.max_attempts = 1;
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        config.background_tx = tx;
        let pipeline = crate::test_utils::test_pipeline_with_pool(pool, config);
        Self {
            pipeline,
            combo_id,
            _jobs: rx,
        }
    }
}

fn result_with(err: CoreError, attempts: u8) -> crate::PipelineResult {
    crate::PipelineResult {
        status_code: err.http_status(),
        error: Some(err),
        final_response: None,
        attempts,
        usage_tuple: None,
    }
}

fn upstream_error(status: u16) -> CoreError {
    CoreError::UpstreamError {
        status,
        provider: "anthropic".into(),
        model: "claude".into(),
        body: SECRET_BODY.into(),
        is_proxy_rotated: false,
        class: openproxy_types::UpstreamErrorClass::Generic,
        is_hard_skip: false,
    }
}

#[tokio::test]
async fn combo_caller_gets_a_generic_error_and_the_real_status_is_kept() {
    let upstream = FailingUpstream::start("500 Internal Server Error").await;
    let harness = Harness::start(&upstream);
    let (req, _cancel) = crate::test_utils::make_request(harness.combo_id);

    let result = harness.pipeline.run(req).await;

    let err = result
        .error
        .expect("a failing target must produce an error");
    let text = err.to_string();
    assert!(
        !text.contains("acme-key-7"),
        "leaked the upstream account: {text}"
    );
    assert!(!text.contains("anthropic"), "leaked the provider: {text}");
    assert!(
        !text.contains("quota_window_exhausted"),
        "leaked upstream state: {text}"
    );
    assert!(
        matches!(err, CoreError::MaskedUpstream { .. }),
        "unexpected error: {text}"
    );
    assert_eq!(err.code(), "upstream_error");

    // The response class follows the real status, and the journal keeps that
    // real status: masking happens after the attempt was accounted for.
    let real = result.status_code;
    assert!(real >= 400, "the fixture must fail, got {real}");
    let expected = match real {
        400 | 413 | 422 => real,
        404 => 404,
        408 | 504 | 529 => 504,
        429 => 429,
        _ => 502,
    };
    assert_eq!(err.http_status(), expected, "real status {real}");
}

#[tokio::test]
async fn the_class_comes_from_the_status_not_the_variant() {
    // Same variant, different statuses: the caller sees what the status allows.
    for (upstream_status, expected) in [(400u16, 400u16), (404, 404), (429, 429), (503, 502)] {
        let mut result = result_with(upstream_error(upstream_status), 1);
        crate::Pipeline::mask_target_error(true, ComboId(7), &mut result);
        let err = result.error.expect("error");
        assert_eq!(err.http_status(), expected, "upstream {upstream_status}");
        assert_eq!(err.code(), "upstream_error");
    }

    // Different variants, any status: all of them end up generic.
    for err in [
        CoreError::Validation(
            "no active AutoClaw, ZCode Starter, or Coding Plan quota authorizes model GLM-5".into(),
        ),
        CoreError::ComboNotFound(12),
        CoreError::NoHealthyTargets(12),
        CoreError::UpstreamConnection("dial tcp 10.0.0.4:443".into()),
        CoreError::Internal("sqlite: no such table usage_journal".into()),
    ] {
        let before = err.to_string();
        let mut result = result_with(err, 1);
        crate::Pipeline::mask_target_error(true, ComboId(7), &mut result);
        let masked = result.error.expect("error");
        assert_eq!(masked.code(), "upstream_error", "{before}");
        assert_ne!(masked.to_string(), before, "{before}");
    }
}

#[tokio::test]
async fn a_direct_model_caller_still_sees_the_raw_failure() {
    // `combo_id` -1 is the synthetic combo used when a caller names a model
    // directly: that caller is the operator debugging a provider.
    let mut result = result_with(upstream_error(500), 1);
    crate::Pipeline::mask_target_error(true, ComboId(-1), &mut result);
    let text = result.error.expect("error").to_string();
    assert!(
        text.contains("anthropic"),
        "direct callers keep the provider: {text}"
    );
}

#[tokio::test]
async fn masking_can_be_disabled() {
    let mut result = result_with(
        CoreError::UpstreamConnection("dial tcp 10.0.0.4:443".into()),
        1,
    );
    crate::Pipeline::mask_target_error(false, ComboId(7), &mut result);
    let text = result.error.expect("error").to_string();
    assert!(
        text.contains("10.0.0.4"),
        "disabled masking keeps the detail: {text}"
    );
}

#[tokio::test]
async fn caller_quota_rejections_are_the_only_exception() {
    let quota = CoreError::QuotaExceeded {
        window: "daily".into(),
        spent_usd: 5.0,
        cap_usd: 5.0,
        resets_at: "2026-11-15T00:00:00Z".into(),
    };
    let mut result = result_with(quota, 1);
    crate::Pipeline::mask_target_error(true, ComboId(7), &mut result);
    let err = result.error.expect("error");
    assert_eq!(err.code(), "quota_exceeded");
    assert_eq!(err.http_status(), 429);

    let credits = CoreError::InsufficientCredits { balance_usd: 0.0 };
    let mut result = result_with(credits, 1);
    crate::Pipeline::mask_target_error(true, ComboId(7), &mut result);
    let err = result.error.expect("error");
    assert_eq!(err.code(), "insufficient_credits");
    assert_eq!(err.http_status(), 402);
}

#[tokio::test]
async fn a_caller_that_disconnected_is_not_rewritten() {
    // 499: there is no response to rewrite.
    let mut result = result_with(
        CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected),
        1,
    );
    crate::Pipeline::mask_target_error(true, ComboId(7), &mut result);
    assert!(matches!(
        result.error.expect("error"),
        CoreError::Cancelled(_)
    ));
}
