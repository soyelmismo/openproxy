use crate::config::{WorkerConfig, emit, read_line};
use anyhow::{Context, ensure};
use openproxy_core::{
    AppConfig,
    admin::{self, CreateAccountInput, CreateCustomModelInput, CreateProviderInput},
    api_keys::CreateApiKeyInput,
};
use openproxy_db::{DbPool, migrations, secrets::MasterKey};
use openproxy_server::{router::build_router, state::AppState};
use openproxy_types::ids::ProviderId;
use serde_json::json;
use std::sync::Arc;

fn seed(pool: &DbPool, master_key: &MasterKey, upstream: String) -> anyhow::Result<()> {
    let provider = ProviderId::new("mock-upstream");
    let w = pool.writer();
    admin::create_provider(
        &w,
        CreateProviderInput {
            id: provider.as_str().into(),
            name: "Benchmark Mock".into(),
            base_url: upstream,
            auth_type: "bearer".into(),
            format: "openai".into(),
            extra_headers_json: None,
            rate_limit_scope: None,
            stream_mode: None,
            prompt_cache_ttl: None,
        },
    )?;
    admin::create_account(
        &w,
        master_key,
        CreateAccountInput {
            provider_id: provider.as_str().into(),
            api_key: Some("sk-mock-bench-key".into()),
            label: Some("bench-account".into()),
            priority: Some(10),
            extra_config_json: None,
        },
    )?;
    admin::create_custom_model(
        &w,
        CreateCustomModelInput {
            provider_id: provider.as_str().into(),
            model_id: "bench-stream-model".into(),
            display_name: Some("Bench Stream Model".into()),
            target_format: "openai".into(),
            ttl_seconds: 3600,
            model_type: Some("chat".into()),
        },
    )?;
    Ok(())
}

