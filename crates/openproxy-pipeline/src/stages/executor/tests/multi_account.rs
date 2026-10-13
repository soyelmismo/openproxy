use openproxy_types::TargetFormat;
use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
use openproxy_types::config::CooldownMode;
use openproxy_types::ids::{ComboId, ComboTargetId, ModelId, ModelRowId, ProviderId};
use openproxy_types::models::Model;
use openproxy_types::providers::RateLimitScope;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test(flavor = "multi_thread")]
async fn test_multi_account_rotation_on_400_tries_all_accounts_before_next_target() {
    use crate::context::PipelineContext;
    use crate::stage::{PipelineChain, PipelineStageEnum};
    use crate::test_utils::{MockAdapter, fresh_pool, make_request};
    use openproxy_adapters::adapters::{AdapterFormat, ProviderAdapterEnum};

    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }

    let count_target1_acc1 = Arc::new(AtomicUsize::new(0));
    let count_target1_acc2 = Arc::new(AtomicUsize::new(0));
    let count_target2 = Arc::new(AtomicUsize::new(0));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let c1 = Arc::clone(&count_target1_acc1);
    let c2 = Arc::clone(&count_target1_acc2);
    let c3 = Arc::clone(&count_target2);

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

                let response = if req_text.contains("/acc1") {
                    c1.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\nconnection: close\r\n\r\n{\"detail\":\"The 'gpt-6.1-sol' model is not supported when using Codex with a ChatGPT account.\"}"
                } else if req_text.contains("/acc2") {
                    c2.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"id\":\"resp-ok\",\"object\":\"chat.completion.chunk\",\"created\":100,\"model\":\"gpt-6.1-sol\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"rotation success\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                } else if req_text.contains("/target2") {
                    c3.fetch_add(1, Ordering::SeqCst);
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\ndata: {\"id\":\"resp-fallback\",\"object\":\"chat.completion.chunk\",\"created\":100,\"model\":\"fallback\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"fallback success\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
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
        mk_mock("codex-1", "/acc1"),
        mk_mock("codex-2", "/acc2"),
        mk_mock("fallback", "/target2"),
    ]);

    let pipeline = crate::Pipeline::new(conn_arc, cfg);

    let combo = Combo {
        id: ComboId(500),
        name: "test_multi_account".into(),
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
        ..Default::default()
    };

    let t1_acc1 = crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(10),
            combo_id: ComboId(500),
            provider_id: ProviderId::new("codex-1"),
            account_id: Some(openproxy_types::AccountId(1522)),
            model_row_id: Some(ModelRowId(777)),
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
            ..Default::default()
        },
        model: Model {
            row_id: ModelRowId(777),
            provider_id: ProviderId::new("codex-1"),
            model_id: ModelId::new("gpt-6.1-sol"),
            target_format: TargetFormat::Openai,
            ..Default::default()
        },
        api_key: "key-1522".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let t1_acc2 = crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(10),
            combo_id: ComboId(500),
            provider_id: ProviderId::new("codex-2"),
            account_id: Some(openproxy_types::AccountId(232)),
            model_row_id: Some(ModelRowId(777)),
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
            ..Default::default()
        },
        model: Model {
            row_id: ModelRowId(777),
            provider_id: ProviderId::new("codex-2"),
            model_id: ModelId::new("gpt-6.1-sol"),
            target_format: TargetFormat::Openai,
            ..Default::default()
        },
        api_key: "key-232".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let t2 = crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(20),
            combo_id: ComboId(500),
            provider_id: ProviderId::new("fallback"),
            account_id: Some(openproxy_types::AccountId(999)),
            model_row_id: Some(ModelRowId(888)),
            sub_combo_id: None,
            priority_order: 2,
            weight: 1,
            active: true,
            rate_limit_scope: RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
            ..Default::default()
        },
        model: Model {
            row_id: ModelRowId(888),
            provider_id: ProviderId::new("fallback"),
            model_id: ModelId::new("fallback"),
            target_format: TargetFormat::Openai,
            ..Default::default()
        },
        api_key: "key-999".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let targets = vec![t1_acc1, t1_acc2, t2];

    let (mut req, _rx) = make_request(ComboId(500));
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

    assert_eq!(result.status_code, 200);
    assert_eq!(
        count_target1_acc1.load(Ordering::SeqCst),
        1,
        "Target 1 Account 1 should be attempted first"
    );
    assert_eq!(
        count_target1_acc2.load(Ordering::SeqCst),
        1,
        "Target 1 Account 2 must be attempted when Account 1 fails with 400"
    );
    assert_eq!(
        count_target2.load(Ordering::SeqCst),
        0,
        "Target 2 must NOT be dispatched because Target 1 Account 2 succeeded"
    );
}
