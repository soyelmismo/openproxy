//! Tests de wiring del dispatcher. Verifican invariantes cross-submódulo
//! que solo son observables con la composición completa:
//! `dispatcher::{fail, rotation, proxy, stream, unary, horde, mod}`.
//!
//! - `handle_non_2xx_response_wires_is_hard_skip_for_validation_required`:
//!   audit fix #1 — 403 + body `VALIDATION_REQUIRED` debe propagar
//!   `is_hard_skip=true` para que el circuit breaker no penalice la cuenta.
//!
//! Este archivo se declara como `#[cfg(test)] mod tests;` desde
//! `dispatcher/mod.rs`, así que `super::*` resuelve contra el orquestador.

use super::UpstreamDispatcher;
use super::types::DispatchContext;
use openproxy_adapters::UpstreamClient;
use openproxy_db::MasterKey;
use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
use openproxy_types::providers::{AuthType, ProviderFormat, RateLimitScope};
use std::sync::atomic::AtomicU64;

/// Build a minimal in-memory-ish DB+pool pair compatible with
/// `UpstreamDispatcher::new`. Mirrors el helper que previamente vivía
/// en `upstream_dispatcher.rs` (test L1995).
pub(super) fn fresh_pool() -> (
    openproxy_db::DbPool,
    std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,
    std::path::PathBuf,
) {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = std::env::temp_dir().join(format!("openproxy-wiring-test-{pid}-{nanos}-{n}"));
    std::fs::create_dir_all(&dir).expect("mkdir tempdir");
    let path = dir.join("wiring.db");
    let pool = openproxy_db::DbPool::open(&path).expect("open pool");
    {
        let mut w = pool.writer();
        openproxy_db::migrations::run(&mut w).expect("migrations");
    }
    let extra = pool.open_connection().expect("open extra connection");
    let conn = std::sync::Arc::new(parking_lot::Mutex::new(extra));
    (pool, conn, path)
}

