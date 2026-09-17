use super::helpers::{is_max_request_timeout, should_skip_preventive_target};
use openproxy_types::{CancelReason, CoreError, UpstreamErrorClass};

#[test]
fn test_is_max_request_timeout() {
    let total_timeout = CoreError::UpstreamTimeout {
        phase: "total".to_string(),
        ms: 30000,
    };
    assert!(is_max_request_timeout(&total_timeout));

    let total_ms_timeout = CoreError::UpstreamTimeout {
        phase: "total_ms".to_string(),
        ms: 30000,
    };
    assert!(is_max_request_timeout(&total_ms_timeout));

    let headers_timeout = CoreError::UpstreamTimeout {
        phase: "headers".to_string(),
        ms: 5000,
    };
    assert!(!is_max_request_timeout(&headers_timeout));

    let watchdog = CoreError::Cancelled(CancelReason::WatchdogTimeout);
    assert!(is_max_request_timeout(&watchdog));

    let client_disc = CoreError::Cancelled(CancelReason::ClientDisconnected);
    assert!(!is_max_request_timeout(&client_disc));

    let gateway_timeout = CoreError::UpstreamError {
        status: 504,
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        body: "gateway timeout while waiting for total response".to_string(),
        is_proxy_rotated: false,
        class: UpstreamErrorClass::Generic,
        is_hard_skip: false,
    };
    assert!(is_max_request_timeout(&gateway_timeout));

    let internal_error = CoreError::UpstreamError {
        status: 500,
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        body: "internal server error".to_string(),
        is_proxy_rotated: false,
        class: UpstreamErrorClass::Generic,
        is_hard_skip: false,
    };
    assert!(!is_max_request_timeout(&internal_error));
}

