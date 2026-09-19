use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
use openproxy_types::config::CooldownMode;
use openproxy_types::ids::{ComboId, ComboTargetId, ModelId, ModelRowId, ProviderId};
use openproxy_types::models::Model;
use openproxy_types::providers::RateLimitScope;
use openproxy_types::TargetFormat;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn test_execute_sequential_targets_skips_failed_race_targets() {
    let (_pool, conn_arc, _path) = crate::test_utils::fresh_pool();
    let master_key = Arc::new(openproxy_db::secrets::MasterKey::generate().unwrap());
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
    failed_targets.insert(ComboTargetId(1).into());
    failed_targets.insert(ComboTargetId(2).into());

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

    let _res = super::super::execute_sequential_targets(
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
            !log_entry.contains("target_id=1 "),
            "Target A was re-dispatched! Log: {log_entry}"
        );
        assert!(
            !log_entry.contains("target_id=2 "),
            "Target B was re-dispatched! Log: {log_entry}"
        );
    }
}

#[tokio::test]
async fn test_adversarial_model_row_failure_deduplication() {
    let (_pool, conn_arc, _path) = crate::test_utils::fresh_pool();
    let master_key = Arc::new(openproxy_db::secrets::MasterKey::generate().unwrap());
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
    failed_targets.insert(ComboTargetId(1).into());

    let mut failed_models = HashSet::new();
    failed_models.insert(ModelRowId(99));

    let (req, _rx) = crate::test_utils::make_request(ComboId(200));
    let mut ctx = crate::context::PipelineContext::new(req, pipeline);

    let _res = super::super::execute_sequential_targets(
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
            !log_entry.contains("target_id=1 "),
            "Target 1 was re-dispatched! Log: {log_entry}"
        );
        assert!(
            !log_entry.contains("target_id=2 "),
            "Target 2 was dispatched despite failed_models! Log: {log_entry}"
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_account_rotation_on_rate_limit_sequential() {
    use crate::context::PipelineContext;
    use crate::stage::{PipelineChain, PipelineStageEnum};
    use crate::test_utils::{MockAdapter, fresh_pool, make_request};
    use openproxy_adapters::adapters::{AdapterFormat, ProviderAdapterEnum};

    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }

    let count_acc1 = Arc::new(AtomicUsize::new(0));
    let count_acc2 = Arc::new(AtomicUsize::new(0));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let c1 = Arc::clone(&count_acc1);
    let c2 = Arc::clone(&count_acc2);

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let c1 = Arc::clone(&c1);
            let c2 = Arc::clone(&c2);

            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req_text = String::from_utf8_lossy(&buf[..n]);

                let response = if req_text.contains("/acc1") {
                    c1.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 300\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"Rate limit exceeded\",\"type\":\"rate_limit_error\"}}"
                } else if req_text.contains("/acc2") {
                    c2.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"id\":\"resp-ok\",\"object\":\"chat.completion.chunk\",\"created\":100,\"model\":\"MiniMax-M3\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"rotation success\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
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
        mk_mock("minimax-1", "/acc1"),
        mk_mock("minimax-2", "/acc2"),
    ]);

    let pipeline = crate::Pipeline::new(conn_arc, cfg);

    let combo = Combo {
        id: ComboId(-1),
        name: "__direct__".into(),
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

    let mk_target = |prov: &str, acc_id: i64| crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(0),
            combo_id: ComboId(-1),
            provider_id: ProviderId::new(prov),
            account_id: Some(openproxy_types::AccountId(acc_id)),
            model_row_id: Some(ModelRowId(1736)),
            sub_combo_id: None,
            priority_order: acc_id as i32,
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
            row_id: ModelRowId(1736),
            provider_id: ProviderId::new(prov),
            model_id: ModelId::new("MiniMax-M3"),
            target_format: TargetFormat::Openai,
            ..Default::default()
        },
        api_key: format!("key-{acc_id}"),
        api_key_label: None,
        custom_meta: None,
    };

    let targets = vec![mk_target("minimax-1", 204), mk_target("minimax-2", 205)];

    let (mut req, _rx) = make_request(ComboId(-1));
    let (tx_direct, _rx_direct) = tokio::sync::mpsc::channel(16);
    req.stream_sink = Some(crate::race_sink::StreamSink::Direct(tx_direct));

    let mut ctx = PipelineContext::new(req, pipeline);
    ctx.combo = Some(combo);
    ctx.targets = targets;

    let chain = PipelineChain::new(vec![PipelineStageEnum::UpstreamExecutor(
        super::super::UpstreamExecutorStage,
    )]);
    let result = chain
        .execute_nested(&mut ctx)
        .await
        .expect("pipeline execute");

    assert_eq!(result.status_code, 200, "Must succeed on rotated account");
    assert!(result.error.is_none(), "Must have no error");
    assert_eq!(count_acc1.load(Ordering::SeqCst), 1, "Account 1 must have been attempted");
    assert_eq!(count_acc2.load(Ordering::SeqCst), 1, "Account 2 must have succeeded");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_account_rotation_after_race_exhaustion() {
    use crate::context::PipelineContext;
    use crate::stage::{PipelineChain, PipelineStageEnum};
    use crate::test_utils::{MockAdapter, fresh_pool, make_request};
    use openproxy_adapters::adapters::{AdapterFormat, ProviderAdapterEnum};

    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }

    let count_lane1 = Arc::new(AtomicUsize::new(0));
    let count_lane2 = Arc::new(AtomicUsize::new(0));
    let count_seq3 = Arc::new(AtomicUsize::new(0));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let c1 = Arc::clone(&count_lane1);
    let c2 = Arc::clone(&count_lane2);
    let c3 = Arc::clone(&count_seq3);

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

                let response = if req_text.contains("/lane1") {
                    c1.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 300\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"lane1 rate limited\"}}"
                } else if req_text.contains("/lane2") {
                    c2.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 300\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"lane2 rate limited\"}}"
                } else if req_text.contains("/seq3") {
                    c3.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"id\":\"resp-seq3\",\"object\":\"chat.completion.chunk\",\"created\":100,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"seq3 winner\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
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
        mk_mock("p-lane1", "/lane1"),
        mk_mock("p-lane2", "/lane2"),
        mk_mock("p-seq3", "/seq3"),
    ]);
    cfg.racing.max_race_size = 2;

    let pipeline = crate::Pipeline::new(conn_arc, cfg);

    let combo = Combo {
        id: ComboId(-1),
        name: "__direct__".into(),
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

    let mk_target = |prov: &str, acc_id: i64, prio: i32| crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(0),
            combo_id: ComboId(-1),
            provider_id: ProviderId::new(prov),
            account_id: Some(openproxy_types::AccountId(acc_id)),
            model_row_id: Some(ModelRowId(500)),
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
            row_id: ModelRowId(500),
            provider_id: ProviderId::new(prov),
            model_id: ModelId::new("m"),
            target_format: TargetFormat::Openai,
            ..Default::default()
        },
        api_key: format!("key-{acc_id}"),
        api_key_label: None,
        custom_meta: None,
    };

    let targets = vec![
        mk_target("p-lane1", 1, 1),
        mk_target("p-lane2", 2, 2),
        mk_target("p-seq3", 3, 3),
    ];

    let (mut req, _rx) = make_request(ComboId(-1));
    let (tx_direct, _rx_direct) = tokio::sync::mpsc::channel(16);
    req.stream_sink = Some(crate::race_sink::StreamSink::Direct(tx_direct));

    let mut ctx = PipelineContext::new(req, pipeline);
    ctx.combo = Some(combo);
    ctx.targets = targets;

    let chain = PipelineChain::new(vec![PipelineStageEnum::UpstreamExecutor(
        super::super::UpstreamExecutorStage,
    )]);
    let result = chain
        .execute_nested(&mut ctx)
        .await
        .expect("pipeline execute");

    assert_eq!(result.status_code, 200, "Must succeed on sequential fallback");
    assert!(result.error.is_none());
    assert_eq!(count_lane1.load(Ordering::SeqCst), 1);
    assert_eq!(count_lane2.load(Ordering::SeqCst), 1);
    assert_eq!(count_seq3.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn test_model_scoped_rate_limit_skips_same_model() {
    use crate::context::PipelineContext;
    use crate::stage::{PipelineChain, PipelineStageEnum};
    use crate::test_utils::{MockAdapter, fresh_pool, make_request};
    use openproxy_adapters::adapters::{AdapterFormat, ProviderAdapterEnum};

    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }

    let count_m1 = Arc::new(AtomicUsize::new(0));
    let count_m2 = Arc::new(AtomicUsize::new(0));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let c1 = Arc::clone(&count_m1);
    let c2 = Arc::clone(&count_m2);

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                break;
            };
            let c1 = Arc::clone(&c1);
            let c2 = Arc::clone(&c2);

            tokio::spawn(async move {
                let mut buf = [0u8; 1024];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req_text = String::from_utf8_lossy(&buf[..n]);

                let response = if req_text.contains("/m1") {
                    c1.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 300\r\nconnection: close\r\n\r\n{\"error\":{\"message\":\"model rate limited\"}}"
                } else if req_text.contains("/m2") {
                    c2.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: [DONE]\n\n"
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
        mk_mock("prov-m1", "/m1"),
        mk_mock("prov-m2", "/m2"),
    ]);

    let pipeline = crate::Pipeline::new(conn_arc, cfg);

    let combo = Combo {
        id: ComboId(999),
        name: "test_model_scope".into(),
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

    let mk_target = |prov: &str, target_id: i64| crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(target_id),
            combo_id: ComboId(999),
            provider_id: ProviderId::new(prov),
            account_id: Some(openproxy_types::AccountId(target_id)),
            model_row_id: Some(ModelRowId(888)),
            sub_combo_id: None,
            priority_order: target_id as i32,
            weight: 1,
            active: true,
            rate_limit_scope: RateLimitScope::Model,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
        },
        model: Model {
            row_id: ModelRowId(888),
            provider_id: ProviderId::new(prov),
            model_id: ModelId::new("shared-model"),
            target_format: TargetFormat::Openai,
            ..Default::default()
        },
        api_key: format!("key-{target_id}"),
        api_key_label: None,
        custom_meta: None,
    };

    let targets = vec![mk_target("prov-m1", 1), mk_target("prov-m2", 2)];

    let (mut req, _rx) = make_request(ComboId(999));
    let (tx_direct, _rx_direct) = tokio::sync::mpsc::channel(16);
    req.stream_sink = Some(crate::race_sink::StreamSink::Direct(tx_direct));

    let mut ctx = PipelineContext::new(req, pipeline);
    ctx.combo = Some(combo);
    ctx.targets = targets;

    let chain = PipelineChain::new(vec![PipelineStageEnum::UpstreamExecutor(
        super::super::UpstreamExecutorStage,
    )]);
    let result = chain
        .execute_nested(&mut ctx)
        .await
        .expect("pipeline execute");

    assert_eq!(result.status_code, 429);
    assert_eq!(count_m1.load(Ordering::SeqCst), 1);
    assert_eq!(count_m2.load(Ordering::SeqCst), 0, "Target 2 must be skipped due to Model rate_limit_scope");
}
