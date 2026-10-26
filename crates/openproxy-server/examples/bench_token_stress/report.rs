use crate::{load::Batch, resources::ProcessReport};
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

#[derive(Serialize)]
pub struct Run {
    pub repetition: usize,
    pub requested: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub validated_tokens: u64,
    pub observed_tokens_in_failed_streams: u64,
    pub tokens_per_second: f64,
    pub elapsed_us: u64,
    pub headers_us: Option<Distribution>,
    pub ttft_us: Option<Distribution>,
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
        persistence: Value,
        profile: Value,
    ) -> Self {
        let valid: Vec<_> = batch
            .requests
            .iter()
            .filter(|r| r.error.is_none())
            .collect();
        let tokens: u64 = valid.iter().map(|r| r.stream.tokens).sum();
        Self {
            repetition,
            requested: batch.requests.len(),
            succeeded: valid.len(),
            failed: batch.requests.len() - valid.len(),
            validated_tokens: tokens,
            observed_tokens_in_failed_streams: batch
                .requests
                .iter()
                .filter(|r| r.error.is_some())
                .map(|r| r.stream.tokens)
                .sum(),
            tokens_per_second: tokens as f64 / (batch.elapsed_us as f64 / 1_000_000.0),
            elapsed_us: batch.elapsed_us,
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
        for (name, distribution) in [
            ("TTFT", &self.ttft_us),
            ("Latency", &self.latency_us),
            ("Headers", &self.headers_us),
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
    let mut failed = crate::load::RequestResult {
        id: 0,
        status: Some(429),
        error: Some("rejected".into()),
        headers_us: Some(1),
        ttft_us: None,
        total_us: 2,
        body_bytes: 0,
        http_frames: 0,
        stream: crate::validate::Validator::default(),
    };
    failed.stream.tokens = 999;
    let report = Run::new(
        1,
        Batch {
            elapsed_us: 1000,
            requests: vec![failed],
        },
        Vec::new(),
        Value::Null,
        Value::Null,
    );
    ensure!(
        report.validated_tokens == 0 && report.tokens_per_second == 0.0 && report.failed == 1,
        "failed streams inflated tokens"
    );
    Ok(())
}