#[test]
fn test_should_skip_preventive_target_disabled_cooldown() {
    use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
    use openproxy_types::config::CooldownMode;
    use openproxy_types::providers::RateLimitScope;
    use openproxy_types::{ComboId, ComboTargetId};

    let limiter = crate::predictive_rate_limit::PredictiveRateLimiter::new();

    let mut combo = Combo {
        id: ComboId(1),
        name: "test".into(),
        strategy: Strategy::Priority,
        race_size: 1,
        preventive_rate_limit: true,
        created_at: "now".into(),
        context_window: None,
        priority_mode: PriorityMode::Strict,
        cooldown_mode: CooldownMode::Flat,
        cooldown_base_secs: Some(60),
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
    };

    let target_a = crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(1),
            combo_id: ComboId(1),
            provider_id: openproxy_types::ProviderId("openai".into()),
            account_id: Some(openproxy_types::AccountId(1)),
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
        },
        model: openproxy_types::models::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId("openai".into()),
            model_id: "gpt-4o".into(),
            ..Default::default()
        },
        api_key: "key1".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let target_b = crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(2),
            account_id: Some(openproxy_types::AccountId(2)),
            ..target_a.target.clone()
        },
        model: target_a.model.clone(),
        api_key: "key2".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let now_ms = crate::predictive_rate_limit::PredictiveRateLimiter::now_ms();
    let key_a =
        crate::predictive_rate_limit::PredictiveRateLimiter::compute_target_key(&target_a.target);

    // Saturate target A
    limiter.report_rate_limited_key(key_a, Some(60), now_ms);

    // When cooldown is enabled on target A and B is healthy -> should skip A
    assert!(should_skip_preventive_target(
        &limiter,
        &combo,
        &target_a,
        key_a,
        std::slice::from_ref(&target_b),
        now_ms,
    ));

    // When target A explicitly disables cooldown via cooldown_mode -> DO NOT skip A
    let mut target_a_none = target_a.clone();
    target_a_none.target.cooldown_mode = Some(CooldownMode::None);
    assert!(!should_skip_preventive_target(
        &limiter,
        &combo,
        &target_a_none,
        key_a,
        std::slice::from_ref(&target_b),
        now_ms,
    ));

    // When target A explicitly disables cooldown via cooldown_base_secs = 0 -> DO NOT skip A
    let mut target_a_base0 = target_a.clone();
    target_a_base0.target.cooldown_base_secs = Some(0);
    assert!(!should_skip_preventive_target(
        &limiter,
        &combo,
        &target_a_base0,
        key_a,
        std::slice::from_ref(&target_b),
        now_ms,
    ));

    // When combo disables cooldown -> DO NOT skip A
    combo.cooldown_mode = CooldownMode::None;
    assert!(!should_skip_preventive_target(
        &limiter,
        &combo,
        &target_a,
        key_a,
        std::slice::from_ref(&target_b),
        now_ms,
    ));

    // When target B is also saturated in limiter, but target B has cooldown disabled,
    // target A (cooldown enabled) CAN skip because target B is an available healthy fallback
    combo.cooldown_mode = CooldownMode::Flat;
    let key_b =
        crate::predictive_rate_limit::PredictiveRateLimiter::compute_target_key(&target_b.target);
    limiter.report_rate_limited_key(key_b, Some(60), now_ms);

    let mut target_b_disabled = target_b;
    target_b_disabled.target.cooldown_mode = Some(CooldownMode::None);
    assert!(should_skip_preventive_target(
        &limiter,
        &combo,
        &target_a,
        key_a,
        &[target_b_disabled],
        now_ms,
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_race_target_deduplication_skips_failed_race_targets_in_sequential_phase() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use crate::stage::{PipelineChain, PipelineStageEnum};
    use crate::context::PipelineContext;
    use crate::test_utils::{fresh_pool, make_request, MockAdapter};
    use openproxy_adapters::adapters::{AdapterFormat, ProviderAdapterEnum};
    use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
    use openproxy_types::ids::{ComboId, ComboTargetId, ModelId, ModelRowId, ProviderId};
    use openproxy_types::models::Model;
    use openproxy_types::providers::RateLimitScope;
    use openproxy_types::TargetFormat;

    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }

    let count_t1 = Arc::new(AtomicUsize::new(0));
    let count_t2 = Arc::new(AtomicUsize::new(0));
    let count_t3 = Arc::new(AtomicUsize::new(0));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let c1 = Arc::clone(&count_t1);
    let c2 = Arc::clone(&count_t2);
    let c3 = Arc::clone(&count_t3);

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let c1 = Arc::clone(&c1);
            let c2 = Arc::clone(&c2);
            let c3 = Arc::clone(&c3);

            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req_text = String::from_utf8_lossy(&buf[..n]);

                let response = if req_text.contains("/t1") {
                    c1.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"t1 failed\"}}"
                } else if req_text.contains("/t2") {
                    c2.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"t2 failed\"}}"
                } else if req_text.contains("/t3") {
                    c3.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"id\":\"resp3\",\"object\":\"chat.completion.chunk\",\"created\":100,\"model\":\"m3\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"winner\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                } else {
                    "HTTP/1.1 404 Not Found\r\nconnection: close\r\n\r\n"
                };

                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            });
        }
    });

    let (_pool, conn_arc, _path) = fresh_pool();
    let master_key = Arc::new(openproxy_db::secrets::MasterKey::generate().unwrap());

    let mk_mock = |id: &str, path: &str| {
        ProviderAdapterEnum::Mock(Box::new(MockAdapter::new(
            id,
            &format!("http://127.0.0.1:{port}{path}"),
            AdapterFormat::Openai,
        )))
    };

    let mut cfg = crate::test_utils::test_config(Arc::clone(&master_key));
    cfg.adapters = Arc::new(vec![
        mk_mock("prov-1", "/t1"),
        mk_mock("prov-2", "/t2"),
        mk_mock("prov-3", "/t3"),
    ]);
    cfg.racing.max_race_size = 2;

    let pipeline = crate::Pipeline::new(conn_arc, cfg);

    let combo = Combo {
        id: ComboId(1),
        name: "test_race_dedup".into(),
        strategy: Strategy::Priority,
        race_size: 2,
        preventive_rate_limit: false,
        created_at: "now".into(),
        context_window: None,
        priority_mode: PriorityMode::Strict,
        cooldown_mode: openproxy_types::config::CooldownMode::None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
    };

    let mk_target = |tid: i64, prov: &str, prio: i32| crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(tid),
            combo_id: ComboId(1),
            provider_id: ProviderId::new(prov),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: prio,
            weight: 1,
            active: true,
            rate_limit_scope: RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
        },
        model: Model {
            row_id: ModelRowId(tid),
            provider_id: ProviderId::new(prov),
            model_id: ModelId::new("test-model"),
            target_format: TargetFormat::Openai,
            ..Default::default()
        },
        api_key: "sk-test".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let targets = vec![
        mk_target(101, "prov-1", 1),
        mk_target(102, "prov-2", 2),
        mk_target(103, "prov-3", 3),
    ];

    let (mut req, _rx) = make_request(ComboId(1));
    let (tx_direct, _rx_direct) = tokio::sync::mpsc::channel(16);
    req.stream_sink = Some(crate::race_sink::StreamSink::Direct(tx_direct));

    let mut ctx = PipelineContext::new(req, pipeline);
    ctx.combo = Some(combo);
    ctx.targets = targets;

    let chain = PipelineChain::new(vec![PipelineStageEnum::UpstreamExecutor(super::UpstreamExecutorStage)]);
    let result = chain.execute_nested(&mut ctx).await.expect("pipeline execute");

    assert_eq!(result.status_code, 200);
    assert!(result.error.is_none());

    // Crucial race deduplication assertions:
    assert_eq!(count_t1.load(Ordering::SeqCst), 1, "Target 1 must not be retried sequentially");
    assert_eq!(count_t2.load(Ordering::SeqCst), 1, "Target 2 must not be retried sequentially");
    assert_eq!(count_t3.load(Ordering::SeqCst), 1, "Target 3 must execute and succeed in sequential phase");
}

