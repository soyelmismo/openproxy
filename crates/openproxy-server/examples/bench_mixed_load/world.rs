//! E2E mixed-load harness (part 2): isolated world — mock paced-SSE upstream,
//! mock JSON-RPC chain, DB seeding and the REAL router served on loopback TCP.
//!
//! Mirrors `bench_token_stress/proxy.rs`: everything background that is not
//! part of the measured load (discovery scheduler, supervisor) is cancelled
//! BEFORE the run so the benchmark sees admission + routing + persistence
//! only, and never a crawler racing the client.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::config::Config;
use anyhow::Context as _;
use axum::{
    Router,
    body::Body,
    extract::State as AxumState,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use openproxy_core::{
    AppConfig,
    admin::{self, CreateAccountInput, CreateCustomModelInput, CreateProviderInput},
    api_keys::CreateApiKeyInput,
};
use openproxy_db::{DbPool, migrations, secrets::MasterKey};
use openproxy_server::{router::build_router, state::AppState};
use openproxy_types::ids::ProviderId;
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};

// ── mock upstream: OpenAI-compatible, paced SSE ────────────────────────────

#[derive(Clone)]
struct PacedUpstream {
    /// Per-request stream duration, uniform inside [min, max].
    pace_min_ms: u64,
    pace_max_ms: u64,
    chunks: u64,
    /// Optional artificial delay BEFORE first byte (server think-time).
    pre_delay_ms: i64,
}

impl PacedUpstream {
    fn duration_ms(&self) -> u64 {
        if self.pace_max_ms <= self.pace_min_ms {
            return self.pace_min_ms;
        }
        // Cheap per-request jitter without pulling rand: nanos % range.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ (std::process::id() as u64) << 17)
            .unwrap_or(self.pace_min_ms);
        self.pace_min_ms + nanos % (self.pace_max_ms - self.pace_min_ms)
    }
}

async fn paced_chat(AxumState(up): AxumState<PacedUpstream>, body: axum::body::Bytes) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let model = parsed
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("bench-stream-model")
        .to_string();
    let streaming = parsed
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let pre = up.pre_delay_ms;
    if pre > 0 {
        tokio::time::sleep(Duration::from_millis(pre as u64)).await;
    }
    if !streaming {
        let resp = json!({
            "id": "chatcmpl-bench", "object": "chat.completion",
            "created": 1_700_000_000, "model": model,
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}
        });
        return (axum::http::StatusCode::OK, axum::Json(resp)).into_response();
    }
    // Paced SSE: `chunks` events, each preceded by an equal slice of the
    // total duration, so the stream's wall time lands inside [min, max].
    let total_ms = up.duration_ms();
    let chunk_ms = (total_ms / up.chunks.max(1)).max(1);
    let model_json = json!(model).to_string();
    let stream = futures::stream::unfold(0u64, move |i| {
        let model_json = model_json.clone();
        async move {
            if i > up.chunks {
                return None;
            }
            if i == up.chunks {
                let done = format!("data: [DONE]\n\n");
                return Some((
                    Ok::<_, std::io::Error>(axum::body::Bytes::from(done)),
                    up.chunks + 1,
                ));
            }
            tokio::time::sleep(Duration::from_millis(chunk_ms)).await;
            let ev = if i == 0 {
                format!(
                    "data: {{\"id\":\"chatcmpl-bench\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":{model_json},\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\"content\":\"tok\"}},\"finish_reason\":null}}]}}\n\n"
                )
            } else if i + 1 == up.chunks {
                format!(
                    "data: {{\"id\":\"chatcmpl-bench\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":{model_json},\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}}}\n\n"
                )
            } else {
                format!(
                    "data: {{\"id\":\"chatcmpl-bench\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":{model_json},\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\" tok\"}},\"finish_reason\":null}}]}}\n\n"
                )
            };
            Some((Ok::<_, std::io::Error>(axum::body::Bytes::from(ev)), i + 1))
        }
    });
    (
        axum::http::StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        Body::from_stream(stream),
    )
        .into_response()
}

async fn paced_models(AxumState(up): AxumState<PacedUpstream>) -> Response {
    let mut data = Vec::new();
    for i in 0..up.chunks.min(8) {
        data.push(
            json!({"id": format!("bench-model-{i}"), "object": "model", "owned_by": "bench"}),
        );
    }
    (
        axum::http::StatusCode::OK,
        axum::Json(json!({"object": "list", "data": data})),
    )
        .into_response()
}

