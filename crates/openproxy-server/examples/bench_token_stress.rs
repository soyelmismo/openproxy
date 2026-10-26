//! Optional multiprocess HTTP stress harness. No load runs under cargo test.
#[path = "bench_token_stress/config.rs"]
mod config;
#[path = "bench_token_stress/driver.rs"]
mod driver;
#[path = "bench_token_stress/load.rs"]
mod load;
#[path = "bench_token_stress/mock.rs"]
mod mock;
#[path = "bench_token_stress/profile.rs"]
mod profile;
#[path = "bench_token_stress/proxy.rs"]
mod proxy;
#[path = "bench_token_stress/report.rs"]
mod report;
#[path = "bench_token_stress/resources.rs"]
mod resources;
#[path = "bench_token_stress/validate.rs"]
mod validate;

// Match the production allocator, except when explicitly profiling with heaptrack.
#[cfg(not(feature = "benchmark-system-allocator"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--role") {
        let role = args
            .get(1)
            .ok_or_else(|| anyhow::anyhow!("missing worker role"))?;
        let config: config::WorkerConfig = serde_json::from_str(&config::read_line()?)?;
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(config.workers)
            .build()?;
        return runtime.block_on(async {
            match role.as_str() {
                "mock" => mock::run(config).await,
                "proxy" => proxy::run(config).await,
                "client" => load::run(config).await,
                _ => anyhow::bail!("unknown worker role {role}"),
            }
        });
    }
    if args.first().is_some_and(|a| a == "--self-test") {
        validate::self_test()?;
        report::self_test()?;
        resources::self_test()?;
        println!(
            "benchmark self-tests: passed (fragmentation, corruption, usage, terminal, percentiles, proc parser)"
        );
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", config::HELP);
        return Ok(());
    }
    driver::run(config::Options::parse(&args)?)
}
