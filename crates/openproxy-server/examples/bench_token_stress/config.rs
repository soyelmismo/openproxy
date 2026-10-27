use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, Write},
    path::PathBuf,
};

pub const HELP: &str = "bench_token_stress [CONCURRENCY=10] [TOKENS=5000] [TOKENS_PER_EVENT=5]
  --repetitions N       Fresh processes and SQLite directory per repetition (default 3)
  --rounds N            Rounds of requests reusing the same HTTP client and server (default 1)
  --keys N             Default ceil(concurrency/64). --keys 1 tests the admission cap
  --proxy-cpus LIST     taskset CPU list (default all allowed CPUs after the first two)
  --client-cpus LIST    taskset CPU list (default second allowed CPU)
  --mock-cpus LIST      taskset CPU list (default first allowed CPU)
  --workers N          Proxy Tokio workers (default number of proxy CPUs)
  --json PATH          Machine-readable report, '-' for JSON stdout
  --synchronous MODE   full (default, matches production) or normal (durability tradeoff)
  --event-interval-us N Mock pause between upstream events per stream (default 0 = firehose)
  --arrival-rate RPS   Open-loop request rate, 0 = simultaneous batch (default)
  --read-delay-ms N    Pause before each client body frame (default 0)
  --cancel-after-tokens N  Deliberately disconnect after N observed tokens (default 0 = off)
  --cancel-percent N   Percentage of clients that disconnect (default 100 when enabled)
  --network-delay-ms N One-way delay per direction via ephemeral loopback TCP relay (default 0)
  --upstream-cut-after-events N Cut upstream stream after N events with error (default 0 = off)
  --upstream-cut-percent P      Percentage of upstream streams to cut (default 100 when enabled)
  --background         Include unrelated discovery/maintenance daemons (default isolated pipeline)
  --recording          Persist request/response bodies and headers (default off)
  --upstream-tls       Serve the mock upstream over HTTPS with ALPN h2 (bench only; reads certs from BENCH_TLS_DIR)
  --perf               Hardware counters for proxy measurement window (Linux perf required)
  --perf-fsync         Also count fsync/fdatasync tracepoints (implies --perf, Linux privileges required)
  --heaptrack          Profile proxy allocations (requires heaptrack and --features benchmark-system-allocator)
  --output-dir PATH    Artifacts parent (default TMPDIR, fresh run directory)
  --self-test          Optional correctness tests, no load, not run by cargo test
  --help               This message

All three roles are separate processes over real loopback HTTP. Affinity is not CPU isolation.
Tokens are synthetic ' tok' units validated from received text, NOT tokenizer throughput.
A failed request makes the run exit nonzero, but its report is still written.";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerConfig {
    pub concurrency: usize,
    pub tokens: usize,
    pub chunk: usize,
    pub keys_count: usize,
    pub workers: usize,
    pub upstream: Option<String>,
    pub url: Option<String>,
    pub keys: Vec<String>,
    pub directory: PathBuf,
    pub synchronous: String,
    #[serde(default = "default_rounds")]
    pub rounds: usize,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub recording: bool,
    #[serde(default)]
    pub upstream_tls: bool,
    #[serde(default)]
    pub event_interval_us: u64,
    #[serde(default)]
    pub arrival_rate_rps: u64,
    #[serde(default)]
    pub read_delay_ms: u64,
    #[serde(default)]
    pub cancel_after_tokens: usize,
    #[serde(default = "all_clients")]
    pub cancel_percent: u64,
    #[serde(default)]
    pub network_delay_ms: u64,
    #[serde(default)]
    pub upstream_cut_after_events: usize,
    #[serde(default = "all_clients")]
    pub upstream_cut_percent: u64,
}

fn default_rounds() -> usize {
    1
}

fn all_clients() -> u64 {
    100
}

pub struct Options {
    pub worker: WorkerConfig,
    pub repetitions: usize,
    pub proxy_cpus: String,
    pub client_cpus: String,
    pub mock_cpus: String,
    pub json: Option<String>,
    pub perf: bool,
    pub perf_fsync: bool,
    pub heaptrack: bool,
}