#[tokio::test]
async fn test_execute_sequential_targets_skips_failed_race_targets() {
    use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
    use openproxy_types::config::CooldownMode;
    use openproxy_types::providers::RateLimitScope;
    use openproxy_types::{ComboId, ComboTargetId, ModelRowId, ProviderId};
    use std::collections::HashSet;

    let (_pool, conn_arc, _path) = crate::test_utils::fresh_pool();
    let master_key = std::sync::Arc::new(openproxy_db::secrets::MasterKey::generate().unwrap());
    let pipeline = crate::Pipeline::new(conn_arc, crate::test_utils::test_config(master_key));

    let combo = Combo {
        id: ComboId(100),
        name: "test_race_dedup".into(),
        strategy: Strategy::Priority,
        race_size: 2,
        preventive_rate_limit: false,
        created_at: "now".into(),
        context_window: None,
        priority_mode: PriorityMode::Strict,
        cooldown_mode: CooldownMode::None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
    };

    let make_target = |id: i64, model_row_id: i64| crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(id),
            combo_id: ComboId(100),
            provider_id: ProviderId::new("openai"),
            account_id: Some(openproxy_types::AccountId(id)),
            model_row_id: Some(ModelRowId(model_row_id)),
            sub_combo_id: None,
            priority_order: id as i32,
            weight: 1,
            active: true,
            rate_limit_scope: RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
        },
        model: openproxy_types::models::Model {
            row_id: ModelRowId(model_row_id),
            provider_id: ProviderId::new("openai"),
            model_id: "gpt-4o".into(),
            ..Default::default()
        },
        api_key: format!("key-{id}"),
        api_key_label: None,
        custom_meta: None,
    };

    let target_a = make_target(1, 10);
    let target_b = make_target(2, 20);
    let target_c = make_target(3, 30);

    let to_run = vec![target_a, target_b, target_c];

    let mut failed_targets = HashSet::new();
    failed_targets.insert(ComboTargetId(1));
    failed_targets.insert(ComboTargetId(2));

    let mut failed_models = HashSet::new();
    failed_models.insert(ModelRowId(10));
    failed_models.insert(ModelRowId(20));

    let (req, _rx) = crate::test_utils::make_request(ComboId(100));
    let mut ctx = crate::context::PipelineContext::new(req, pipeline);

    let simulated_race_err = crate::PipelineResult {
        status_code: 500,
        error: Some(openproxy_types::CoreError::UpstreamError {
            status: 500,
            provider: "openai".into(),
            model: "gpt-4o".into(),
            body: "upstream lane failed".into(),
            is_proxy_rotated: false,
            class: openproxy_types::UpstreamErrorClass::Generic,
            is_hard_skip: false,
        }),
        final_response: None,
        attempts: 2,
        usage_tuple: None,
    };

    let _res = super::execute_sequential_targets(
        &mut ctx,
        &combo,
        &to_run,
        2,
        Some(simulated_race_err),
        failed_targets,
        failed_models,
    )
    .await;

    for log_entry in &ctx.combo_walk_log {
        assert!(
            !log_entry.contains("target:1 "),
            "Target A was re-dispatched! Log: {log_entry}"
        );
        assert!(
            !log_entry.contains("target:2 "),
            "Target B was re-dispatched! Log: {log_entry}"
        );
    }
}

