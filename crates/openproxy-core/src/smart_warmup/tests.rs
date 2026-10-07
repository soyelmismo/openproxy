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
        weekly_reset_at: Some(future_str.clone()),
        session_used: Some(1),
        session_limit: Some(1000),
        ..AccountQuota::empty()
    };
    assert!(!is_model_quota_ready_for_warmup(
        &gemini_ticking,
        "gemini-2.5-pro",
        now
    ));

    // 6. Gemini with partial weekly usage (e.g. 500/1000) but fresh session window IS ready
    let gemini_partial_weekly = AccountQuota {
        weekly_used: Some(500),
        weekly_limit: Some(1000),
        weekly_reset_at: Some(future_str.clone()),
        session_used: Some(0),
        session_limit: Some(1000),
        ..AccountQuota::empty()
    };
    assert!(is_model_quota_ready_for_warmup(
        &gemini_partial_weekly,
        "gemini-2.5-pro",
        now
    ));

    // 7. Gemini with 100% exhausted weekly quota is NOT ready
    let gemini_weekly_exhausted = AccountQuota {
        weekly_used: Some(1000),
        weekly_limit: Some(1000),
        weekly_reset_at: Some(future_str.clone()),
        session_used: Some(0),
        session_limit: Some(1000),
        ..AccountQuota::empty()
    };
    assert!(!is_model_quota_ready_for_warmup(
        &gemini_weekly_exhausted,
        "gemini-2.5-pro",
        now
    ));

    // 8. Claude with partial weekly usage but fresh 5h window IS ready
    let claude_partial_weekly = AccountQuota {
        model_details: Some(
            vec![
                detail("Claude (Weekly)", 500, Some(future_str.clone()), 0.5),
                detail("Claude (5h)", 0, None, 1.0),
            ]
            .into_boxed_slice(),
        ),
        ..AccountQuota::empty()
    };
    assert!(is_model_quota_ready_for_warmup(
        &claude_partial_weekly,
        "claude-sonnet-4-6",
        now
    ));

    // 9. Claude with 100% exhausted weekly bucket is NOT ready
    let claude_weekly_exhausted = AccountQuota {
        model_details: Some(
            vec![
                detail("Claude (Weekly)", 1000, Some(future_str), 0.0),
                detail("Claude (5h)", 0, None, 1.0),
            ]
            .into_boxed_slice(),
        ),
        ..AccountQuota::empty()
    };
    assert!(!is_model_quota_ready_for_warmup(
        &claude_weekly_exhausted,
        "claude-sonnet-4-6",
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

#[test]
fn test_default_strategies_includes_all_providers() {
    let strats = default_strategies();
    assert_eq!(strats.len(), 3);
    let ids: Vec<&str> = strats.iter().map(|s| s.provider_id()).collect();
    assert!(ids.contains(&"antigravity"));
    assert!(ids.contains(&"codex"));
    assert!(ids.contains(&"claude-code"));
}

#[test]
fn test_codex_warmup_quota_ready() {
    let strat = CodexWarmupStrategy::new();
    assert_eq!(strat.provider_id(), "codex");

    let now = 1700000000;
    let future_str = chrono::DateTime::from_timestamp(now + 3600, 0)
        .unwrap()
        .to_rfc3339();

    // 1. Ready when session_used is 0
    let ready_quota = AccountQuota {
        session_used: Some(0),
        session_reset_at: Some(future_str.clone()),
        weekly_used: Some(10),
        weekly_reset_at: Some(future_str.clone()),
        ..AccountQuota::empty()
    };
    assert!(strat.is_quota_ready(&ready_quota, "gpt-5-codex", now));

    // 2. Not ready when session_used > 0 and future reset
    let ticking_quota = AccountQuota {
        session_used: Some(25),
        session_reset_at: Some(future_str.clone()),
        ..AccountQuota::empty()
    };
    assert!(!strat.is_quota_ready(&ticking_quota, "gpt-5-codex", now));

    // 3. Not ready when weekly limit is 100% used
    let exhausted_quota = AccountQuota {
        session_used: Some(0),
        weekly_used: Some(100),
        weekly_reset_at: Some(future_str),
        ..AccountQuota::empty()
    };
    assert!(!strat.is_quota_ready(&exhausted_quota, "gpt-5-codex", now));
}

#[test]
fn test_claude_code_warmup_quota_ready() {
    let strat = ClaudeCodeWarmupStrategy::new();
    assert_eq!(strat.provider_id(), "claude-code");

    let now = 1700000000;
    let future_str = chrono::DateTime::from_timestamp(now + 3600, 0)
        .unwrap()
        .to_rfc3339();

    // 1. Ready when 5h session_used is 0
    let ready_quota = AccountQuota {
        session_used: Some(0),
        session_reset_at: Some(future_str.clone()),
        weekly_used: Some(40),
        weekly_reset_at: Some(future_str.clone()),
        ..AccountQuota::empty()
    };
    assert!(strat.is_quota_ready(&ready_quota, "claude-3-7-sonnet-20250219", now));

    // 2. Not ready when session_used > 0 and ticking
    let ticking_quota = AccountQuota {
        session_used: Some(15),
        session_reset_at: Some(future_str.clone()),
        ..AccountQuota::empty()
    };
    assert!(!strat.is_quota_ready(&ticking_quota, "claude-3-7-sonnet-20250219", now));

    // 3. Not ready when weekly limit is 100% used (even if session_used is 0!)
    let weekly_exhausted = AccountQuota {
        session_used: Some(0),
        weekly_used: Some(100),
        weekly_reset_at: Some(future_str.clone()),
        ..AccountQuota::empty()
    };
    assert!(!strat.is_quota_ready(&weekly_exhausted, "claude-3-7-sonnet-20250219", now));

    // 4. Not ready when a weekly model detail is 100% exhausted
    let detail_exhausted = AccountQuota {
        session_used: Some(0),
        weekly_used: Some(50),
        model_details: Some(
            vec![ModelQuotaDetail {
                model_id: "Claude Sonnet (Weekly)".to_string(),
                session_used: 100,
                session_limit: 100,
                session_reset_at: Some(future_str),
                remaining_fraction: 0.0,
            }]
            .into_boxed_slice(),
        ),
        ..AccountQuota::empty()
    };
    assert!(!strat.is_quota_ready(&detail_exhausted, "claude-3-7-sonnet-20250219", now));
}

#[test]
fn test_antigravity_weekly_exhaustion_blocks_warmup() {
    let strat = AntigravityWarmupStrategy::new();
    let now = 1700000000;
    let future_str = chrono::DateTime::from_timestamp(now + 3600, 0)
        .unwrap()
        .to_rfc3339();

    // Even if session_used is 0, 100% weekly usage (1000/1000) blocks warmup
    let quota = AccountQuota {
        session_used: Some(0),
        session_limit: Some(1000),
        weekly_used: Some(1000),
        weekly_limit: Some(1000),
        weekly_reset_at: Some(future_str.clone()),
        ..AccountQuota::empty()
    };
    assert!(!strat.is_quota_ready(&quota, "claude-sonnet-4-6", now));

    // Also blocks when weekly_limit is None and weekly_used >= 1000
    let quota_no_limit = AccountQuota {
        session_used: Some(0),
        session_limit: Some(1000),
        weekly_used: Some(1000),
        weekly_limit: None,
        weekly_reset_at: Some(future_str),
        ..AccountQuota::empty()
    };
    assert!(!strat.is_quota_ready(&quota_no_limit, "claude-sonnet-4-6", now));
}


