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

#[derive(Clone, Copy, Default)]
struct ClientPattern {
    read_delay_ms: u64,
    cancel_after_tokens: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct RequestResult {
    pub id: usize,
    pub status: Option<u16>,
    pub error: Option<String>,
    /// Intentional cancellation is not a validation success or a failure.
    #[serde(default)]
    pub cancelled_as_planned: bool,
    #[serde(default)]
    pub planned_start_offset_us: u64,
    #[serde(default)]
    pub scheduling_lag_us: u64,
    pub headers_us: Option<u64>,
    pub ttft_us: Option<u64>,
    pub total_us: u64,
    pub body_bytes: u64,
    pub http_frames: u64,
    /// Largest gap between consecutive body frames after the first one.
    #[serde(default)]
    pub max_gap_us: u64,
    /// Frames that start with an SSE comment (proxy keep-alive).
    #[serde(default)]
    pub keepalive_frames: u64,
    /// Whether the frame closing the largest gap was a keep-alive comment.
    #[serde(default)]
    pub max_gap_closed_by_keepalive: bool,
    /// Offset from request start where the largest gap ended.
    #[serde(default)]
    pub max_gap_end_us: u64,
    /// Data frames received before / after the largest gap.
    #[serde(default)]
    pub frames_before_max_gap: u64,
    /// Offset of the last frame carrying synthetic content tokens.
    #[serde(default)]
    pub last_token_us: Option<u64>,
    /// Offset of the frame carrying `data: [DONE]`.
    #[serde(default)]
    pub done_us: Option<u64>,
    /// Request start relative to the batch start (after the shared barrier).
    #[serde(default)]
    pub start_offset_us: u64,
    /// Largest gap between two frames that both carry content tokens.
    /// Keep-alive comments and the post-`[DONE]` tail are excluded.
    #[serde(default)]
    pub max_token_gap_us: u64,
    pub stream: Validator,
    /// Aggregated into [`Batch`] before emitting; not serialized per request.
    #[serde(skip)]
    pub token_gaps: GapHistogram,
    #[serde(skip)]
    pub timeline: Vec<u64>,
}

/// Width of one throughput timeline window.
pub const TIMELINE_WINDOW_US: u64 = 100_000;

/// Gaps between consecutive token-carrying frames, bucketed by
/// `floor(log2(gap_us))`. Bucket `i` holds gaps in `[2^i, 2^(i+1))` µs, bucket
/// 0 also holds 0 µs.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct GapHistogram(pub Vec<u64>);

impl GapHistogram {
    pub fn record(&mut self, gap_us: u64) {
        let bucket = (u64::BITS - gap_us.max(1).leading_zeros() - 1) as usize;
        if self.0.len() <= bucket {
            self.0.resize(bucket + 1, 0);
        }
        self.0[bucket] += 1;
    }

    pub fn merge(&mut self, other: &Self) {
        if self.0.len() < other.0.len() {
            self.0.resize(other.0.len(), 0);
        }
        for (a, b) in self.0.iter_mut().zip(&other.0) {
            *a += b;
        }
    }