impl Options {
    pub fn parse(args: &[String]) -> anyhow::Result<Self> {
        let allowed = crate::resources::allowed_cpus()?;
        ensure!(
            allowed.len() >= 3,
            "need at least 3 allowed CPUs for disjoint roles"
        );
        let mut positional = Vec::new();
        let mut flags = std::collections::BTreeMap::new();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            if matches!(
                arg.as_str(),
                "--perf"
                    | "--perf-fsync"
                    | "--heaptrack"
                    | "--background"
                    | "--recording"
                    | "--upstream-tls"
            ) {
                flags.insert(arg.as_str(), "true".to_owned());
            } else if arg.starts_with("--") {
                ensure!(
                    [
                        "--repetitions",
                        "--rounds",
                        "--keys",
                        "--proxy-cpus",
                        "--client-cpus",
                        "--mock-cpus",
                        "--workers",
                        "--json",
                        "--synchronous",
                        "--output-dir",
                        "--event-interval-us",
                        "--arrival-rate",
                        "--read-delay-ms",
                        "--cancel-after-tokens",
                        "--cancel-percent",
                        "--network-delay-ms",
                        "--upstream-cut-after-events",
                        "--upstream-cut-percent"
                    ]
                    .contains(&arg.as_str()),
                    "unknown flag {arg}"
                );
                flags.insert(
                    arg.as_str(),
                    iter.next().context("missing flag value")?.clone(),
                );
            } else {
                positional.push(arg.parse::<usize>().context("invalid positional number")?);
            }
        }
        ensure!(
            positional.len() <= 3,
            "expected at most 3 positional arguments"
        );
        let concurrency = positional.first().copied().unwrap_or(10);
        let tokens = positional.get(1).copied().unwrap_or(5000);
        let chunk = positional.get(2).copied().unwrap_or(5);
        ensure!(
            (1..=900).contains(&concurrency),
            "concurrency must be 1..=900 (connection cap 1024)"
        );
        ensure!(
            (1..=10_000_000).contains(&tokens),
            "tokens must be 1..=10000000"
        );
        ensure!(
            (1..=10_000).contains(&chunk),
            "chunk size must be 1..=10000"
        );
        let proxy_cpus = flags.get("--proxy-cpus").cloned().unwrap_or_else(|| {
            allowed
                .iter()
                .skip(2)
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        });
        let client_cpus = flags
            .get("--client-cpus")
            .cloned()
            .unwrap_or_else(|| allowed[1].to_string());
        let mock_cpus = flags
            .get("--mock-cpus")
            .cloned()
            .unwrap_or_else(|| allowed[0].to_string());
        let sets = [&proxy_cpus, &client_cpus, &mock_cpus]
            .map(|s| crate::resources::parse_cpus(s))
            .into_iter()
            .collect::<anyhow::Result<Vec<_>>>()?;
        for (i, set) in sets.iter().enumerate() {
            ensure!(
                !set.is_empty() && set.iter().all(|cpu| allowed.contains(cpu)),
                "CPU list outside process affinity"
            );
            ensure!(
                sets.iter()
                    .skip(i + 1)
                    .all(|other| !set.iter().any(|c| other.contains(c))),
                "role CPU lists must not overlap"
            );
        }
        let number = |name: &str, default: usize| -> anyhow::Result<usize> {
            flags.get(name).map_or(Ok(default), |v| {
                v.parse().with_context(|| format!("invalid {name}"))
            })
        };
        let repetitions = number("--repetitions", 3)?;
        let keys_count = number("--keys", concurrency.div_ceil(64))?;
        let workers = number("--workers", sets[0].len())?;
        ensure!(
            (1..=100).contains(&repetitions)
                && (1..=concurrency).contains(&keys_count)
                && (1..=32).contains(&workers),
            "invalid repetitions, keys or workers"
        );
        let synchronous = flags
            .get("--synchronous")
            .cloned()
            .unwrap_or_else(|| "full".into());
        ensure!(
            synchronous == "full" || synchronous == "normal",
            "synchronous must be full or normal"
        );
        let heaptrack = flags.contains_key("--heaptrack");
        ensure!(
            !heaptrack || cfg!(feature = "benchmark-system-allocator"),
            "heaptrack requires --features benchmark-system-allocator"
        );
        let arrival_rate_rps = number("--arrival-rate", 0)? as u64;
        let read_delay_ms = number("--read-delay-ms", 0)? as u64;
        let cancel_after_tokens = number("--cancel-after-tokens", 0)?;
        let cancel_percent = number("--cancel-percent", 100)? as u64;
        let rounds = number("--rounds", 1)?;
        let network_delay_ms = number("--network-delay-ms", 0)? as u64;
        let upstream_cut_after_events = number("--upstream-cut-after-events", 0)?;
        let upstream_cut_percent = number("--upstream-cut-percent", 100)? as u64;
        ensure!(
            (1..=100_000).contains(&rounds),
            "rounds must be between 1 and 100000"
        );
        ensure!(
            network_delay_ms <= 10_000,
            "network delay must be <= 10000 ms"
        );
        ensure!(
            upstream_cut_percent <= 100,
            "upstream-cut-percent must be 0..=100"
        );
        ensure!(
            arrival_rate_rps <= 1_000_000 && read_delay_ms <= 1000,
            "invalid arrival rate or read delay"
        );
        ensure!(
            cancel_percent <= 100 && cancel_after_tokens < tokens,
            "cancel threshold must be below tokens; percent must be 0..=100"
        );
        ensure!(
            arrival_rate_rps == 0 || concurrency as u64 * 1000 / arrival_rate_rps < 100_000,
            "open-loop schedule must fit within 100 s"
        );
        let directory = flags
            .get("--output-dir")
            .map_or_else(std::env::temp_dir, PathBuf::from);
        Ok(Self {
            worker: WorkerConfig {
                concurrency,
                tokens,
                chunk,
                keys_count,
                workers,
                upstream: None,
                url: None,
                keys: Vec::new(),
                directory,
                synchronous,
                rounds,
                arrival_rate_rps,
                read_delay_ms,
                cancel_after_tokens,
                cancel_percent,
                network_delay_ms,
                upstream_cut_after_events,
                upstream_cut_percent,
                background: flags.contains_key("--background"),
                recording: flags.contains_key("--recording"),
                upstream_tls: flags.contains_key("--upstream-tls"),
                event_interval_us: flags
                    .get("--event-interval-us")
                    .map_or(Ok(0), |v| v.parse())
                    .context("invalid --event-interval-us")?,
            },
            repetitions,
            proxy_cpus,
            client_cpus,
            mock_cpus,
            json: flags.get("--json").cloned(),
            perf: flags.contains_key("--perf") || flags.contains_key("--perf-fsync"),
            perf_fsync: flags.contains_key("--perf-fsync"),
            heaptrack,
        })
    }
}

pub fn read_line() -> anyhow::Result<String> {
    let mut line = String::new();
    ensure!(
        std::io::stdin().lock().read_line(&mut line)? > 0,
        "control channel closed"
    );
    Ok(line.trim().to_owned())
}

pub fn emit(value: &impl Serialize) -> anyhow::Result<()> {
    serde_json::to_writer(std::io::stdout().lock(), value)?;
    println!();
    std::io::stdout().flush()?;
    Ok(())
}
