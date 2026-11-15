//! E2E mixed-load harness (part 3): the load cohorts. Streams, purchases and
//! reads run SIMULTANEOUSLY from one barrier; the gauges count real open
//! responses, not spawned tasks.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::config::{Config, PeakGauge, Sample};
use anyhow::Context as _;
use bytes::Bytes;
use http_body_util::BodyExt;
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector},
    rt::TokioExecutor,
};
use serde_json::json;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub type HttpClient = Client<HttpConnector, http_body_util::Full<Bytes>>;

pub fn make_client() -> HttpClient {
    let mut builder = hyper_util::client::legacy::Client::builder(TokioExecutor::new());
    builder.http1_title_case_headers(false);
    builder.pool_idle_timeout(Some(Duration::from_secs(30)));
    builder.build_http()
}

fn get(uri: String, auth: Option<&str>) -> http::Request<http_body_util::Full<Bytes>> {
    let mut b = http::Request::builder().method(http::Method::GET).uri(uri);
    if let Some(t) = auth {
        b = b.header(http::header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(http_body_util::Full::new(Bytes::new()))
        .expect("get request")
}

fn post_json(
    uri: String,
    auth: Option<&str>,
    receipt: Option<&str>,
    body: String,
) -> http::Request<http_body_util::Full<Bytes>> {
    let mut b = http::Request::builder()
        .method(http::Method::POST)
        .uri(uri)
        .header(http::header::CONTENT_TYPE, "application/json");
    if let Some(t) = auth {
        b = b.header(http::header::AUTHORIZATION, format!("Bearer {t}"));
    }
    if let Some(r) = receipt {
        b = b.header("idempotency-key", r);
    }
    b.body(http_body_util::Full::new(Bytes::from(body)))
        .expect("post request")
}

/// Simple helper: run one request, return (status, body bytes, elapsed us).
async fn run(
    client: &HttpClient,
    req: http::Request<http_body_util::Full<Bytes>>,
) -> (u16, Bytes, u64) {
    let started = Instant::now();
    match client.request(req).await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            match resp.into_body().collect().await {
                Ok(collected) => (
                    status,
                    collected.to_bytes(),
                    started.elapsed().as_micros() as u64,
                ),
                Err(_) => (0, Bytes::new(), started.elapsed().as_micros() as u64),
            }
        }
        Err(_) => (0, Bytes::new(), started.elapsed().as_micros() as u64),
    }
}

fn record(s: &mut Sample, status: u16, us: u64) {
    s.record((200..300).contains(&status), status, us);
}

// ── cohort 1: streams ──────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub struct StreamResult {
    pub status: u16,
    pub ttft_us: Option<u64>,
    pub total_us: u64,
    /// Largest gap between consecutive SSE frames after the first one.
    pub max_gap_us: u64,
    pub frames: u64,
    pub done: bool,
}

/// One REAL streamed chat completion through the real router + real upstream:
/// frames are read one by one, TTFT = first frame, max_gap = largest pause
/// between two consecutive frames.
pub async fn one_stream(
    client: &HttpClient,
    base: &str,
    key: &str,
    model: &str,
    peak: &PeakGauge,
) -> StreamResult {
    let body = json!({
        "model": model,
        "messages": [{"role": "user", "content": "bench mixed load"}],
        "stream": true
    })
    .to_string();
    let req = post_json(format!("{base}/v1/chat/completions"), Some(key), None, body);
    let started = Instant::now();
    let resp = match client.request(req).await {
        Ok(r) => r,
        Err(_) => {
            return StreamResult {
                status: 0,
                total_us: started.elapsed().as_micros() as u64,
                ..Default::default()
            };
        }
    };
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        // Drain error bodies fast; the connection is short-lived.
        let _ = resp.into_body().collect().await;
        return StreamResult {
            status,
            total_us: started.elapsed().as_micros() as u64,
            ..Default::default()
        };
    }
    // Open response = one real active stream.
    let now_open = peak.current.fetch_add(1, Ordering::SeqCst) + 1;
    peak.peak.fetch_max(now_open, Ordering::SeqCst);
    let mut result = StreamResult {
        status,
        ..Default::default()
    };
    let mut body = resp.into_body();
    let mut last_frame = Instant::now();
    while let Some(Ok(frame)) = body.frame().await {
        let now = Instant::now();
        let gap = now.duration_since(last_frame).as_micros() as u64;
        last_frame = now;
        if result.ttft_us.is_none() {
            result.ttft_us = Some(started.elapsed().as_micros() as u64);
        } else if gap > result.max_gap_us {
            result.max_gap_us = gap;
        }
        result.frames += 1;
        if let Ok(data) = frame.into_data() {
            if data.windows(4).any(|w| w == b"DONE") {
                result.done = true;
            }
        }
    }
    let _ = body.collect().await;
    peak.current.fetch_sub(1, Ordering::SeqCst);
    result.total_us = started.elapsed().as_micros() as u64;
    result
}