    /// Upper bound (exclusive, µs) of the bucket holding the nearest-rank
    /// quantile `per_mille`/1000. Resolution is a factor of two by design.
    pub fn quantile_upper_bound_us(&self, per_mille: u64) -> Option<u64> {
        let total: u64 = self.0.iter().sum();
        if total == 0 {
            return None;
        }
        let rank = (total * per_mille).div_ceil(1000).max(1);
        let mut seen = 0;
        self.0.iter().enumerate().find_map(|(i, n)| {
            seen += n;
            if seen >= rank {
                1u64.checked_shl((i + 1) as u32)
            } else {
                None
            }
        })
    }
}

#[derive(Serialize, Deserialize)]
pub struct Batch {
    pub elapsed_us: u64,
    pub requests: Vec<RequestResult>,
    /// All token-to-token frame gaps of every request (log2 µs buckets).
    #[serde(default)]
    pub token_gap_histogram_log2_us: GapHistogram,
    /// Validated content tokens received per 100 ms window since batch start.
    #[serde(default)]
    pub tokens_per_window: Vec<u64>,
}

async fn request(
    client: HttpClient,
    request: axum::http::Request<Full<Bytes>>,
    id: usize,
    tokens: usize,
    batch_start: Arc<std::sync::OnceLock<Instant>>,
    planned_start_offset_us: u64,
    pattern: ClientPattern,
) -> RequestResult {
    let start = Instant::now();
    let batch_start = *batch_start.get_or_init(|| start);
    let mut result = RequestResult {
        id,
        status: None,
        error: None,
        cancelled_as_planned: false,
        planned_start_offset_us,
        scheduling_lag_us: start
            .saturating_duration_since(batch_start + Duration::from_micros(planned_start_offset_us))
            .as_micros() as u64,
        headers_us: None,
        ttft_us: None,
        total_us: 0,
        body_bytes: 0,
        http_frames: 0,
        max_gap_us: 0,
        keepalive_frames: 0,
        max_gap_closed_by_keepalive: false,
        max_gap_end_us: 0,
        frames_before_max_gap: 0,
        last_token_us: None,
        done_us: None,
        start_offset_us: start.saturating_duration_since(batch_start).as_micros() as u64,
        max_token_gap_us: 0,
        stream: Validator::default(),
        token_gaps: GapHistogram::default(),
        timeline: Vec::new(),
    };
    let mut last_token_frame: Option<Instant> = None;
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
        loop {
            if pattern.read_delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(pattern.read_delay_ms)).await;
            }
            let Some(frame) = body.frame().await else {
                break;
            };
            let frame = frame?;
            if let Some(data) = frame.data_ref() {
                let now = Instant::now();
                let keepalive = data.starts_with(b":");
                result.keepalive_frames += u64::from(keepalive);
                if let Some(previous) = last_frame {
                    let gap = now.duration_since(previous).as_micros() as u64;
                    if gap > result.max_gap_us {
                        result.max_gap_us = gap;
                        result.max_gap_closed_by_keepalive = keepalive;
                        result.max_gap_end_us = start.elapsed().as_micros() as u64;
                        result.frames_before_max_gap = result.http_frames;
                    }
                }
                last_frame = Some(now);
                result.http_frames += 1;
                let offset = start.elapsed().as_micros() as u64;
                let tokens_before = result.stream.tokens;
                let done_before = result.stream.done;
                let content_received = result.stream.push(data)?;
                let frame_tokens = result.stream.tokens.saturating_sub(tokens_before);
                if frame_tokens > 0 {
                    result.last_token_us = Some(offset);
                    if let Some(previous) = last_token_frame {
                        let gap = now.duration_since(previous).as_micros() as u64;
                        result.max_token_gap_us = result.max_token_gap_us.max(gap);
                        result.token_gaps.record(gap);
                    }
                    last_token_frame = Some(now);
                    let window = ((result.start_offset_us + offset) / TIMELINE_WINDOW_US) as usize;
                    if result.timeline.len() <= window {
                        result.timeline.resize(window + 1, 0);
                    }
                    result.timeline[window] += frame_tokens;
                }
                if !done_before && result.stream.done {
                    result.done_us = Some(offset);
                }
                result.body_bytes += data.len() as u64;
                if content_received && result.ttft_us.is_none() {
                    result.ttft_us = Some(offset);
                }
                if pattern.cancel_after_tokens > 0
                    && result.stream.tokens >= pattern.cancel_after_tokens as u64
                {
                    ensure!(
                        !result.stream.done,
                        "stream finished before deliberate cancellation"
                    );
                    result.cancelled_as_planned = true;
                    // Dropping the HTTP body closes this response, not the
                    // other clients. Partial tokens never count as validated.
                    return Ok(());
                }
            }
        }
        ensure!(
            pattern.cancel_after_tokens == 0,
            "stream ended before cancellation threshold"
        );
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
    is_warmup: bool,
) -> anyhow::Result<axum::http::Request<Full<Bytes>>> {
    let key = config
        .keys
        .get(id % config.keys.len())
        .context("missing key")?;
    let builder = axum::http::Request::builder()
        .method("POST")
        .uri(format!(
            "{}/v1/chat/completions",
            config.url.as_deref().context("missing proxy URL")?
        ))
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {key}"));
    Ok(builder.body(Full::new(Bytes::from(
        json!({"model":"bench-stream-model","stream":true,"stream_options":{"include_usage":true},"messages":[{"role":"user","content":format!("bench client {} request #{id}", if is_warmup { "warmup" } else { "stream" })}]}).to_string(),
    )))?)
}