#[tokio::test(flavor = "multi_thread")]
async fn handle_non_2xx_response_wires_is_hard_skip_for_validation_required() {
    let (_pool, conn_arc, _path) = fresh_pool();

    // Seed a provider so the proxy-rotation branch (which is
    // a no-op when `use_proxies=0` — the SQL default) succeeds.
    let provider_id = "wired-test";
    let pid = openproxy_types::ids::ProviderId::new(provider_id);
    {
        let conn = conn_arc.lock();
        openproxy_db::providers::create(
            &conn,
            openproxy_db::providers::NewProvider {
                id: &pid,
                name: provider_id,
                base_url: "https://example.com",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: RateLimitScope::Account,
            },
        )
        .expect("seed provider");
    }

    let model = openproxy_types::models::Model {
        row_id: openproxy_types::ids::ModelRowId(1),
        provider_id: pid.clone(),
        model_id: openproxy_types::ids::ModelId::new("g-2.5"),
        display_name: None,
        discovered_at: "2024-01-01".into(),
        expires_at: None,
        timeout_overrides_json: None,
        last_test_at: None,
        context_length: None,
        max_output_tokens: None,
        capabilities_json: None,
        family: None,
        model_type: "test".into(),
        input_modalities_json: None,
        output_modalities_json: None,
        last_test_status: None,
        target_format: openproxy_types::TargetFormat::Openai,
        active: true,
        custom: false,
        ..Default::default()
    };

    let target = ComboTarget {
        id: openproxy_types::ids::ComboTargetId(1),
        combo_id: openproxy_types::ids::ComboId(1),
        provider_id: pid.clone(),
        // account_id = None so the 401/403 broadcast branch is skipped.
        account_id: None,
        model_row_id: None,
        sub_combo_id: None,
        priority_order: 1,
        weight: 1,
        active: true,
        cooldown_mode: None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        rate_limit_scope: RateLimitScope::Account,
    };

    let combo = Combo {
        id: openproxy_types::ids::ComboId(1),
        name: "wired".into(),
        strategy: Strategy::Priority,
        priority_mode: PriorityMode::Strict,
        race_size: 1,
        created_at: "2024-01-01".into(),
        context_window: None,
        cooldown_mode: openproxy_types::config::CooldownMode::None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: Some(3600),
        preventive_rate_limit: false,
    };

    // Build the dispatcher.
    let repo = std::sync::Arc::new(crate::repository::SqlitePipelineRepository::new(
        std::sync::Arc::clone(&conn_arc),
    ));
    let tracker = crate::usage_tracker::UsageTracker {
        conn: std::sync::Arc::clone(&conn_arc),
        background_tx: tokio::sync::mpsc::channel(1).0,
        record_bodies_and_headers: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        compression_stats_cell: std::sync::Arc::new(parking_lot::RwLock::new(None)),
        selection_registry: std::sync::Arc::new(openproxy_types::SelectionRegistry::new()),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        repo: std::sync::Arc::clone(&repo)
            as std::sync::Arc<dyn crate::repository::PipelineRepository>,
    };
    let cfg = crate::PipelineConfig {
        defaults: crate::timeouts::Timeouts::from_config(
            &openproxy_types::config::TimeoutsConfig::default(),
        ),
        racing: openproxy_types::config::RacingConfig::default(),
        retries: openproxy_types::config::RetriesConfig::default(),
        max_attempts: 1,
        master_key: std::sync::Arc::new(MasterKey::generate()),
        adapters: std::sync::Arc::new(Vec::new()),
        cooldown_secs: 60,
        cooldown_max_secs: 3600,
        cooldown_factor: 2,
        upstream_client: UpstreamClient::new(),
        oauth_provider_registry: None,
        compression_mode: openproxy_compression::CompressionMode::Off,
        idle_chunk_retryable: true,
        quota_protection: openproxy_types::config::QuotaProtectionConfig::default(),
        background_tx: tokio::sync::mpsc::channel(1).0,
    };
    let dispatcher = UpstreamDispatcher::new(
        std::sync::Arc::clone(&conn_arc),
        cfg,
        tracker,
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    );

    let started = std::time::Instant::now();
    let dctx = DispatchContext {
        attempt: 1,
        race_size: 1,
        started,
        model: &model,
        proxy_url: None,
        proxy_status: None,
    };

    // Build a PipelineRequest.
    let (_tx, rx) = tokio::sync::watch::channel::<Option<openproxy_types::CancelReason>>(None);
    let req = crate::PipelineRequest {
        request_id: openproxy_types::ids::RequestId::new(),
        trace_id: openproxy_types::ids::TraceId::new(),
        combo_id: openproxy_types::ids::ComboId(1),
        openai_request: std::sync::Arc::new(openproxy_types::OpenAIRequest {
            model: "g-2.5".into(),
            messages: vec![openproxy_types::OpenAIMessage {
                role: "user".into(),
                content: Some(serde_json::Value::String("hi".into())),
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
        client_disconnected: rx,
        stream_sink: None,
        api_key_id: None,
        combo_override: None,
        targets_override: None,
        request_headers: std::collections::BTreeMap::new(),
        request_body_json: None,
        race_cancelled: false,
        race_cancel: None,
        endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        compressed_messages: std::sync::Arc::new(std::sync::OnceLock::new()),
        proxy_override: None,
    };

    // 403 + VALIDATION_REQUIRED body → must produce is_hard_skip=true.
    let result = dispatcher
        .handle_non_2xx_response(
            403,
            None,
            r#"{"error":{"code":"VALIDATION_REQUIRED"}}"#.to_string(),
            req,
            &combo,
            &target,
            &model,
            &dctx,
            42,
            Some(10),
        )
        .await;

    let err = result
        .error
        .expect("non-2xx response must produce an error");
    assert!(
        err.is_hard_skip(),
        "audit #1: 403 VALIDATION_REQUIRED must yield is_hard_skip=true, got error={err:?}"
    );
    assert_eq!(
        err.upstream_error_class(),
        Some(openproxy_types::UpstreamErrorClass::ValidationRequired)
    );
}