/// Spawns the mock upstream on its own thread and returns its base URL.
pub fn spawn_paced_upstream(cfg: &Config) -> String {
    let up = PacedUpstream {
        pace_min_ms: cfg.pace_min.as_millis() as u64,
        pace_max_ms: cfg.pace_max.as_millis() as u64,
        chunks: 8,
        pre_delay_ms: 0,
    };
    let app = Router::new()
        .route("/v1/chat/completions", post(paced_chat))
        .route("/chat/completions", post(paced_chat))
        .route("/v1/models", get(paced_models))
        .with_state(up);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock upstream");
    let addr = listener.local_addr().expect("mock upstream addr");
    listener
        .set_nonblocking(true)
        .expect("nonblocking upstream");
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("mock upstream runtime");
        rt.block_on(async move {
            axum::serve(
                tokio::net::TcpListener::from_std(listener).expect("upstream listener"),
                app,
            )
            .await
            .expect("serve mock upstream");
        });
    });
    format!("http://{addr}/v1")
}

// ── mock chain: Base JSON-RPC with controllable logs ───────────────────────

#[derive(Default)]
pub struct MockChain {
    /// Next block number the mock head reports; every order payment is placed
    /// at `head` when registered, with the mock clock stamping `now` there.
    head: AtomicI64,
    logs: Mutex<Vec<Value>>,
}

impl MockChain {
    /// Registers an exact-amount USDC transfer into the collector at the head
    /// block and advances the head so later scans find it inside the window.
    pub fn push_payment(&self, amount_units: i64, tx: &str, log_index: i64) {
        let mut logs = self.logs.lock();
        let block = self.head.load(Ordering::SeqCst) + 1;
        let seed = tx.trim_start_matches("0x").replace('-', "");
        let padded = format!("{seed:0>64}").to_lowercase();
        let log = json!({
            "address": "0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913",
            "topics": [
                "0xddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef",
                "0x0000000000000000000000001111111111111111111111111111111111111111",
                "0x00000000000000000000000012c7ab78f21ba0253a21d34a0151d44ddb824950"
            ],
            "data": format!("0x{amount_units:064x}"),
            "bench_timestamp": unix_now(),
            "logIndex": format!("0x{log_index:x}"),
            "blockNumber": format!("0x{block:x}"),
            "transactionHash": format!("0x{padded}")
        });
        logs.push(log);
        self.head.store(block, Ordering::SeqCst);
    }
}

async fn chain_entry(
    AxumState(chain): AxumState<Arc<MockChain>>,
    body: axum::body::Bytes,
) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let method = parsed.get("method").and_then(Value::as_str).unwrap_or("");
    let params = parsed.get("params").cloned().unwrap_or(Value::Null);
    let hex_qty = |v: i64| format!("0x{v:x}");
    let head = chain.head.load(Ordering::SeqCst);
    let payload = match method {
        "eth_chainId" => json!({"jsonrpc": "2.0", "id": 1, "result": hex_qty(8453)}),
        "eth_getBlockByNumber" => {
            let tag = params
                .as_array()
                .and_then(|p| p.first())
                .and_then(Value::as_str)
                .unwrap_or("safe")
                .to_string();
            let number = match tag.as_str() {
                "safe" | "finalized" => Some(head),
                hex => i64::from_str_radix(hex.trim_start_matches("0x"), 16).ok(),
            };
            // Linear clock anchored at the wall clock: block N is
            // (head - N) * 2s old, exactly what the watcher's window rules
            // and the order-anchor bootstrap expect.
            let result = number.map(|n| {
                json!({
                    "number": hex_qty(n),
                    "timestamp": hex_qty(
                        chain.logs.lock().iter().find(|l| l["blockNumber"] == hex_qty(n))
                            .and_then(|l| l["bench_timestamp"].as_i64()).unwrap_or_else(unix_now)
                    )
                })
            });
            json!({"jsonrpc": "2.0", "id": 1, "result": result})
        }
        "eth_getLogs" => {
            let filter = params
                .as_array()
                .and_then(|p| p.first())
                .cloned()
                .unwrap_or(Value::Null);
            let parse = |k: &str, default: i64| {
                filter
                    .get(k)
                    .and_then(Value::as_str)
                    .and_then(|h| i64::from_str_radix(h.trim_start_matches("0x"), 16).ok())
                    .unwrap_or(default)
            };
            let from = parse("fromBlock", 0);
            let to = parse("toBlock", i64::MAX);
            let logs: Vec<Value> = chain
                .logs
                .lock()
                .iter()
                .filter(|l| {
                    let block = l
                        .get("blockNumber")
                        .and_then(Value::as_str)
                        .and_then(|h| i64::from_str_radix(h.trim_start_matches("0x"), 16).ok())
                        .unwrap_or(-1);
                    block >= from && block <= to
                })
                .cloned()
                .collect();
            json!({"jsonrpc": "2.0", "id": 1, "result": logs})
        }
        _ => json!({"jsonrpc": "2.0", "id": 1, "error": {"code": -32601, "message": "unknown"}}),
    };
    axum::Json(payload).into_response()
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Spawns the mock JSON-RPC node on its own thread; returns (addr, chain).
pub fn spawn_mock_chain() -> (String, Arc<MockChain>) {
    let chain = Arc::new(MockChain::default());
    // Head starts ahead of 0 so the watcher's first tick has a bounded lookback
    // that still reaches every later payment block.
    chain.head.store(1_000, Ordering::SeqCst);
    let app = Router::new()
        .route("/", post(chain_entry))
        .with_state(Arc::clone(&chain));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock chain");
    let addr = listener.local_addr().expect("mock chain addr");
    listener.set_nonblocking(true).expect("nonblocking chain");
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("mock chain runtime");
        rt.block_on(async move {
            axum::serve(
                tokio::net::TcpListener::from_std(listener).expect("chain listener"),
                app,
            )
            .await
            .expect("serve mock chain");
        });
    });
    (format!("http://{addr}"), chain)
}