pub async fn run(config: WorkerConfig) -> anyhow::Result<()> {
    ensure!(!config.keys.is_empty(), "missing keys");
    let rounds = config.rounds.max(1);
    let client: HttpClient = Client::builder(TokioExecutor::new())
        .pool_max_idle_per_host(config.concurrency)
        .build_http();
    for id in 0..config.keys.len() {
        let result = request(
            client.clone(),
            build_request(&config, id, true)?,
            id,
            config.tokens,
            Arc::default(),
            0,
            ClientPattern::default(),
        )
        .await;
        ensure!(result.error.is_none(), "warmup failed: {:?}", result.error);
    }
    emit(&json!({"ready":true,"pid":std::process::id()}))?;
    ensure!(
        tokio::task::spawn_blocking(read_line).await?? == "go",
        "expected go"
    );

    let mut all_results = Vec::with_capacity(config.concurrency * rounds);
    let total_start = Instant::now();
    let batch_start = Arc::new(std::sync::OnceLock::<Instant>::new());
    let _ = batch_start.set(total_start);

    for round in 0..rounds {
        let round_offset_us = total_start.elapsed().as_micros() as u64;
        let barrier = Arc::new(tokio::sync::Barrier::new(config.concurrency + 1));
        let mut tasks = tokio::task::JoinSet::new();

        for i in 0..config.concurrency {
            let req_id = round * config.concurrency + i;
            let request_body = build_request(&config, req_id, false)?;
            let worker_client = client.clone();
            let worker_barrier = Arc::clone(&barrier);
            let worker_start = Arc::clone(&batch_start);
            let tokens = config.tokens;
            let planned = round_offset_us
                + (i as u64 * 1_000_000)
                    .checked_div(config.arrival_rate_rps)
                    .unwrap_or(0);
            let pattern = ClientPattern {
                read_delay_ms: config.read_delay_ms,
                cancel_after_tokens: if (i as u64 * 100 / config.concurrency as u64)
                    < config.cancel_percent
                {
                    config.cancel_after_tokens
                } else {
                    0
                },
            };
            tasks.spawn(async move {
                worker_barrier.wait().await;
                if planned > 0 {
                    let start = *worker_start.get_or_init(Instant::now);
                    tokio::time::sleep_until((start + Duration::from_micros(planned)).into()).await;
                }
                request(
                    worker_client,
                    request_body,
                    req_id,
                    tokens,
                    worker_start,
                    planned,
                    pattern,
                )
                .await
            });
        }

        barrier.wait().await;
        while let Some(result) = tasks.join_next().await {
            all_results.push(result?);
        }
    }

    let elapsed_us = total_start.elapsed().as_micros() as u64;
    // All requests across all rounds have completed. Close idle pooled connections
    // before coordinator drains the server, keeping this process alive for /proc.
    drop(client);
    all_results.sort_unstable_by_key(|r| r.id);
    let mut token_gaps = GapHistogram::default();
    let mut tokens_per_window = Vec::new();
    // Only validated streams count, like the headline throughput.
    for r in all_results
        .iter()
        .filter(|r| r.error.is_none() && !r.cancelled_as_planned)
    {
        token_gaps.merge(&r.token_gaps);
        if tokens_per_window.len() < r.timeline.len() {
            tokens_per_window.resize(r.timeline.len(), 0);
        }
        for (total, n) in tokens_per_window.iter_mut().zip(&r.timeline) {
            *total += n;
        }
    }
    let validated_tokens: u64 = all_results
        .iter()
        .filter(|r| r.error.is_none() && !r.cancelled_as_planned)
        .map(|r| r.stream.tokens)
        .sum();
    ensure!(
        tokens_per_window.iter().sum::<u64>() == validated_tokens,
        "timeline token count differs from validated text"
    );
    emit(&Batch {
        elapsed_us,
        requests: all_results,
        token_gap_histogram_log2_us: token_gaps,
        tokens_per_window,
    })?;
    // Keep /proc available until the coordinator's final resource snapshot.
    tokio::task::spawn_blocking(read_line).await??;
    Ok(())
}

pub fn self_test() -> anyhow::Result<()> {
    let mut h = GapHistogram::default();
    for gap in [0, 1, 3, 1000, 1023, 1024] {
        h.record(gap);
    }
    // 0,1 -> bucket 0; 3 -> 1; 1000,1023 -> 9; 1024 -> 10
    ensure!(
        h.0 == vec![2, 1, 0, 0, 0, 0, 0, 0, 0, 2, 1],
        "log2 buckets {:?}",
        h.0
    );
    ensure!(h.quantile_upper_bound_us(500) == Some(4), "p50 bound");
    ensure!(h.quantile_upper_bound_us(1000) == Some(2048), "p100 bound");
    ensure!(
        GapHistogram::default()
            .quantile_upper_bound_us(500)
            .is_none(),
        "empty histogram"
    );
    Ok(())
}
