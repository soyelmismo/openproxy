//! E2E mixed-load harness (part 1): configuration, pacing budgets and the
//! JSON report types. No load runs under `cargo test`; this is an example
//! binary executed manually with `cargo run`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use serde::Serialize;
use std::time::Duration;

/// Runtime knobs. Every count is overridable by flag or env var so the same
/// binary runs the tiny smoke preset and the full mixed-load scenario.
#[derive(Clone, Debug)]
pub struct Config {
    /// Overlapping SSE streams against the REAL router (real TCP loopback).
    pub streams: usize,
    /// Full purchase funnels: order -> watcher credit -> reveal -> ack.
    pub orders: usize,
    /// Authenticated GETs with purchased-user keys (models / me/plan / plans).
    pub reads: usize,
    /// Distinct client chat keys. Defaults to `ceil(streams / per_key_limit)`
    /// plus margin so the REAL per-key inflight cap (10) never 429s.
    pub keys: usize,
    /// Upstream SSE stream duration range (the mock paces each stream inside
    /// it, so the 500 streams genuinely overlap).
    pub pace_min: Duration,
    pub pace_max: Duration,
    /// Concurrency caps the app_state is explicitly retuned to. Bench uses the
    /// production defaults (512 global / 10 per key) and buys headroom with
    /// keys, never by raising production limits.
    pub per_key_limit: usize,
    pub global_limit: usize,
    /// Max simultaneous read tasks (0 = every read at once).
    pub read_concurrency: usize,
    /// Seed rows into the `usage` history table (realism for analytics reads).
    pub seed_usage: usize,
    /// Seed catalog models (realism for /v1/models).
    pub seed_catalog: usize,
    /// Tokio worker threads of THIS process (client + router + mocks).
    pub workers: usize,
    /// SQLite `synchronous` pragma: "full" (production) or "normal".
    pub synchronous: String,
    /// JSON report destination; "-" = stdout.
    pub json: String,
}

pub const HELP: &str = "bench_mixed_load [flags]
  --streams N            overlapping SSE streams (env MIX_STREAMS; default 20, full 500)
  --orders N             full purchase funnels (env MIX_ORDERS; default 20, full 1500)
  --reads N              purchased-user GETs (env MIX_READS; default 40, full 2000)
  --keys N               client keys (default ceil(streams/per_key_limit)+4)
  --pace-ms A B          upstream stream duration range ms (default 300 600; full 15000 30000)
  --read-concurrency N   max simultaneous reads, 0 = all at once (default 0)
  --seed-usage N         historical `usage` rows (default 0; full 100000)
  --seed-catalog N       catalog models (default 0; full 1000)
  --workers N            tokio worker threads (default 4)
  --synchronous MODE     sqlite full|normal (default full)
  --json PATH            report destination, '-' stdout (default '-')
  --full                 the documented full scenario in one flag
  --help";

impl Config {
    /// Flag > env > default. `full` flips the documented scenario in one shot.
    pub fn parse(args: &[String]) -> anyhow::Result<Self> {
        let flag = |name: &str| -> Option<String> {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1).cloned())
        };
        let num = |name: &str, env: &str, default: usize| -> anyhow::Result<usize> {
            if let Some(v) = flag(name) {
                return v.parse().map_err(|_| anyhow::anyhow!("invalid {name}"));
            }
            if let Ok(v) = std::env::var(env) {
                return v.parse().map_err(|_| anyhow::anyhow!("invalid {env}"));
            }
            Ok(default)
        };
        let full = args.iter().any(|a| a == "--full");
        let streams = num("--streams", "MIX_STREAMS", if full { 500 } else { 20 })?;
        let orders = num("--orders", "MIX_ORDERS", if full { 1500 } else { 20 })?;
        let reads = num("--reads", "MIX_READS", if full { 2000 } else { 40 })?;
        let pace = if let Some(a) = flag("--pace-ms") {
            let b = flag("--pace-ms")
                .and_then(|_| args.iter().position(|x| x == "--pace-ms"))
                .and_then(|i| args.get(i + 2).cloned());
            (
                a.parse::<u64>()?,
                b.as_deref()
                    .map(str::parse::<u64>)
                    .transpose()?
                    .unwrap_or(if full { 30_000 } else { 600 }),
            )
        } else if full {
            (15_000, 30_000)
        } else {
            (300, 600)
        };
        let per_key_limit = num("--per-key-limit", "MIX_PER_KEY_LIMIT", 10)?;
        let keys = num(
            "--keys",
            "MIX_KEYS",
            streams.div_ceil(per_key_limit.max(1)) + 4,
        )?;
        anyhow::ensure!(
            keys * per_key_limit >= streams,
            "need keys * per_key_limit >= streams ({}*{} < {}); the REAL 10/key \
             inflight cap 429s otherwise",
            keys,
            per_key_limit,
            streams
        );
        Ok(Self {
            streams,
            orders,
            reads,
            keys,
            pace_min: Duration::from_millis(pace.0),
            pace_max: Duration::from_millis(pace.1.max(pace.0)),
            per_key_limit,
            global_limit: num("--global-limit", "MIX_GLOBAL_LIMIT", 512)?,
            read_concurrency: num("--read-concurrency", "MIX_READ_CONCURRENCY", 0)?,
            seed_usage: num(
                "--seed-usage",
                "MIX_SEED_USAGE",
                if full { 100_000 } else { 0 },
            )?,
            seed_catalog: num(
                "--seed-catalog",
                "MIX_SEED_CATALOG",
                if full { 1000 } else { 0 },
            )?,
            workers: num("--workers", "MIX_WORKERS", 4)?,
            synchronous: flag("--synchronous").unwrap_or_else(|| "full".into()),
            json: flag("--json").unwrap_or_else(|| "-".into()),
        })
    }

    /// One uniform pacing budget for every SSE chunk of a stream.
    pub fn stream_pace(&self) -> Duration {
        self.pace_max
    }
}