// ── DB seeding + real router ───────────────────────────────────────────────

pub struct World {
    pub state: AppState,
    pub stream_keys: Vec<String>,
    pub db_pool: Arc<DbPool>,
    pub master_key: Arc<MasterKey>,
    pub model: String,
}

pub async fn build_world(cfg: &Config, upstream_url: &str) -> anyhow::Result<World> {
    openproxy_core::install_rustls_crypto_provider();
    // OPENPROXY_ALLOW_PRIVATE_UPSTREAMS is irrelevant here: the dev-dependency
    // `ssrf-bypass` feature of openproxy-adapters turns the loopback filter off
    // for this example build (same mechanism the e2e suite relies on).
    let dir = std::env::temp_dir().join(format!("bench-mixed-load-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).context("create bench dir")?;
    let synchronous = if cfg.synchronous == "normal" {
        openproxy_types::SqliteSynchronous::Normal
    } else {
        openproxy_types::SqliteSynchronous::Full
    };
    let (pool, master_key) = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let pool = Arc::new(DbPool::open_with_options(
            &dir.join("bench.db"),
            0,
            synchronous,
        )?);
        migrations::run(&mut pool.writer())?;
        Ok((pool, Arc::new(MasterKey::generate()?)))
    })
    .await??;

    let provider = ProviderId::new("bench-upstream");
    {
        let w = pool.writer();
        admin::create_provider(
            &w,
            CreateProviderInput {
                id: provider.as_str().into(),
                name: "Bench Mixed Upstream".into(),
                base_url: upstream_url.to_string(),
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
            &master_key,
            CreateAccountInput {
                provider_id: provider.as_str().into(),
                api_key: Some("sk-bench-upstream".into()),
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
                display_name: Some("Bench Stream".into()),
                target_format: "openai".into(),
                ttl_seconds: 3600,
                model_type: Some("chat".into()),
            },
        )?;
        // Optional catalog realism: N extra models the /v1/models read path
        // has to walk (default off; `--full` seeds 1000).
        for i in 0..cfg.seed_catalog {
            admin::create_custom_model(
                &w,
                CreateCustomModelInput {
                    provider_id: provider.as_str().into(),
                    model_id: format!("bench-catalog-{i}"),
                    display_name: Some(format!("Bench Catalog {i}")),
                    target_format: "openai".into(),
                    ttl_seconds: 3600,
                    model_type: Some("chat".into()),
                },
            )?;
        }
        // Optional historical `usage` rows so analytics-shaped reads hit a
        // non-empty table (default off; `--full` seeds 100_000).
        if cfg.seed_usage > 0 {
            seed_usage_history(&w, cfg.seed_usage)?;
        }
        // One sellable plan tier ($1 -> 1 credit, 7d TTL) for the funnel.
        openproxy_core::credits::tiers::create(
            &w,
            "bench-plan",
            &openproxy_core::credits::TierConfig::default(),
            &openproxy_core::credits::PlanCommerce {
                price_usd: Some(1.0),
                credit_usd: Some(1.0),
                ttl_days: 7,
                stock: None,
            },
        )?;
    }

    let mut app_config = AppConfig::default();
    app_config.server.allow_anonymous = false;
    // Reader count: left at the server default (core count). An A/B against
    // 16 readers showed no read-path gain (the reader lock p95 is < 1 µs
    // either way) while quota p50 drifted from ~1.6 s to ~2.9 s — more
    // readers only add scheduler pressure on a 4-core host.
    // Bench isolation: lift the per-key RPM limiter (NOT the concurrency caps,
    // which stay at production 10/key + 512/global) so throughput measures the
    // pipeline, not the sliding window.
    app_config.server.rate_limit_requests_per_minute = 1_000_000;
    app_config.credits = Some(openproxy_types::config::CreditsConfig {
        rpc_url: String::new(), // replaced by the caller with the mock chain URL
        usdc_address: String::from("0x833589fCD6eDb6E08f4c7C32D4f71b54bdA02913"),
        collector_address: String::from("0x12c7ab78f21ba0253a21d34a0151d44ddb824950"),
        payment_window_minutes: 15,
        scan_interval_secs: 1,
        ttl_days: vec![7, 15, 30],
        chain_id: 8453,
        max_block_chunk: 500,
        payment_tolerance_units: 250_000,
        checkout_order_creations_per_ip: 100_000,
        checkout_order_window_secs: 300,
        checkout_poll_per_ip_per_min: 100_000,
    });
    let state = AppState::for_test(
        app_config,
        Arc::clone(&pool),
        Arc::clone(&master_key),
        Arc::new(parking_lot::RwLock::new(Arc::new(vec![]))),
    );
    // Production admission caps, set explicitly for the report.
    state.set_max_concurrent_per_key(cfg.per_key_limit);
    state.set_max_concurrent_global(cfg.global_limit);
    // Cancel every background daemon that is not part of the measured load.
    state.discovery_scheduler().cancel();
    state.supervisor().shutdown();
    state.discovery_scheduler().shutdown_and_wait().await;
    state.supervisor().shutdown_and_wait().await;
    state.rebuild_adapters().await?;

    // Distinct client keys for the stream cohort: the real inflight limiter
    // holds 10 per key, so >= ceil(streams/10) keys are REQUIRED, not cosmetic.
    let keys_state = state.clone();
    let keys_count = cfg.keys;
    let stream_keys: Vec<String> = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        (0..keys_count)
            .map(|i| {
                let (record, plaintext) = keys_state.services().api_keys.create(
                    CreateApiKeyInput {
                        label: Some(format!("bench-stream-{i}")),
                        scopes: vec!["chat".into()],
                        allowed_models: None,
                        allowed_combos: None,
                        blacklisted_providers: None,
                        allowed_providers: None,
                        blacklisted_models: None,
                        only_whitelisted: false,
                        expires_at: None,
                    },
                    "bench_setup",
                )?;
                keys_state.cache_api_key(Arc::new(record));
                Ok(plaintext)
            })
            .collect()
    })
    .await??;

    Ok(World {
        state,
        stream_keys,
        db_pool: pool,
        master_key,
        model: "bench-stream-model".into(),
    })
}