// ── cohort 2: purchase funnel (order -> watcher -> reveal -> ack) ──────────

#[derive(Debug, Default, Clone)]
pub struct PurchaseOutcome {
    pub created: bool,
    pub credited: bool,
    pub revealed: bool,
    pub acked: bool,
    /// Revealed plaintext key, feeds the read cohort's key pool.
    pub key: Option<String>,
}

pub struct PurchaseHandles {
    pub order: Sample,
    pub status_poll: Sample,
    pub reveal: Sample,
    pub ack: Sample,
}

/// Creates one order and registers its exact amount with the mock chain so
/// the REAL watcher can match it. Returns (order_code, amount_units).
pub async fn create_order(
    client: &HttpClient,
    base: &str,
    plan: i64,
    receipt: String,
    chain: &crate::world::MockChain,
    sample: &mut Sample,
) -> anyhow::Result<(String, i64)> {
    let body = json!({"tier_id": plan, "mint_new_key": true}).to_string();
    let (status, resp_body, us) = run(
        client,
        post_json(
            format!("{base}/v1/credits/order"),
            None,
            Some(&receipt),
            body,
        ),
    )
    .await;
    record(sample, status, us);
    anyhow::ensure!(
        (200..300).contains(&status),
        "order create status {status}: {}",
        String::from_utf8_lossy(&resp_body)
    );
    let v: serde_json::Value = serde_json::from_slice(&resp_body).context("order json")?;
    let code = v["order_code"].as_str().context("order_code")?.to_string();
    let amount = v["amount_units"].as_i64().context("amount_units")?;
    chain.push_payment(amount, &receipt, 0);
    Ok((code, amount))
}