#[tokio::test]
async fn test_adversarial_model_row_failure_deduplication() {
    use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
    use openproxy_types::config::CooldownMode;
    use openproxy_types::providers::RateLimitScope;
    use openproxy_types::{ComboId, ComboTargetId, ModelRowId, ProviderId};
    use std::collections::HashSet;

    let (_pool, conn_arc, _path) = crate::test_utils::fresh_pool();
    let master_key = std::sync::Arc::new(openproxy_db::secrets::MasterKey::generate().unwrap());
    let pipeline = crate::Pipeline::new(conn_arc, crate::test_utils::test_config(master_key));

    let combo = Combo {
        id: ComboId(200),
        name: "test_model_dedup".into(),
        strategy: Strategy::Priority,
        race_size: 1,
        preventive_rate_limit: false,
        created_at: "now".into(),
        context_window: None,
        priority_mode: PriorityMode::Strict,
        cooldown_mode: CooldownMode::None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
    };

    let make_target = |id: i64, model_row_id: i64, prov: &str| crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(id),
            combo_id: ComboId(200),
            provider_id: ProviderId::new(prov),
            account_id: Some(openproxy_types::AccountId(id)),
            model_row_id: Some(ModelRowId(model_row_id)),
            sub_combo_id: None,
            priority_order: id as i32,
            weight: 1,
            active: true,
            rate_limit_scope: RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
        },
        model: openproxy_types::models::Model {
            row_id: ModelRowId(model_row_id),
            provider_id: ProviderId::new(prov),
            model_id: "shared-model".into(),
            ..Default::default()
        },
        api_key: format!("key-{id}"),
        api_key_label: None,
        custom_meta: None,
    };

    let target_1 = make_target(1, 99, "openai");
    let target_2 = make_target(2, 99, "azure");
    let target_3 = make_target(3, 100, "anthropic");

    let to_run = vec![target_1, target_2, target_3];

    let mut failed_targets = HashSet::new();
    failed_targets.insert(ComboTargetId(1));

    let mut failed_models = HashSet::new();
    failed_models.insert(ModelRowId(99));

    let (req, _rx) = crate::test_utils::make_request(ComboId(200));
    let mut ctx = crate::context::PipelineContext::new(req, pipeline);

    let _res = super::execute_sequential_targets(
        &mut ctx,
        &combo,
        &to_run,
        1,
        None,
        failed_targets,
        failed_models,
    )
    .await;

    for log_entry in &ctx.combo_walk_log {
        assert!(
            !log_entry.contains("target:1 "),
            "Target 1 was re-dispatched! Log: {log_entry}"
        );
        assert!(
            !log_entry.contains("target:2 "),
            "Target 2 was dispatched despite failed_models! Log: {log_entry}"
        );
    }
}

#[tokio::test]
async fn test_adversarial_single_target_combo_bypasses_race() {
    use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
    use openproxy_types::config::CooldownMode;
    use openproxy_types::providers::RateLimitScope;
    use openproxy_types::{ComboId, ComboTargetId, ModelRowId, ProviderId};

    let (_pool, conn_arc, _path) = crate::test_utils::fresh_pool();
    let master_key = std::sync::Arc::new(openproxy_db::secrets::MasterKey::generate().unwrap());
    let pipeline = crate::Pipeline::new(conn_arc, crate::test_utils::test_config(master_key));

    let combo = Combo {
        id: ComboId(300),
        name: "single_target".into(),
        strategy: Strategy::Priority,
        race_size: 1,
        preventive_rate_limit: false,
        created_at: "now".into(),
        context_window: None,
        priority_mode: PriorityMode::Strict,
        cooldown_mode: CooldownMode::None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
    };

    let single_target = crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(1),
            combo_id: ComboId(300),
            provider_id: ProviderId::new("openai"),
            account_id: Some(openproxy_types::AccountId(1)),
            model_row_id: Some(ModelRowId(1)),
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
        },
        model: openproxy_types::models::Model {
            row_id: ModelRowId(1),
            provider_id: ProviderId::new("openai"),
            model_id: "gpt-4o".into(),
            ..Default::default()
        },
        api_key: "key-1".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let to_run = vec![single_target];
    let (req, _rx) = crate::test_utils::make_request(ComboId(300));
    let mut ctx = crate::context::PipelineContext::new(req, pipeline);

    let outcome = super::race::try_initial_race(&mut ctx, &combo, &to_run, 1).await;
    assert!(outcome.is_none(), "Race must be bypassed for race_size <= 1");

    let evaluated = super::evaluate_initial_race(&mut ctx, &combo, &to_run, 1).await;
    match evaluated {
        super::InitialRaceOutcome::Exhausted {
            last_result,
            failed_targets,
            failed_models,
        } => {
            assert!(last_result.is_none());
            assert!(failed_targets.is_empty());
            assert!(failed_models.is_empty());
        }
        super::InitialRaceOutcome::Success(_) => panic!("Expected Exhausted with empty sets"),
    }
}