// ── latency records ────────────────────────────────────────────────────────

#[derive(Debug, Default, Clone)]
pub struct Sample {
    pub ok: u64,
    pub failed: u64,
    /// status histogram (non-2xx buckets), "other" for transport errors.
    pub statuses: Vec<(u16, u64)>,
    pub lat_us: Vec<u64>,
}

impl Sample {
    pub fn record(&mut self, ok: bool, status: u16, us: u64) {
        if ok {
            self.ok += 1;
        } else {
            self.failed += 1;
        }
        if let Some(e) = self.statuses.iter_mut().find(|(s, _)| *s == status) {
            e.1 += 1;
        } else {
            self.statuses.push((status, 1));
        }
        self.lat_us.push(us);
    }
}

/// Per-endpoint latency block of the report.
#[derive(Debug, Default, Serialize)]
pub struct EndpointStats {
    pub endpoint: String,
    pub ok: u64,
    pub failed: u64,
    pub statuses: Vec<(u16, u64)>,
    pub p50_us: Option<u64>,
    pub p90_us: Option<u64>,
    pub p99_us: Option<u64>,
    pub max_us: Option<u64>,
}

pub fn percentile(sorted: &[u64], p: f64) -> Option<u64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((sorted.len() as f64 * p).ceil() as usize)
        .saturating_sub(1)
        .min(sorted.len() - 1);
    Some(sorted[idx])
}

impl EndpointStats {
    pub fn finish(endpoint: &str, mut s: Sample) -> Self {
        s.lat_us.sort_unstable();
        Self {
            endpoint: endpoint.into(),
            ok: s.ok,
            failed: s.failed,
            statuses: s.statuses,
            p50_us: percentile(&s.lat_us, 0.50),
            p90_us: percentile(&s.lat_us, 0.90),
            p99_us: percentile(&s.lat_us, 0.99),
            max_us: s.lat_us.last().copied(),
        }
    }
}

/// Stream cohort aggregate: TTFT, total duration, largest inter-chunk gap.
#[derive(Debug, Default, Serialize)]
pub struct StreamStats {
    pub started: u64,
    pub completed_ok: u64,
    pub failed: u64,
    pub statuses: Vec<(u16, u64)>,
    pub ttft_p50_us: Option<u64>,
    pub ttft_p99_us: Option<u64>,
    pub total_p50_us: Option<u64>,
    pub total_p99_us: Option<u64>,
    pub max_gap_p50_us: Option<u64>,
    pub max_gap_p99_us: Option<u64>,
    pub max_gap_max_us: Option<u64>,
}

/// Peak-concurrency tracker. This counts REAL open responses (a stream only
/// counts while its body is still streaming), never spawned tasks.
#[derive(Debug, Default)]
pub struct PeakGauge {
    pub current: std::sync::atomic::AtomicU64,
    pub peak: std::sync::atomic::AtomicU64,
}
