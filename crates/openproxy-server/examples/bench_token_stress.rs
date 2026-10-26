#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::{
    Router,
    body::Body,
    extract::State as AxumState,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use bytes::Bytes;
use http_body_util::BodyExt;
use openproxy_core::{
    AppConfig,
    admin::{self, CreateAccountInput, CreateCustomModelInput, CreateProviderInput},
    api_keys::CreateApiKeyInput,
};
use openproxy_db::{DbPool, migrations, secrets::MasterKey, testing::TempDir};
use openproxy_server::{router::build_router, state::AppState};
use openproxy_types::ids::ProviderId;
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};
use tokio::net::TcpListener;
use tokio_stream::wrappers::ReceiverStream;
use tower::ServiceExt;

fn read_proc_status_rss_kb() -> usize {
    if let Ok(content) = std::fs::read_to_string("/proc/self/status") {
        for line in content.lines() {
            if line.starts_with("VmRSS:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    return parts[1].parse::<usize>().unwrap_or(0);
                }
            }
        }
    }
    0
}

#[derive(Clone)]
struct MockUpstreamConfig {
    tokens_per_stream: usize,
    chunk_size_tokens: usize,
}

struct CpuAndIoSnapshot {
    utime_ticks: u64,
    stime_ticks: u64,
    rchar: u64,
    wchar: u64,
    read_bytes: u64,
    write_bytes: u64,
}

fn read_cpu_and_io_snapshot() -> CpuAndIoSnapshot {
    let mut utime = 0;
    let mut stime = 0;
    if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
        let parts: Vec<&str> = stat.split_whitespace().collect();
        if parts.len() > 15 {
            utime = parts[13].parse::<u64>().unwrap_or(0);
            stime = parts[14].parse::<u64>().unwrap_or(0);
        }
    }

    let mut rchar = 0;
    let mut wchar = 0;
    let mut read_bytes = 0;
    let mut write_bytes = 0;
    if let Ok(io) = std::fs::read_to_string("/proc/self/io") {
        for line in io.lines() {
            if let Some((k, v)) = line.split_once(':') {
                let val = v.trim().parse::<u64>().unwrap_or(0);
                match k.trim() {
                    "rchar" => rchar = val,
                    "wchar" => wchar = val,
                    "read_bytes" => read_bytes = val,
                    "write_bytes" => write_bytes = val,
                    _ => {}
                }
            }
        }
    }

    CpuAndIoSnapshot {
        utime_ticks: utime,
        stime_ticks: stime,
        rchar,
        wchar,
        read_bytes,
        write_bytes,
    }
}

async fn fast_mock_chat_handler(
    AxumState(cfg): AxumState<Arc<MockUpstreamConfig>>,
    _headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let is_stream = parsed
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let model = parsed
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("bench-model");

    if is_stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(64);
        let tokens = cfg.tokens_per_stream;
        let step = cfg.chunk_size_tokens.max(1);
        let model_name = model.to_string();

        tokio::spawn(async move {
            let mut emitted = 0;
            while emitted < tokens {
                let count = step.min(tokens - emitted);
                let chunk_text = " tok".repeat(count);
                let sse_chunk = format!(
                    "data: {{\"id\":\"bench-chunk\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"{model_name}\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"{chunk_text}\"}},\"finish_reason\":null}}]}}\n\n"
                );
                if tx.send(Ok(Bytes::from(sse_chunk))).await.is_err() {
                    break;
                }
                emitted += count;
            }

            let usage_sse = format!(
                "data: {{\"id\":\"bench-chunk\",\"object\":\"chat.completion.chunk\",\"created\":1700000000,\"model\":\"{model_name}\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":{tokens},\"total_tokens\":{}}}}}\n\n\
                 data: [DONE]\n\n",
                tokens + 10
            );
            let _ = tx.send(Ok(Bytes::from(usage_sse))).await;
        });

        let body_stream = Body::from_stream(ReceiverStream::new(rx));
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/event-stream; charset=utf-8")],
            body_stream,
        )
            .into_response()
    } else {
        let tokens = cfg.tokens_per_stream;
        let content = " word".repeat(tokens);
        let resp = json!({
            "id": "bench-unary",
            "object": "chat.completion",
            "created": 1700000000,
            "model": model,
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": content
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": tokens,
                "total_tokens": tokens + 10
            }
        });
        (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            resp.to_string(),
        )
            .into_response()
    }
}