pub async fn run(config: WorkerConfig) -> anyhow::Result<()> {
    openproxy_core::install_rustls_crypto_provider();
    let setup_config = config.clone();
    let (pool, key) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        std::fs::create_dir_all(&setup_config.directory)?;
        let synchronous = if setup_config.synchronous == "full" {
            openproxy_types::SqliteSynchronous::Full
        } else {
            openproxy_types::SqliteSynchronous::Normal
        };
        let pool = Arc::new(DbPool::open_with_options(
            &setup_config.directory.join("bench.db"),
            0,
            synchronous,
        )?);
        migrations::run(&mut pool.writer())?;
        let key = Arc::new(MasterKey::generate()?);
        seed(
            &pool,
            &key,
            setup_config.upstream.context("missing mock URL")?,
        )?;
        Ok((pool, key))
    })
    .await??;
    let mut app_config = AppConfig::default();
    app_config.server.allow_anonymous = false;
    // Keep production admission and RPM limits. Distribute load across real keys.
    let state = AppState::for_test(
        app_config,
        Arc::clone(&pool),
        key,
        Arc::new(parking_lot::RwLock::new(Arc::new(vec![]))),
    );
    state.set_recording(config.recording);
    if !config.background {
        // Keep admission, routing, PII and durable usage unchanged.
        // Recording is controlled separately by --recording.
        // Catalog refresh and public-proxy crawling are not part of this load.
        state.discovery_scheduler().cancel();
        state.supervisor().shutdown();
        state.discovery_scheduler().shutdown_and_wait().await;
        state.supervisor().shutdown_and_wait().await;
    }
    state.rebuild_adapters().await?;
    let setup_state = state.clone();
    let keys = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<String>> {
        (0..config.keys_count)
            .map(|i| {
                let (record, plaintext) = setup_state.services().api_keys.create(
                    CreateApiKeyInput {
                        label: Some(format!("bench-{i}")),
                        scopes: vec!["chat".into()],
                        allowed_models: None,
                        allowed_combos: None,
                        blacklisted_providers: None,
                        blacklisted_models: None,
                        expires_at: None,
                    },
                    "bench_setup",
                )?;
                setup_state.cache_api_key(Arc::new(record));
                Ok(plaintext)
            })
            .collect()
    })
    .await??;
    let listener = openproxy_server::listener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let requested = openproxy_server::loopback_notrack::requested();
    // Unlike production's best-effort optimization, a benchmark must fail closed:
    // silently falling back would compare two tracked runs and claim a false A/B.
    let notrack = if requested {
        Some(openproxy_server::loopback_notrack::LoopbackNotrack::install(&[port]).await?)
    } else {
        None
    };
    let mut network = json!({
        "notrack_requested": requested,
        "notrack_active": notrack.is_some(),
        "listener_port": port,
        "upstream": config.upstream,
        "table": notrack.as_ref().map(|guard| guard.table()),
    });
    std::fs::write(
        config.directory.join("loopback-notrack.json"),
        serde_json::to_vec_pretty(&network)?,
    )?;
    emit(
        &json!({"pid":std::process::id(),"url":format!("http://{}", listener.local_addr()?),"keys":keys}),
    )?;
    let app = build_router(state.clone());
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stopped.await;
        })
        .await
    });
    // Each child sees exactly one measured batch. Drain warmup before start.
    ensure!(
        tokio::task::spawn_blocking(read_line).await?? == "prepare",
        "expected prepare"
    );
    wait_for_usage(&state).await?;
    #[cfg(feature = "benchmark-sql")]
    {
        openproxy_db::sql_profile::start();
        openproxy_pipeline::profile::start();
    }
    let sql_start = std::time::Instant::now();
    // Positive backend evidence, not just absence of a fallback warning.
    // Linux truncates comm to 15 bytes. The shared ring starts during warmup.
    let io_uring_active = std::fs::read_dir("/proc/self/task")?
        .filter_map(Result::ok)
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("comm")).ok())
        .any(|name| name.trim() == "openproxy-io-ur");
    emit(&json!({"prepared":true,"io_uring_active":io_uring_active}))?;
    ensure!(
        tokio::task::spawn_blocking(read_line).await?? == "finish",
        "expected finish"
    );
    let drain_start = std::time::Instant::now();
    let _ = stop.send(());
    server.await??;
    let server_shutdown_us = drain_start.elapsed().as_micros();
    if let Some(guard) = notrack {
        let table = guard.table().to_owned();
        guard.remove().await;
        let listed = tokio::process::Command::new("nft")
            .args(["-j", "list", "tables"])
            .output()
            .await?;
        ensure!(listed.status.success(), "cannot verify notrack cleanup");
        let ruleset: serde_json::Value = serde_json::from_slice(&listed.stdout)?;
        let entries = ruleset["nftables"]
            .as_array()
            .context("missing nftables JSON array")?;
        ensure!(
            !entries.iter().any(|entry| entry["table"]["name"] == table),
            "notrack owner table survived shutdown"
        );
        network["table_removed"] = json!(true);
    }
    std::fs::write(
        config.directory.join("loopback-notrack.json"),
        serde_json::to_vec_pretty(&network)?,
    )?;
    let notrack_shutdown_us = drain_start.elapsed().as_micros() - server_shutdown_us;
    state.shutdown_usage_worker().await?;
    let usage_shutdown_us =
        drain_start.elapsed().as_micros() - server_shutdown_us - notrack_shutdown_us;
    #[cfg(feature = "benchmark-sql")]
    let batching_profile =
        serde_json::to_value(openproxy_pipeline::profile::stop()).unwrap_or(json!({}));
    #[cfg(feature = "benchmark-sql")]
    let mut sql = serde_json::to_value(openproxy_db::sql_profile::stop())?;
    #[cfg(feature = "benchmark-sql")]
    {
        let window_seconds = sql_start.elapsed().as_secs_f64();
        sql["window_seconds"] = json!(window_seconds);
        sql["statements_per_second"] =
            json!(sql["statements"].as_u64().unwrap_or(0) as f64 / window_seconds);
    }
    #[cfg(not(feature = "benchmark-sql"))]
    let _ = sql_start;
    #[cfg(not(feature = "benchmark-sql"))]
    let sql = json!({"status":"not instrumented; build with --features benchmark-sql"});
    #[cfg(not(feature = "benchmark-sql"))]
    let batching_profile = json!({"status":"not instrumented"});

    let persistence_json = json!({
        "drained": true,
        "io_uring_active": io_uring_active,
        "drain_us": drain_start.elapsed().as_micros(),
        "server_shutdown_us": server_shutdown_us,
        "notrack_shutdown_us": notrack_shutdown_us,
        "usage_shutdown_us": usage_shutdown_us,
        "network": network,
        "sql": sql,
        "batching": batching_profile
    });
    emit(&persistence_json)?;
    // Parent samples /proc after persistence but before the process exits.
    tokio::task::spawn_blocking(read_line).await??;
    Ok(())
}

async fn wait_for_usage(state: &AppState) -> anyhow::Result<()> {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let stats = state.usage_worker_stats().await?;
            ensure!(stats.failed_batches == 0, "warmup persistence failed");
            if stats.pending == 0 {
                return Ok(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await?
}
