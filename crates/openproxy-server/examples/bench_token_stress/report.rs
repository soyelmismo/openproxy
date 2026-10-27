use crate::{
    load::Batch,
    resources::{ProcessReport, TcpCounters},
};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Serialize)]
pub struct Distribution {
    pub count: usize,
    pub mean: f64,
    pub standard_deviation: f64,
    pub min: f64,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
    pub max: f64,
}
impl Distribution {
    pub fn from(mut values: Vec<f64>) -> Option<Self> {
        if values.is_empty() {
            return None;
        }
        values.sort_by(f64::total_cmp);
        let count = values.len();
        let mean = values.iter().sum::<f64>() / count as f64;
        let percentile =
            |p: usize| values[(count * p).div_ceil(100).saturating_sub(1).min(count - 1)];
        Some(Self {
            count,
            mean,
            standard_deviation: (values.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
                / count as f64)
                .sqrt(),
            min: values[0],
            p50: percentile(50),
            p95: percentile(95),
            p99: percentile(99),
            max: values[count - 1],
        })
    }
}

/// Two-sided Student-t 95% confidence interval for a mean of independent
/// repetitions. No interval from a single sample. For df > 30 use the df=30
/// critical value, a conservative bound rather than a normal approximation.
pub fn mean_ci95(values: &[f64]) -> Option<[f64; 2]> {
    const T: [f64; 30] = [
        12.706205, 4.302653, 3.182446, 2.776445, 2.570582, 2.446912, 2.364624, 2.306004, 2.262157,
        2.228139, 2.200985, 2.178813, 2.160369, 2.144787, 2.131450, 2.119905, 2.109816, 2.100922,
        2.093024, 2.085963, 2.079614, 2.073873, 2.068658, 2.063899, 2.059539, 2.055529, 2.051831,
        2.048407, 2.045230, 2.042272,
    ];
    if values.len() < 2 {
        return None;
    }
    let n = values.len();
    let mean = values.iter().sum::<f64>() / n as f64;
    let sample_variance = values.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1) as f64;
    let half_width = T[(n - 2).min(T.len() - 1)] * (sample_variance / n as f64).sqrt();
    Some([mean - half_width, mean + half_width])
}

#[derive(Serialize)]
pub struct PerRequest {
    pub cpu_us: f64,
    pub storage_write_bytes: f64,
    pub write_syscalls: f64,
    pub voluntary_context_switches: f64,
    pub involuntary_context_switches: f64,
    pub run_queue_wait_us: f64,
    /// Every statement the proxy ran, background and persistence included.
    pub sql_statements: Option<f64>,
    /// Explicit BEGIN statements per request, including failed/retried ones.
    /// Excludes implicit autocommit. Not a count of fsync syscalls.
    pub explicit_transaction_begins: Option<f64>,
    /// Real syscall entries, not inferred from SQLite BEGIN/COMMIT counts.
    /// None when perf tracepoints were not collected or not supported.
    pub fsync_syscalls: Option<f64>,
    pub fdatasync_syscalls: Option<f64>,
}

fn profiled_syscalls_per_request(profile: &Value, event: &str, completed: f64) -> Option<f64> {
    if completed <= 0.0 {
        return None;
    }
    profile
        .get("counters")?
        .get(event)?
        .get("value")?
        .as_f64()
        .filter(|count| count.is_finite() && *count >= 0.0)
        .map(|count| count / completed)
}

fn timeline_cv(windows: &[u64]) -> Option<f64> {
    let first = windows.iter().position(|&n| n > 0)?;
    let last = windows.iter().rposition(|&n| n > 0)?;
    let active = &windows[first..=last];
    let d = Distribution::from(active.iter().map(|&n| n as f64).collect())?;
    (d.mean > 0.0).then(|| d.standard_deviation / d.mean)
}

