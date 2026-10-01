use super::*;
use openproxy_db::DbPool;
use openproxy_types::{AccountQuota, ModelQuotaDetail};

fn fresh_pool() -> Arc<DbPool> {
    Arc::new(DbPool::test_pool_with_prefix("openproxy-smart-warmup-test").expect("open pool"))
}

#[test]
fn warmup_request_shapes_for_claude_and_gemini() {
    let claude = build_warmup_request("claude-sonnet-4-6");
    assert_eq!(claude.model, "claude-sonnet-4-6");
    assert_eq!(
        claude.messages[0].content.as_ref().and_then(|v| v.as_str()),
        Some("Say hi")
    );
    assert_eq!(claude.max_tokens, None);
    assert_eq!(claude.temperature, Some(0.0));

    let gemini = build_warmup_request("gemini-pro-agent");
    assert_eq!(gemini.model, "gemini-pro-agent");
    let content = gemini.messages[0]
        .content
        .as_ref()
        .and_then(|v| v.as_str())
        .unwrap();
    assert!(content.contains("Fibonacci"));
    assert_eq!(gemini.max_tokens, Some(1024));
    assert_eq!(gemini.temperature, Some(0.2));
}

#[test]
fn smart_warmup_extract_reads_snake_case_canonical() {
    use serde_json::json;
    let v = json!({"project_id":"canonical"});
    assert_eq!(
        openproxy_pipeline::credentials::antigravity_project_from_value(&v),
        Some("canonical".to_string())
    );
}

#[test]
fn test_is_model_quota_ready_for_warmup() {
    let now = 1700000000;
    let future_str = chrono::DateTime::from_timestamp(now + 3600, 0)
        .unwrap()
        .to_rfc3339();
    let past_str = chrono::DateTime::from_timestamp(now - 3600, 0)
        .unwrap()
        .to_rfc3339();

    let detail = |id: &str, used, reset, frac| ModelQuotaDetail {
        model_id: id.to_string(),
        session_used: used,
        session_limit: 1000,
        session_reset_at: reset,
        remaining_fraction: frac,
    };

    // 1. Claude model with future reset and usage is NOT ready
    let claude_ticking = AccountQuota {
        model_details: Some(
            vec![detail(
                "claude-sonnet-4-6",
                1,
                Some(future_str.clone()),
                0.999,
            )]
            .into_boxed_slice(),
        ),
        ..AccountQuota::empty()
    };
    assert!(!is_model_quota_ready_for_warmup(
        &claude_ticking,
        "claude-sonnet-4-6",
        now
    ));

    // 2. Claude model with expired reset and 100% capacity IS ready
    let claude_ready = AccountQuota {
        model_details: Some(
            vec![detail("claude-sonnet-4-6", 0, Some(past_str), 1.0)].into_boxed_slice(),
        ),
        ..AccountQuota::empty()
    };
    assert!(is_model_quota_ready_for_warmup(
        &claude_ready,
        "claude-sonnet-4-6",
        now
    ));

    // 3. Model matching Claude summary bucket "Claude (5h)"
    let claude_summary_ticking = AccountQuota {
        model_details: Some(
            vec![detail("Claude (5h)", 1, Some(future_str.clone()), 0.999)].into_boxed_slice(),
        ),
        ..AccountQuota::empty()
    };
    assert!(!is_model_quota_ready_for_warmup(
        &claude_summary_ticking,
        "claude-opus-4-6-thinking",
        now
    ));

    // 4. Gemini account with 0 weekly used IS ready (kickstarts weekly countdown)
    let gemini_ready = AccountQuota {
        weekly_used: Some(0),
        weekly_limit: Some(1000),
        weekly_reset_at: Some(future_str.clone()),
        session_used: Some(0),
        session_limit: Some(1000),
        session_reset_at: Some(future_str.clone()),
        ..AccountQuota::empty()
    };
    assert!(is_model_quota_ready_for_warmup(
        &gemini_ready,
        "gemini-2.5-pro",
        now
    ));

    // 5. Gemini account with weekly usage already ticking is NOT ready
    let gemini_ticking = AccountQuota {
        weekly_used: Some(1),
        weekly_limit: Some(1000),
        weekly_reset_at: Some(future_str),
        session_used: Some(1),
        session_limit: Some(1000),
        ..AccountQuota::empty()
    };
    assert!(!is_model_quota_ready_for_warmup(
        &gemini_ticking,
        "gemini-2.5-pro",
        now
    ));
}

#[tokio::test]
async fn smart_warmup_disabled_config_is_noop() {
    let pool = fresh_pool();
    let mut config = AppConfig::default();
    config.smart_warmup.enabled = false;

    let upstream = UpstreamClient::new();
    let master_key = Arc::new(MasterKey::generate().expect("generate key"));

    start_smart_warmup_scheduler(
        Arc::clone(&pool),
        config.clone(),
        Arc::clone(&upstream),
        Arc::clone(&master_key),
    );

    let res = start_smart_warmup_scheduler_with_cancel(
        Arc::clone(&pool),
        config.clone(),
        Arc::clone(&upstream),
        Arc::clone(&master_key),
        None,
    );
    assert!(res.is_none());

    let cancel = CancellationToken::new();
    run_smart_warmup_scheduler(pool, config, upstream, master_key, cancel).await;
}

#[tokio::test]
async fn smart_warmup_scheduler_cancelled_exits_promptly() {
    let pool = fresh_pool();
    let mut config = AppConfig::default();
    config.smart_warmup.enabled = true;
    config.smart_warmup.interval_secs = 3600;

    let upstream = UpstreamClient::new();
    let master_key = Arc::new(MasterKey::generate().expect("generate key"));

    let cancel = CancellationToken::new();
    cancel.cancel(); // Pre-cancelled token

    // Must return immediately without executing network calls
    run_smart_warmup_scheduler(pool, config, upstream, master_key, cancel).await;
}

#[tokio::test]
async fn smart_warmup_cycle_with_cancel_breaks_cleanly() {
    let pool = fresh_pool();
    let mut config = AppConfig::default();
    config.smart_warmup.enabled = true;

    let upstream = UpstreamClient::new();
    let master_key = Arc::new(MasterKey::generate().expect("generate key"));

    let cancel = CancellationToken::new();
    cancel.cancel();

    run_warmup_cycle_with_cancel(&pool, &config, &upstream, &master_key, Some(&cancel)).await;
}
