use crate::{
    config::{WorkerConfig, emit, read_line},
    validate::Validator,
};
use anyhow::{Context, ensure};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

type HttpClient = Client<HttpConnector, Full<Bytes>>;

#[derive(Debug, Serialize, Deserialize)]
pub struct RequestResult {
    pub id: usize,
    pub status: Option<u16>,
    pub error: Option<String>,
    pub headers_us: Option<u64>,
    pub ttft_us: Option<u64>,
    pub total_us: u64,
    pub body_bytes: u64,
    pub http_frames: u64,
    /// Largest gap between consecutive body frames after the first one.
    #[serde(default)]
    pub max_gap_us: u64,
    pub stream: Validator,
}

#[derive(Serialize, Deserialize)]
pub struct Batch {
    pub elapsed_us: u64,
    pub requests: Vec<RequestResult>,
}

async fn request(
    client: HttpClient,
    request: axum::http::Request<Full<Bytes>>,
    id: usize,
    tokens: usize,
) -> RequestResult {
    let start = Instant::now();
    let mut result = RequestResult {
        id,
        status: None,
        error: None,
        headers_us: None,
        ttft_us: None,
        total_us: 0,
        body_bytes: 0,
        http_frames: 0,
        max_gap_us: 0,
        stream: Validator::default(),
    };
    let outcome = tokio::time::timeout(Duration::from_secs(120), async {
        let response = client.request(request).await?;
        result.headers_us = Some(start.elapsed().as_micros() as u64);
        result.status = Some(response.status().as_u16());
        ensure!(response.status().is_success(), "HTTP {}", response.status());
        ensure!(
            response
                .headers()
                .get("content-type")
                .is_some_and(|s| s.as_bytes().starts_with(b"text/event-stream")),
            "not SSE"
        );
        let mut body = response.into_body();
        let mut last_frame: Option<Instant> = None;
        while let Some(frame) = body.frame().await {
            let frame = frame?;
            if let Some(data) = frame.data_ref() {
                let now = Instant::now();
                if let Some(previous) = last_frame {
                    result.max_gap_us = result
                        .max_gap_us
                        .max(now.duration_since(previous).as_micros() as u64);
                }
                last_frame = Some(now);
                result.http_frames += 1;
                result.body_bytes += data.len() as u64;
                if result.stream.push(data)? && result.ttft_us.is_none() {
                    result.ttft_us = Some(start.elapsed().as_micros() as u64);
                }
            }
        }
        result.stream.complete(tokens)
    })
    .await;
    result.total_us = start.elapsed().as_micros() as u64;
    result.error = match outcome {
        Ok(Ok(())) => None,
        Ok(Err(e)) => Some(format!("{e:#}")),
        Err(e) => Some(e.to_string()),
    };
    result
}

fn build_request(
    config: &WorkerConfig,
    id: usize,
) -> anyhow::Result<axum::http::Request<Full<Bytes>>> {
    let key = config
        .keys
        .get(id % config.keys.len())
        .context("missing key")?;
    Ok(axum::http::Request::builder().method("POST")
        .uri(format!("{}/v1/chat/completions", config.url.as_deref().context("missing proxy URL")?))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"))
        .body(Full::new(Bytes::from(json!({"model":"bench-stream-model","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":format!("bench client stream request #{id}")}]}).to_string())))?)
}

pub async fn run(config: WorkerConfig) -> anyhow::Result<()> {
    ensure!(!config.keys.is_empty(), "missing keys");
    let client: HttpClient = Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(config.concurrency)
        .build_http();
    for id in 0..config.keys.len() {
        let result = request(
            client.clone(),
            build_request(&config, id)?,
            id,
            config.tokens,
        )
        .await;
        ensure!(result.error.is_none(), "warmup failed: {:?}", result.error);
    }
    // Prebuild requests and spawn blocked workers before starting the timer.
    let barrier = Arc::new(tokio::sync::Barrier::new(config.concurrency + 1));
    let mut tasks = tokio::task::JoinSet::new();
    for id in 0..config.concurrency {
        let request_body = build_request(&config, id)?;
        let worker_client = client.clone();
        let worker_barrier = Arc::clone(&barrier);
        let tokens = config.tokens;
        tasks.spawn(async move {
            worker_barrier.wait().await;
            request(worker_client, request_body, id, tokens).await
        });
    }
    emit(&json!({"ready":true,"pid":std::process::id()}))?;
    ensure!(
        tokio::task::spawn_blocking(read_line).await?? == "go",
        "expected go"
    );
    let start = Instant::now();
    barrier.wait().await;
    let mut results = Vec::with_capacity(config.concurrency);
    while let Some(result) = tasks.join_next().await {
        results.push(result?);
    }
    let elapsed_us = start.elapsed().as_micros() as u64;
    // All requests have completed. Close idle pooled connections before the
    // coordinator drains the server, keeping this process alive for /proc.
    drop(client);
    results.sort_unstable_by_key(|r| r.id);
    emit(&Batch {
        elapsed_us,
        requests: results,
    })?;
    // Keep /proc available until the coordinator's final resource snapshot.
    tokio::task::spawn_blocking(read_line).await??;
    Ok(())
}