#[tokio::test]
async fn test_adversarial_race_winner_returns_empty_failure_sets() {
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use crate::test_utils::{fresh_pool, make_request, MockAdapter};
    use openproxy_adapters::adapters::{AdapterFormat, ProviderAdapterEnum};
    use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
    use openproxy_types::ids::{ComboId, ComboTargetId, ModelId, ModelRowId, ProviderId};
    use openproxy_types::models::Model;
    use openproxy_types::providers::RateLimitScope;
    use openproxy_types::TargetFormat;

    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let response = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"id\":\"w1\",\"object\":\"chat.completion.chunk\",\"created\":100,\"model\":\"m1\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"winner\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                let _ = socket.shutdown().await;
            });
        }
    });

    let (_pool, conn_arc, _path) = fresh_pool();
    let master_key = Arc::new(openproxy_db::secrets::MasterKey::generate().unwrap());
    let mut cfg = crate::test_utils::test_config(Arc::clone(&master_key));
    cfg.adapters = Arc::new(vec![
        ProviderAdapterEnum::Mock(Box::new(MockAdapter::new(
            "prov-w1",
            &format!("http://127.0.0.1:{port}/w1"),
            AdapterFormat::Openai,
        ))),
        ProviderAdapterEnum::Mock(Box::new(MockAdapter::new(
            "prov-w2",
            &format!("http://127.0.0.1:{port}/w2"),
            AdapterFormat::Openai,
        ))),
    ]);
    cfg.racing.max_race_size = 2;
    let pipeline = crate::Pipeline::new(conn_arc, cfg);

    let combo = Combo {
        id: ComboId(400),
        name: "test_winner".into(),
        strategy: Strategy::Priority,
        race_size: 2,
        preventive_rate_limit: false,
        created_at: "now".into(),
        context_window: None,
        priority_mode: PriorityMode::Strict,
        cooldown_mode: openproxy_types::config::CooldownMode::None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
    };

    let mk_target = |tid: i64, prov: &str| crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(tid),
            combo_id: ComboId(400),
            provider_id: ProviderId::new(prov),
            account_id: None,
            model_row_id: Some(ModelRowId(tid)),
            sub_combo_id: None,
            priority_order: tid as i32,
            weight: 1,
            active: true,
            rate_limit_scope: RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
        },
        model: Model {
            row_id: ModelRowId(tid),
            provider_id: ProviderId::new(prov),
            model_id: ModelId::new("test-model"),
            target_format: TargetFormat::Openai,
            ..Default::default()
        },
        api_key: "sk-test".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let targets = vec![mk_target(401, "prov-w1"), mk_target(402, "prov-w2")];
    let (mut req, _rx) = make_request(ComboId(400));
    let (tx_direct, _rx_direct) = tokio::sync::mpsc::channel(16);
    req.stream_sink = Some(crate::race_sink::StreamSink::Direct(tx_direct));

    let outcome = crate::racing::run_race(&pipeline, req, &combo, targets, 2).await;
    assert!(outcome.result.error.is_none(), "Race should succeed");
    assert!(outcome.failed_targets.is_empty(), "Winner must produce zero failed_targets");
    assert!(outcome.failed_models.is_empty(), "Winner must produce zero failed_models");
}