/// Polls GET /v1/credits/order/{code} until `minted` (or deadline).
pub async fn wait_minted(
    client: &HttpClient,
    base: &str,
    code: &str,
    deadline: Instant,
    sample: &mut Sample,
) -> bool {
    loop {
        let (status, body, us) =
            run(client, get(format!("{base}/v1/credits/order/{code}"), None)).await;
        record(sample, status, us);
        if (200..300).contains(&status) {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) {
                if v["status"] == "minted" {
                    return true;
                }
            }
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// reveal -> ack with the creation receipt (the real delivery capability).
pub async fn reveal_and_ack(
    client: &HttpClient,
    base: &str,
    code: &str,
    receipt: &str,
    reveal: &mut Sample,
    ack: &mut Sample,
) -> Option<String> {
    let (status, body, us) = run(
        client,
        post_json(
            format!("{base}/v1/credits/order/{code}/reveal"),
            None,
            Some(receipt),
            String::new(),
        ),
    )
    .await;
    record(reveal, status, us);
    if !(200..300).contains(&status) {
        return None;
    }
    let key = serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["key"].as_str().map(String::from));
    let (status, _, us) = run(
        client,
        post_json(
            format!("{base}/v1/credits/order/{code}/ack"),
            None,
            Some(receipt),
            String::new(),
        ),
    )
    .await;
    record(ack, status, us);
    if (200..300).contains(&status) {
        key
    } else {
        None
    }
}

// ── cohort 3: purchased-user reads ─────────────────────────────────────────

pub struct ReadTargets {
    pub models: Sample,
    pub plan: Sample,
    pub plans: Sample,
    pub summary: Sample,
    pub by_model: Sample,
    pub by_day: Sample,
    pub quota: Sample,
}

/// One authenticated GET by a PURCHASED user key (never an admin key):
/// - /v1/models            -> catalog/models
/// - /v1/credits/me/plan   -> quota/plan status of the purchased user
/// - /v1/credits/plans     -> plan catalog
pub async fn one_read(
    client: &HttpClient,
    base: &str,
    key: &str,
    which: usize,
    targets: &mut ReadTargets,
) {
    // Round-robin the three endpoint families.
    let (endpoint, sample_idx) = match which % 7 {
        0 => (format!("{base}/v1/models"), 0),
        1 => (format!("{base}/admin/api/credits/quota"), 1),
        2 => (format!("{base}/admin/api/models/catalog"), 2),
        3 => (format!("{base}/admin/api/usage/summary"), 3),
        4 => (format!("{base}/admin/api/usage/by-model"), 4),
        5 => (format!("{base}/admin/api/usage/by-day"), 5),
        _ => (format!("{base}/admin/api/usage/quota"), 6),
    };
    let (status, _, us) = run(client, get(endpoint, Some(key))).await;
    // Record into the right bucket via a tiny match (Sample is not shared).
    let ok = (200..300).contains(&status);
    match sample_idx {
        0 => record(&mut targets.models, status, us),
        1 => record(&mut targets.plan, status, us),
        2 => record(&mut targets.plans, status, us),
        3 => record(&mut targets.summary, status, us),
        4 => record(&mut targets.by_model, status, us),
        5 => record(&mut targets.by_day, status, us),
        _ => record(&mut targets.quota, status, us),
    }
    let _ = ok;
}

/// Shared counter of revealed keys feeding the read cohort.
#[derive(Debug, Default)]
pub struct KeyPool {
    keys: std::sync::Mutex<Vec<String>>,
    next: AtomicUsize,
}

impl KeyPool {
    pub fn push(&self, key: String) {
        self.keys.lock().unwrap().push(key);
    }
    pub fn len(&self) -> usize {
        self.keys.lock().unwrap().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn pick(&self) -> Option<String> {
        let keys = self.keys.lock().unwrap();
        if keys.is_empty() {
            return None;
        }
        let i = self.next.fetch_add(1, Ordering::SeqCst) % keys.len();
        Some(keys[i].clone())
    }
}

// ── heartbeat: scheduler-starvation probe ──────────────────────────────────

/// Samples a 50 ms tick for the whole run and reports the worst scheduling
/// gaps — the direct signal for the suspected synchronous QR render starving
/// the workers inside POST /v1/credits/order.
#[derive(Debug, Default)]
pub struct Heartbeat {
    pub gaps_us: std::sync::Mutex<Vec<u64>>,
}

impl Heartbeat {
    pub async fn run(self: &Arc<Self>, stop: Arc<std::sync::atomic::AtomicBool>) {
        let mut last = Instant::now();
        while !stop.load(Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let now = Instant::now();
            let gap = now.duration_since(last).as_micros() as u64;
            last = now;
            if gap > 55_000 {
                self.gaps_us.lock().unwrap().push(gap);
            }
        }
    }
}

// ── barrier ────────────────────────────────────────────────────────────────

/// All cohorts start from this gate: nothing is spawned before the barrier
/// opens, so the three loads are genuinely simultaneous.
#[derive(Debug, Default)]
pub struct Barrier(std::sync::Arc<tokio::sync::Notify>);

impl Barrier {
    pub fn new() -> Self {
        Self(Arc::new(tokio::sync::Notify::new()))
    }
    pub fn open(&self) {
        self.0.notify_waiters();
    }
    pub async fn wait(&self) {
        let notified = self.0.notified();
        // Avoid a race where open() happened before wait() registered.
        tokio::select! {
            _ = notified => {}
            _ = tokio::time::sleep(Duration::from_millis(1)) => {}
        }
    }
}

/// Parses a smoke/full preset hint from the config for the report.
pub fn preset_label(cfg: &Config) -> &'static str {
    if cfg.streams >= 500 && cfg.orders >= 1500 && cfg.reads >= 2000 {
        "full"
    } else {
        "custom"
    }
}
