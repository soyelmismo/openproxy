use crate::{
    config::{Options, WorkerConfig},
    load::Batch,
    profile::Perf,
    report::{Distribution, Run},
    resources::{Sampler, ticks_per_second},
};
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

struct Worker {
    child: Child,
    input: ChildStdin,
    output: mpsc::Receiver<std::io::Result<String>>,
}
impl Worker {
    fn spawn(
        role: &str,
        cpus: &str,
        config: &WorkerConfig,
        heaptrack: bool,
    ) -> anyhow::Result<Self> {
        let mut command = Command::new("taskset");
        command.args(["-c", cpus]);
        if heaptrack {
            // Launch the preload library directly: no wrapper chatter on JSON stdout,
            // no wrapper-owned /tmp FIFO and no orphaned grandchildren on error.
            let library = std::env::var("HEAPTRACK_PRELOAD")
                .unwrap_or_else(|_| "/usr/lib/heaptrack/libheaptrack_preload.so".into());
            ensure!(
                Path::new(&library).is_file(),
                "set HEAPTRACK_PRELOAD to libheaptrack_preload.so"
            );
            command
                .arg("env")
                .arg(format!("LD_PRELOAD={library}"))
                .arg(format!(
                    "DUMP_HEAPTRACK_OUTPUT={}",
                    config.directory.join("heaptrack-proxy.raw").display()
                ));
        }
        command
            .arg(std::env::current_exe()?)
            .args(["--role", role])
            // Child-only environment, set before its Tokio threads exist.
            .env("OPENPROXY_ALLOW_PRIVATE_UPSTREAMS", "true")
            .env("MIMALLOC_DISALLOW_ARENA_ALLOC", "1")
            .env("MIMALLOC_PURGE_DELAY", "10")
            .env("MIMALLOC_ARENA_EAGER_COMMIT", "0")
            .env("MIMALLOC_EAGER_COMMIT", "0")
            .env("MIMALLOC_ARENA_RESERVE", "65536")
            .env("MIMALLOC_ABANDONED_PAGE_PURGE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(File::create(
                config.directory.join(format!("{role}.stderr")),
            )?));
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn {role}; taskset/heaptrack must be installed"))?;
        let input = child.stdin.take().context("worker stdin")?;
        let output = child.stdout.take().context("worker stdout")?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(output).lines() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut worker = Self {
            child,
            input,
            output: rx,
        };
        writeln!(worker.input, "{}", serde_json::to_string(config)?)?;
        worker.input.flush()?;
        Ok(worker)
    }
    fn read(&self) -> anyhow::Result<Value> {
        let line = self
            .output
            .recv_timeout(Duration::from_secs(150))
            .context("worker control timeout or exit; inspect role.stderr")??;
        serde_json::from_str(&line).with_context(|| format!("invalid worker output: {line}"))
    }
    fn send(&mut self, command: &str) -> anyhow::Result<()> {
        writeln!(self.input, "{command}")?;
        self.input.flush()?;
        Ok(())
    }
    fn finish(&mut self, command: &str) -> anyhow::Result<()> {
        self.send(command)?;
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = self.child.try_wait()? {
                ensure!(status.success(), "worker failed: {status}");
                return Ok(());
            }
            ensure!(Instant::now() < deadline, "worker did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn pid(value: &Value) -> anyhow::Result<u32> {
    Ok(value
        .get("pid")
        .and_then(Value::as_u64)
        .context("missing worker PID")?
        .try_into()?)
}

fn one_run(
    options: &Options,
    repetition: usize,
    directory: &Path,
    ticks: u64,
) -> anyhow::Result<Run> {
    let mut config = options.worker.clone();
    config.directory = directory.join(format!("run-{repetition}"));
    std::fs::create_dir_all(&config.directory)?;
    config.workers = 1;
    let mut mock = Worker::spawn("mock", &options.mock_cpus, &config, false)?;
    let mock_ready = mock.read()?;
    config.upstream = Some(mock_ready["url"].as_str().context("mock URL")?.to_owned());
    config.workers = options.worker.workers;
    let mut proxy = Worker::spawn("proxy", &options.proxy_cpus, &config, options.heaptrack)?;
    let proxy_ready = proxy.read()?;
    config.keys = serde_json::from_value(proxy_ready["keys"].clone())?;
    config.url = Some(proxy_ready["url"].as_str().context("proxy URL")?.to_owned());
    config.workers = 1;
    let mut client = Worker::spawn("client", &options.client_cpus, &config, false)?;
    let client_ready = client.read()?;
    ensure!(
        client_ready["ready"] == true,
        "client warmup did not complete"
    );
    proxy.send("prepare")?;
    ensure!(proxy.read()?["prepared"] == true, "proxy did not prepare");
    let perf = options
        .perf
        .then(|| Perf::start(pid(&proxy_ready)?, &config.directory))
        .transpose()?;
    let sampler = Sampler::start(vec![
        pid(&proxy_ready)?,
        pid(&mock_ready)?,
        pid(&client_ready)?,
    ])?;
    client.send("go")?;
    let batch: Batch = serde_json::from_value(client.read()?)?;
    let after_client = sampler.snapshot()?;
    proxy.send("finish")?;
    let persistence = proxy.read()?;
    ensure!(persistence["drained"] == true, "persistence did not drain");
    let resources = sampler.finish(after_client, ticks)?;
    let profile = if let Some(perf) = perf {
        perf.stop()?
    } else {
        json!({"status":"not instrumented; pass --perf for hardware cycles"})
    };
    proxy.finish("exit")?;
    client.finish("exit")?;
    mock.finish("exit")?;
    let mut profile = profile;
    if options.heaptrack {
        profile["allocations"] = crate::profile::interpret_heaptrack(&config.directory)?;
    }
    Ok(Run::new(repetition, batch, resources, persistence, profile))
}

pub fn run(options: Options) -> anyhow::Result<()> {
    std::fs::create_dir_all(&options.worker.directory)?;
    let directory = options
        .worker
        .directory
        .join(format!("openproxy-benchmark-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&directory)?;
    let ticks = ticks_per_second()?;
    eprintln!("Artifacts: {}", directory.display());
    eprintln!(
        "CPU affinity: proxy {} ({} workers), mock {}, client {}. Background host load is NOT isolated.",
        options.proxy_cpus, options.worker.workers, options.mock_cpus, options.client_cpus
    );
    let mut runs = Vec::new();
    for repetition in 1..=options.repetitions {
        let run = one_run(&options, repetition, &directory, ticks)?;
        run.print();
        runs.push(run);
    }
    let failed: usize = runs.iter().map(|r| r.failed).sum();
    let throughput = Distribution::from(runs.iter().map(|r| r.tokens_per_second).collect());
    let result = json!({
        "schema_version":2, "configuration":options.worker, "repetitions":options.repetitions,
        "affinity":{"proxy":options.proxy_cpus,"mock":options.mock_cpus,"client":options.client_cpus},
        "allocator":if cfg!(feature="benchmark-system-allocator") { "system (allocation profiling, do not compare RSS with mimalloc)" } else { "mimalloc (production allocator with child environment tuning)" },
        "sql_instrumented":cfg!(feature="benchmark-sql"), "clock_ticks_per_second":ticks,
        "notes":["Synthetic tokens: exact received ' tok' units, not tokenizer throughput", "RSS is residency, not allocations; sampled every 5ms; HWM includes warmup", "CPU ticks are scheduled CPU time, NOT hardware cycles", "SQL counts include background/persistence; no phase attribution", "AppState::for_test and axum::serve, not the production startup/hardened accept loop", "Loopback transport and disjoint affinity do not eliminate host noise", "Per-request latency includes scheduler, sockets, upstream and client work, not pure pipeline CPU"],
        "artifacts":directory, "throughput_across_repetitions":throughput,"failed_requests":failed,"runs":runs,
    });
    let text = serde_json::to_string_pretty(&result)?;
    std::fs::write(directory.join("report.json"), &text)?;
    if let Some(path) = &options.json {
        if path == "-" {
            println!("{text}");
        } else {
            std::fs::write(path, &text)?;
        }
    }
    if let Some(stats) = &throughput {
        eprintln!(
            "Throughput across runs: mean {:.0}, stddev {:.0} ({:.1}%), min {:.0}, max {:.0}",
            stats.mean,
            stats.standard_deviation,
            100.0 * stats.standard_deviation / stats.mean,
            stats.min,
            stats.max
        );
    }
    ensure!(
        failed == 0,
        "{failed} requests failed validation/admission; report written, tokens not inflated"
    );
    Ok(())
}