#[derive(Serialize)]
pub struct Run {
    pub repetition: usize,
    pub requested: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub cancelled_as_planned: usize,
    pub partial_tokens_in_cancelled_streams: u64,
    /// Arrival deadline to close, including client scheduling lag.
    pub scheduled_latency_us: Option<Distribution>,
    pub client_scheduling_lag_us: Option<Distribution>,
    pub validated_tokens: u64,
    pub observed_tokens_in_failed_streams: u64,
    pub tokens_per_second: f64,
    /// Tokens divided by the time until the last `[DONE]` reached a client.
    /// Excludes the post-stream tail (usage persistence, connection close).
    pub tokens_per_second_to_done: Option<f64>,
    /// Completed (closed) requests per second over the whole batch.
    pub requests_per_second: f64,
    pub elapsed_us: u64,
    /// Per stream: `[DONE]` received -> response fully closed.
    pub done_to_close_us: Option<Distribution>,
    pub headers_us: Option<Distribution>,
    pub ttft_us: Option<Distribution>,
    pub max_gap_us: Option<Distribution>,
    /// Per stream: largest gap between two token-carrying frames.
    pub max_token_gap_us: Option<Distribution>,
    /// Nearest-rank bounds over every token-to-token gap of the batch
    /// (power-of-two buckets: the true value is below the bound and at least
    /// half of it).
    pub token_gap_p50_upper_bound_us: Option<u64>,
    pub token_gap_p99_upper_bound_us: Option<u64>,
    pub token_gap_p999_upper_bound_us: Option<u64>,
    /// Coefficient of variation of tokens per 100 ms window, from the first to
    /// the last window that received tokens. Lower is steadier.
    pub timeline_tokens_per_window_cv: Option<f64>,
    /// Host-wide TCP counter delta for the measured window.
    pub tcp_host_delta: TcpCounters,
    /// Total proxy cost divided by completed requests (includes work on
    /// failed/cancelled requests and persistence, NOT causal attribution).
    pub proxy_per_request: Option<PerRequest>,
    pub http_frames_per_stream: Option<Distribution>,
    pub latency_us: Option<Distribution>,
    pub all_attempt_latency_us: Option<Distribution>,
    pub client_batch: Batch,
    pub resources_proxy_mock_client: Vec<ProcessReport>,
    pub persistence: Value,
    pub profile: Value,
}
impl Run {
    pub fn new(
        repetition: usize,
        batch: Batch,
        resources: Vec<ProcessReport>,
        tcp_host_delta: TcpCounters,
        persistence: Value,
        profile: Value,
    ) -> Self {
        let valid: Vec<_> = batch
            .requests
            .iter()
            .filter(|r| r.error.is_none() && !r.cancelled_as_planned)
            .collect();
        let tokens: u64 = valid.iter().map(|r| r.stream.tokens).sum();
        let gaps = &batch.token_gap_histogram_log2_us;
        let completed = valid.len() as f64;
        let proxy = resources.first().filter(|_| completed > 0.0);
        let proxy_per_request = proxy.map(|r| {
            let d = &r.delta_including_persistence;
            let sql = persistence
                .pointer("/sql/statements")
                .and_then(Value::as_f64);
            let begins = persistence
                .pointer("/sql/by_prepared_sql")
                .and_then(Value::as_object)
                .map(|m| {
                    m.iter()
                        .filter(|(k, _)| k.trim_start().to_ascii_uppercase().starts_with("BEGIN"))
                        .filter_map(|(_, v)| v.as_f64())
                        .sum::<f64>()
                });
            PerRequest {
                cpu_us: r.cpu_seconds * 1e6 / completed,
                storage_write_bytes: d.storage_write_bytes as f64 / completed,
                write_syscalls: d.write_syscalls as f64 / completed,
                voluntary_context_switches: d.voluntary_context_switches as f64 / completed,
                involuntary_context_switches: d.involuntary_context_switches as f64 / completed,
                run_queue_wait_us: d.run_queue_wait_ns as f64 / 1000.0 / completed,
                sql_statements: sql.map(|n| n / completed),
                explicit_transaction_begins: begins.map(|n| n / completed),
                fsync_syscalls: profiled_syscalls_per_request(
                    &profile,
                    "syscalls:sys_enter_fsync",
                    completed,
                ),
                fdatasync_syscalls: profiled_syscalls_per_request(
                    &profile,
                    "syscalls:sys_enter_fdatasync",
                    completed,
                ),
            }
        });
        Self {
            repetition,
            requested: batch.requests.len(),
            succeeded: valid.len(),
            failed: batch.requests.iter().filter(|r| r.error.is_some()).count(),
            cancelled_as_planned: batch
                .requests
                .iter()
                .filter(|r| r.cancelled_as_planned)
                .count(),
            partial_tokens_in_cancelled_streams: batch
                .requests
                .iter()
                .filter(|r| r.cancelled_as_planned)
                .map(|r| r.stream.tokens)
                .sum(),
            scheduled_latency_us: Distribution::from(
                valid
                    .iter()
                    .map(|r| (r.total_us + r.scheduling_lag_us) as f64)
                    .collect(),
            ),
            client_scheduling_lag_us: Distribution::from(
                batch
                    .requests
                    .iter()
                    .map(|r| r.scheduling_lag_us as f64)
                    .collect(),
            ),
            validated_tokens: tokens,
            observed_tokens_in_failed_streams: batch
                .requests
                .iter()
                .filter(|r| r.error.is_some())
                .map(|r| r.stream.tokens)
                .sum(),
            tokens_per_second: tokens as f64 / (batch.elapsed_us as f64 / 1_000_000.0),
            tokens_per_second_to_done: valid
                .iter()
                .map(|r| r.done_us.map(|d| r.start_offset_us + d))
                .collect::<Option<Vec<_>>>()
                .and_then(|ends| ends.into_iter().max())
                .filter(|&end| end > 0)
                .map(|end| tokens as f64 / (end as f64 / 1_000_000.0)),
            requests_per_second: valid.len() as f64 / (batch.elapsed_us as f64 / 1_000_000.0),
            elapsed_us: batch.elapsed_us,
            done_to_close_us: Distribution::from(
                valid
                    .iter()
                    .filter_map(|r| r.done_us.map(|d| r.total_us.saturating_sub(d) as f64))
                    .collect(),
            ),
            headers_us: Distribution::from(
                valid
                    .iter()
                    .filter_map(|r| r.headers_us)
                    .map(|t| t as f64)
                    .collect(),
            ),
            ttft_us: Distribution::from(
                valid
                    .iter()
                    .filter_map(|r| r.ttft_us)
                    .map(|t| t as f64)
                    .collect(),
            ),
            max_gap_us: Distribution::from(valid.iter().map(|r| r.max_gap_us as f64).collect()),
            max_token_gap_us: Distribution::from(
                valid.iter().map(|r| r.max_token_gap_us as f64).collect(),
            ),
            token_gap_p50_upper_bound_us: gaps.quantile_upper_bound_us(500),
            token_gap_p99_upper_bound_us: gaps.quantile_upper_bound_us(990),
            token_gap_p999_upper_bound_us: gaps.quantile_upper_bound_us(999),
            timeline_tokens_per_window_cv: timeline_cv(&batch.tokens_per_window),
            tcp_host_delta,
            proxy_per_request,
            http_frames_per_stream: Distribution::from(
                valid.iter().map(|r| r.http_frames as f64).collect(),
            ),
            latency_us: Distribution::from(valid.iter().map(|r| r.total_us as f64).collect()),
            all_attempt_latency_us: Distribution::from(
                batch.requests.iter().map(|r| r.total_us as f64).collect(),
            ),
            client_batch: batch,
            resources_proxy_mock_client: resources,
            persistence,
            profile,
        }
    }
    pub fn print(&self) {
        eprintln!(
            "run {}: {}/{} validated, {} failed; {} tokens; {:.0} synthetic tokens/s; {:.2} ms batch",
            self.repetition,
            self.succeeded,
            self.requested,
            self.failed,
            self.validated_tokens,
            self.tokens_per_second,
            self.elapsed_us as f64 / 1000.0
        );
        if let Some(to_done) = self.tokens_per_second_to_done {
            eprintln!(
                "  to last [DONE]: {to_done:.0} tokens/s; {:.1} completed requests/s",
                self.requests_per_second
            );
        }
        if self.cancelled_as_planned > 0 {
            eprintln!(
                "  deliberate disconnects: {}; partial tokens excluded: {}",
                self.cancelled_as_planned, self.partial_tokens_in_cancelled_streams
            );
        }
        for (name, distribution) in [
            ("TTFT", &self.ttft_us),
            ("[DONE]->close", &self.done_to_close_us),
            ("Max frame gap", &self.max_gap_us),
            ("Max token gap", &self.max_token_gap_us),
            ("HTTP frames/stream", &self.http_frames_per_stream),
            ("Latency", &self.latency_us),
            ("Headers", &self.headers_us),
            ("Scheduled latency", &self.scheduled_latency_us),
            ("Client scheduling lag", &self.client_scheduling_lag_us),
        ] {
            if let Some(d) = distribution {
                eprintln!(
                    "  {name} us: mean {:.0}; p50 {:.0}; p95 {:.0}; p99 {:.0}",
                    d.mean, d.p50, d.p95, d.p99
                );
            }
        }
        for (name, r) in ["proxy", "mock", "client"]
            .iter()
            .zip(&self.resources_proxy_mock_client)
        {
            let delta = &r.delta_including_persistence;
            eprintln!(
                "  {name}: CPU {:.3}s ({:.1}% of one core); RSS baseline {} KiB, peak sampled {} KiB, end {} KiB; storage write {} KiB",
                r.cpu_seconds,
                r.cpu_percent_of_one_core,
                r.baseline.rss_kib,
                r.peak_sampled_rss_kib,
                r.after_persistence.rss_kib,
                delta.storage_write_bytes / 1024
            );
        }
        if let (Some(p50), Some(p99), Some(p999)) = (
            self.token_gap_p50_upper_bound_us,
            self.token_gap_p99_upper_bound_us,
            self.token_gap_p999_upper_bound_us,
        ) {
            eprintln!(
                "  token-to-token gap us (log2 bucket bound): p50 <{p50}; p99 <{p99}; p99.9 <{p999}"
            );
        }
        if let Some(cv) = self.timeline_tokens_per_window_cv {
            eprintln!("  tokens per 100 ms window: CV {cv:.3}");
        }
        if let Some(p) = &self.proxy_per_request {
            eprintln!(
                "  proxy per request: CPU {:.0} us; storage write {:.0} B; write syscalls {:.0}; ctx switches vol {:.1} invol {:.1}; run-queue wait {:.0} us; SQL {:.1}; explicit BEGIN {:.3}",
                p.cpu_us,
                p.storage_write_bytes,
                p.write_syscalls,
                p.voluntary_context_switches,
                p.involuntary_context_switches,
                p.run_queue_wait_us,
                p.sql_statements.unwrap_or(f64::NAN),
                p.explicit_transaction_begins.unwrap_or(f64::NAN)
            );
            eprintln!(
                "  proxy real sync syscalls per completed request: fsync {:?}; fdatasync {:?} (None = not collected)",
                p.fsync_syscalls, p.fdatasync_syscalls
            );
        }
        let t = &self.tcp_host_delta;
        eprintln!(
            "  TCP host delta: listen overflows {}; listen drops {}; SYN retrans {}; retrans segs {}; resets {}",
            t.listen_overflows,
            t.listen_drops,
            t.syn_retransmits,
            t.retransmitted_segments,
            t.established_resets
        );
        if let Some(n) = self.persistence.pointer("/sql/statements") {
            eprintln!("  SQL statements incl. persistence/background: {n}");
        }
        for r in self
            .client_batch
            .requests
            .iter()
            .filter(|r| r.error.is_some())
            .take(5)
        {
            eprintln!("  request {} failed: {:?}", r.id, r.error);
        }
    }
}

