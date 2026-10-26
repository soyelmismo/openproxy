use anyhow::{Context, ensure};
use serde::{Deserialize, Serialize};
use std::{
    io::{BufRead, Write},
    path::PathBuf,
};

pub const HELP: &str = "bench_token_stress [CONCURRENCY=10] [TOKENS=5000] [TOKENS_PER_EVENT=5]
  --repetitions N       Fresh processes and SQLite directory per repetition (default 3)
  --keys N             Default ceil(concurrency/64). --keys 1 tests the admission cap
  --proxy-cpus LIST     taskset CPU list (default all allowed CPUs after the first two)
  --client-cpus LIST    taskset CPU list (default second allowed CPU)
  --mock-cpus LIST      taskset CPU list (default first allowed CPU)
  --workers N          Proxy Tokio workers (default number of proxy CPUs)
  --json PATH          Machine-readable report, '-' for JSON stdout
  --synchronous MODE   full (default, matches production) or normal (durability tradeoff)
  --perf               Hardware counters for proxy measurement window (Linux perf required)
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
}

pub struct Options {
    pub worker: WorkerConfig,
    pub repetitions: usize,
    pub proxy_cpus: String,
    pub client_cpus: String,
    pub mock_cpus: String,
    pub json: Option<String>,
    pub perf: bool,
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
            if arg == "--perf" || arg == "--heaptrack" {
                flags.insert(arg.as_str(), "true".to_owned());
            } else if arg.starts_with("--") {
                ensure!(
                    [
                        "--repetitions",
                        "--keys",
                        "--proxy-cpus",
                        "--client-cpus",
                        "--mock-cpus",
                        "--workers",
                        "--json",
                        "--synchronous",
                        "--output-dir"
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
            },
            repetitions,
            proxy_cpus,
            client_cpus,
            mock_cpus,
            json: flags.get("--json").cloned(),
            perf: flags.contains_key("--perf"),
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