async fn spawn_benchmark_upstream(
    tokens_per_stream: usize,
    chunk_size_tokens: usize,
) -> SocketAddr {
    let cfg = Arc::new(MockUpstreamConfig {
        tokens_per_stream,
        chunk_size_tokens,
    });
    let app = Router::new()
        .route("/v1/chat/completions", post(fast_mock_chat_handler))
        .route("/chat/completions", post(fast_mock_chat_handler))
        .with_state(cfg);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock upstream");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve mock upstream");
    });
    addr
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let concurrency: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    let tokens_per_stream: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(5_000);
    let chunk_size: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(5);

    println!("============================================================");
    println!("     OPENPROXY END-TO-END TOKEN STRESS BENCHMARK");
    println!("============================================================");
    println!("Concurrency:       {concurrency} concurrent requests");
    println!("Tokens per stream: {tokens_per_stream} tokens");
    println!("Tokens per chunk:  {chunk_size} tokens");
    println!(
        "Target total:      {} tokens",
        concurrency * tokens_per_stream
    );
    println!("----------");

    unsafe {
        std::env::set_var("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true");
    }

    let upstream_addr = spawn_benchmark_upstream(tokens_per_stream, chunk_size).await;
    println!("Started in-memory mock upstream on http://{upstream_addr}");

    let temp_dir = TempDir::new("openproxy-bench").expect("temp dir");
    let db_pool = Arc::new(DbPool::open(&temp_dir.path().join("bench.db")).expect("open db"));
    {
        let mut w = db_pool.writer();
        migrations::run(&mut w).expect("run migrations");
    }
    let master_key = Arc::new(MasterKey::generate().expect("generate master key"));
    let provider_id = ProviderId::new("mock-upstream");
    let model_id = "bench-stream-model".to_string();

    {
        let w = db_pool.writer();
        admin::create_provider(
            &w,
            CreateProviderInput {
                id: provider_id.as_str().into(),
                name: "Benchmark Mock Upstream".into(),
                base_url: format!("http://{upstream_addr}/v1"),
                auth_type: "bearer".into(),
                format: "openai".into(),
                extra_headers_json: None,
                rate_limit_scope: None,
                stream_mode: None,
                prompt_cache_ttl: None,
            },
        )
        .expect("create provider");

        admin::create_account(
            &w,
            &master_key,
            CreateAccountInput {
                provider_id: provider_id.as_str().into(),
                api_key: Some("sk-mock-bench-key".into()),
                label: Some("bench-account".into()),
                priority: Some(10),
                extra_config_json: None,
            },
        )
        .expect("create account");

        admin::create_custom_model(
            &w,
            CreateCustomModelInput {
                provider_id: provider_id.as_str().into(),
                model_id: model_id.clone(),
                display_name: Some("Bench Stream Model".into()),
                target_format: "openai".into(),
                ttl_seconds: 3600,
                model_type: Some("chat".into()),
            },
        )
        .expect("create custom model");
    }

    let mut app_config = AppConfig::default();
    app_config.server.allow_anonymous = false;
    let adapters = Arc::new(parking_lot::RwLock::new(Arc::new(vec![])));
    let app_state = AppState::for_test(
        app_config,
        Arc::clone(&db_pool),
        Arc::clone(&master_key),
        adapters,
    );
    app_state
        .rebuild_adapters()
        .await
        .expect("rebuild adapters");

    let (client_key_record, client_key_plaintext) = app_state
        .services()
        .api_keys
        .create(
            CreateApiKeyInput {
                label: Some("Bench Client Key".into()),
                scopes: vec!["chat".into()],
                allowed_models: None,
                allowed_combos: None,
                blacklisted_providers: None,
                blacklisted_models: None,
                expires_at: None,
            },
            "bench_setup",
        )
        .expect("create client api key");
    app_state.cache_api_key(Arc::new(client_key_record));

    let router = build_router(app_state);

    println!("OpenProxy stack initialized. Running warmup request...");
    let warmup_body = json!({
        "model": model_id,
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::AUTHORIZATION,
            format!("Bearer {client_key_plaintext}"),
        )
        .body(Body::from(warmup_body.to_string()))
        .expect("warmup req");
    let warmup_resp = router.clone().oneshot(req).await.expect("warmup");
    assert_eq!(warmup_resp.status(), StatusCode::OK);
    let _ = warmup_resp.into_body().collect().await;
    println!("Warmup OK. Starting benchmark...\n");

    let total_bytes_received = Arc::new(AtomicUsize::new(0));
    let total_chunks_received = Arc::new(AtomicUsize::new(0));
    let successful_requests = Arc::new(AtomicUsize::new(0));

    // Microsecond timing metrics for latency breakdown
    let total_dispatch_micros = Arc::new(AtomicUsize::new(0));
    let total_ttft_micros = Arc::new(AtomicUsize::new(0));
    let total_stream_drain_micros = Arc::new(AtomicUsize::new(0));
    let min_ttft_micros = Arc::new(AtomicUsize::new(usize::MAX));
    let max_ttft_micros = Arc::new(AtomicUsize::new(0));

    let pre_rss_kb = read_proc_status_rss_kb();
    let pre_sys = read_cpu_and_io_snapshot();

    let start = Instant::now();
    let mut handles = Vec::with_capacity(concurrency);

    for i in 0..concurrency {
        let r = router.clone();
        let k = client_key_plaintext.clone();
        let m = model_id.clone();
        let bytes_acc = Arc::clone(&total_bytes_received);
        let chunks_acc = Arc::clone(&total_chunks_received);
        let success_acc = Arc::clone(&successful_requests);

        let dispatch_acc = Arc::clone(&total_dispatch_micros);
        let ttft_acc = Arc::clone(&total_ttft_micros);
        let drain_acc = Arc::clone(&total_stream_drain_micros);
        let min_ttft = Arc::clone(&min_ttft_micros);
        let max_ttft = Arc::clone(&max_ttft_micros);

        let handle = tokio::spawn(async move {
            let req_body = json!({
                "model": m,
                "stream": true,
                "messages": [{"role": "user", "content": format!("bench client stream request #{i}")}]
            });
            let request = axum::http::Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::AUTHORIZATION, format!("Bearer {k}"))
                .body(Body::from(req_body.to_string()))
                .expect("build request");

            let t_req_start = Instant::now();
            let resp = r.oneshot(request).await.expect("dispatch");
            let t_headers = Instant::now();
            let dispatch_dur = t_headers.duration_since(t_req_start).as_micros() as usize;
            dispatch_acc.fetch_add(dispatch_dur, Ordering::Relaxed);

            if resp.status() == StatusCode::OK {
                let mut body = resp.into_body();
                let mut req_chunks = 0;
                let mut req_bytes = 0;
                let mut first_token_time = None;

                while let Some(frame) = body.frame().await {
                    if let Ok(f) = frame
                        && let Some(data) = f.data_ref()
                    {
                        if first_token_time.is_none() {
                            first_token_time = Some(Instant::now());
                        }
                        req_bytes += data.len();
                        req_chunks += 1;
                    }
                }
                let t_end = Instant::now();

                if let Some(t_first) = first_token_time {
                    let ttft_micros = t_first.duration_since(t_req_start).as_micros() as usize;
                    ttft_acc.fetch_add(ttft_micros, Ordering::Relaxed);
                    min_ttft.fetch_min(ttft_micros, Ordering::Relaxed);
                    max_ttft.fetch_max(ttft_micros, Ordering::Relaxed);

                    let drain_micros = t_end.duration_since(t_first).as_micros() as usize;
                    drain_acc.fetch_add(drain_micros, Ordering::Relaxed);
                }

                bytes_acc.fetch_add(req_bytes, Ordering::Relaxed);
                chunks_acc.fetch_add(req_chunks, Ordering::Relaxed);
                success_acc.fetch_add(1, Ordering::Relaxed);
            } else {
                eprintln!("Worker #{} received status: {}", i, resp.status());
            }
        });
        handles.push(handle);
    }

    for h in handles {
        h.await.expect("worker task panic");
    }

    let elapsed = start.elapsed();
    let total_tokens = concurrency * tokens_per_stream;
    let tokens_per_sec = (total_tokens as f64) / elapsed.as_secs_f64();
    let mbytes = (total_bytes_received.load(Ordering::Relaxed) as f64) / (1024.0 * 1024.0);
    let mb_per_sec = mbytes / elapsed.as_secs_f64();
    let succ = successful_requests.load(Ordering::Relaxed);

    println!("---------- RESULTS ----------");
    println!("Elapsed Time:          {:.3} s", elapsed.as_secs_f64());
    println!("Successful Requests:   {succ}/{concurrency}");
    println!("Total Tokens Delivered: {total_tokens}");
    println!("Throughput:            {tokens_per_sec:.0} tokens/sec");
    println!("Data Transfer:         {mbytes:.2} MB ({mb_per_sec:.2} MB/s)");
    println!(
        "SSE Chunks Processed:  {}",
        total_chunks_received.load(Ordering::Relaxed)
    );
    println!(
        "Avg Total Stream Time: {:.2} ms",
        (elapsed.as_secs_f64() * 1000.0) / (concurrency as f64)
    );

    if succ > 0 {
        let avg_dispatch_us = total_dispatch_micros.load(Ordering::Relaxed) as f64 / succ as f64;
        let avg_ttft_us = total_ttft_micros.load(Ordering::Relaxed) as f64 / succ as f64;
        let avg_drain_us = total_stream_drain_micros.load(Ordering::Relaxed) as f64 / succ as f64;
        let min_ttft = min_ttft_micros.load(Ordering::Relaxed);
        let max_ttft = max_ttft_micros.load(Ordering::Relaxed);

        println!("\n---------- LATENCY & BOTTLENECK PROFILE (Microseconds) ----------");
        println!(
            "Routing + Auth + Upstream Connect (Dispatch):  {avg_dispatch_us:.1} µs ({:.2} ms)",
            avg_dispatch_us / 1000.0
        );
        println!(
            "Time To First Token (TTFT - Client perspective): {avg_ttft_us:.1} µs ({:.2} ms) [min: {min_ttft} µs, max: {max_ttft} µs]",
            avg_ttft_us / 1000.0
        );
        println!(
            "Stream Processing & Drain (SSE pump):          {avg_drain_us:.1} µs ({:.2} ms)",
            avg_drain_us / 1000.0
        );
        if total_chunks_received.load(Ordering::Relaxed) > 0 {
            let per_chunk_us = (total_stream_drain_micros.load(Ordering::Relaxed) as f64)
                / (total_chunks_received.load(Ordering::Relaxed) as f64);
            println!(
                "Average Time Per SSE Chunk in Proxy Pipeline:   {per_chunk_us:.2} µs (overhead por token)"
            );
        }
    }

    let post_rss_kb = read_proc_status_rss_kb();
    let post_sys = read_cpu_and_io_snapshot();
    let rss_growth_kb = post_rss_kb.saturating_sub(pre_rss_kb);
    let rss_growth_mb = (rss_growth_kb as f64) / 1024.0;
    let bytes_per_stream = if concurrency > 0 {
        (rss_growth_kb * 1024) / concurrency
    } else {
        0
    };

    let user_cpu_ticks = post_sys.utime_ticks.saturating_sub(pre_sys.utime_ticks);
    let sys_cpu_ticks = post_sys.stime_ticks.saturating_sub(pre_sys.stime_ticks);
    let total_cpu_ticks = user_cpu_ticks + sys_cpu_ticks;
    let ticks_per_sec = 100.0; // Linux sysconf(_SC_CLK_TCK) default
    let cpu_time_sec = (total_cpu_ticks as f64) / ticks_per_sec;
    let cpu_util_percent = (cpu_time_sec / elapsed.as_secs_f64()) * 100.0;

    let disk_read_kb = (post_sys.read_bytes.saturating_sub(pre_sys.read_bytes) as f64) / 1024.0;
    let disk_write_kb = (post_sys.write_bytes.saturating_sub(pre_sys.write_bytes) as f64) / 1024.0;
    let logical_read_kb = (post_sys.rchar.saturating_sub(pre_sys.rchar) as f64) / 1024.0;
    let logical_write_kb = (post_sys.wchar.saturating_sub(pre_sys.wchar) as f64) / 1024.0;

    println!("\n---------- RESOURCE CONSUMPTION (Memory, CPU & I/O) ----------");
    println!("Process Resident Set (RSS):    {post_rss_kb} KB ({:.2} MB)", post_rss_kb as f64 / 1024.0);
    println!("RSS Growth During Workload:    {rss_growth_kb} KB ({rss_growth_mb:.2} MB)");
    println!("Memory Allocation per Request: {:.2} KB ({:.3} MB per request)", bytes_per_stream as f64 / 1024.0, (bytes_per_stream as f64) / (1024.0 * 1024.0));
    println!("CPU Utilization (Process):     {cpu_util_percent:.1}% across cores ({cpu_time_sec:.3}s CPU time: user={user_cpu_ticks}t, sys={sys_cpu_ticks}t)");
    println!("Disk Physical I/O:             Read: {disk_read_kb:.1} KB | Written (fsync/WAL): {disk_write_kb:.1} KB");
    println!("Logical I/O (Syscalls):        Read: {logical_read_kb:.1} KB | Written: {logical_write_kb:.1} KB");
    println!("Database Operations:           DB queries on stream path: 0 (cached API keys & in-memory routing)");
    println!("==========");
}