pub fn self_test() -> anyhow::Result<()> {
    use anyhow::ensure;
    let distribution = Distribution::from((1..=100).map(f64::from).collect())
        .ok_or_else(|| anyhow::anyhow!("empty distribution"))?;
    ensure!(
        distribution.p50 == 50.0 && distribution.p95 == 95.0 && distribution.p99 == 99.0,
        "nearest-rank percentiles"
    );
    ensure!(
        Distribution::from(Vec::new()).is_none(),
        "empty distribution"
    );
    let profile = serde_json::json!({"counters":{
        "syscalls:sys_enter_fsync":{"value":382.0},
        "syscalls:sys_enter_fdatasync":{"value":0.0},
        "unsupported":{"value":null}
    }});
    ensure!(
        profiled_syscalls_per_request(&profile, "syscalls:sys_enter_fsync", 800.0) == Some(0.4775)
            && profiled_syscalls_per_request(&profile, "syscalls:sys_enter_fdatasync", 800.0)
                == Some(0.0)
            && profiled_syscalls_per_request(&profile, "unsupported", 800.0).is_none()
            && profiled_syscalls_per_request(&profile, "missing", 800.0).is_none()
            && profiled_syscalls_per_request(&profile, "syscalls:sys_enter_fsync", 0.0).is_none(),
        "fsync normalization distinguishes unsupported from measured zero"
    );
    let mut failed = crate::load::RequestResult {
        id: 0,
        status: Some(429),
        error: Some("rejected".into()),
        cancelled_as_planned: false,
        planned_start_offset_us: 0,
        scheduling_lag_us: 0,
        headers_us: Some(1),
        ttft_us: None,
        total_us: 2,
        body_bytes: 0,
        http_frames: 0,
        max_gap_us: 0,
        keepalive_frames: 0,
        max_gap_closed_by_keepalive: false,
        max_gap_end_us: 0,
        frames_before_max_gap: 0,
        last_token_us: None,
        done_us: None,
        start_offset_us: 0,
        max_token_gap_us: 0,
        stream: crate::validate::Validator::default(),
        token_gaps: Default::default(),
        timeline: Vec::new(),
    };
    failed.stream.tokens = 999;
    let report = Run::new(
        1,
        Batch {
            elapsed_us: 1000,
            requests: vec![failed],
            token_gap_histogram_log2_us: Default::default(),
            tokens_per_window: Vec::new(),
        },
        Vec::new(),
        TcpCounters::default(),
        Value::Null,
        Value::Null,
    );
    ensure!(
        report.validated_tokens == 0 && report.tokens_per_second == 0.0 && report.failed == 1,
        "failed streams inflated tokens"
    );
    ensure!(
        timeline_cv(&[0, 10, 10, 10, 0]) == Some(0.0),
        "timeline CV ignores idle edges"
    );
    ensure!(timeline_cv(&[0, 0]).is_none(), "empty timeline");
    ensure!(mean_ci95(&[1.0]).is_none(), "no CI from one sample");
    let ci = mean_ci95(&[1.0, 2.0, 3.0]).ok_or_else(|| anyhow::anyhow!("missing CI"))?;
    ensure!(
        (ci[0] + 0.48414).abs() < 0.0001 && (ci[1] - 4.48414).abs() < 0.0001,
        "Student-t CI"
    );
    Ok(())
}