fn seed_usage_history(w: &rusqlite::Connection, rows: usize) -> anyhow::Result<()> {
    let mut stmt = w.prepare_cached(
        "INSERT INTO usage(request_id, trace_id, provider_id, upstream_model_id, \
         attempt, race_total, race_lost, total_ms, status_code, client_response, \
         cost_usd, prompt_tokens, completion_tokens) \
         VALUES (?1, ?1, 'bench-upstream', 'bench-stream-model', 1, 1, 0, 250, 200, 1, \
         0.0000125, 10, 2)",
    )?;
    w.execute_batch("BEGIN")?;
    for i in 0..rows {
        stmt.execute([&format!("hist-{i}")])?;
    }
    w.execute_batch("COMMIT")?;
    Ok(())
}

/// Serves the REAL router (build_router output) on a real loopback TCP
/// listener with ConnectInfo, exactly like production main.rs does.
pub async fn serve_router(state: AppState) -> anyhow::Result<SocketAddr> {
    let listener = openproxy_server::listener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let app = build_router(state).into_make_service_with_connect_info::<std::net::SocketAddr>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve bench router");
    });
    Ok(addr)
}

/// Sleep helper so callers can await a deadline without importing tokio here.
pub async fn sleep(d: Duration) {
    tokio::time::sleep(d).await;
}

/// Wall time of one phase, for the report.
pub struct Phase {
    pub name: &'static str,
    pub started: Instant,
}

pub fn phase(name: &'static str) -> Phase {
    Phase {
        name,
        started: Instant::now(),
    }
}

impl Phase {
    pub fn elapsed_us(&self) -> u64 {
        self.started.elapsed().as_micros() as u64
    }
}
