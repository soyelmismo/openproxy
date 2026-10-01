//! Isolated stdlib benchmark measuring SQLite PRAGMA synchronous = FULL vs NORMAL.
//!
//! Usage:
//!     cargo run --example bench_durability -- --mode full --txs 1000
//!     cargo run --example bench_durability -- --mode normal --txs 1000
//!     cargo run --example bench_durability -- --mode full --path /tmp/custom.db --txs 1000

use openproxy_db::DbPool;
use openproxy_types::config::SqliteSynchronous;
use std::env;
use std::path::PathBuf;
use std::time::Instant;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut mode = SqliteSynchronous::Full;
    let mut custom_path: Option<PathBuf> = None;
    let mut num_txs: usize = 1000;

    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--mode" | "-m" => {
                let val = args.next().ok_or("Missing value for --mode flag")?;
                mode = match val.to_ascii_lowercase().as_str() {
                    "normal" => SqliteSynchronous::Normal,
                    "full" => SqliteSynchronous::Full,
                    other => {
                        return Err(format!(
                            "Invalid --mode value '{other}'. Expected 'full' or 'normal'"
                        )
                        .into());
                    }
                };
            }
            "--path" | "-p" => {
                let val = args.next().ok_or("Missing value for --path flag")?;
                custom_path = Some(PathBuf::from(val));
            }
            "--txs" | "-n" => {
                let val = args.next().ok_or("Missing value for --txs flag")?;
                num_txs = val
                    .parse::<usize>()
                    .map_err(|e| format!("Invalid number for --txs: {e}"))?;
                if num_txs == 0 {
                    return Err("Number of transactions (--txs) must be greater than zero".into());
                }
            }
            "--help" | "-h" => {
                println!(
                    "Usage: bench_durability [--mode <full|normal>] [--path <path>] [--txs <count>]"
                );
                return Ok(());
            }
            unknown => {
                return Err(format!("Unknown argument: '{unknown}'").into());
            }
        }
    }

    if num_txs == 0 {
        return Err("Number of transactions (--txs) must be greater than zero".into());
    }

    // Isolate benchmark storage: use RAII TempDir if no custom path was provided.
    // If a custom path is specified, it must be a base directory or a non-pre-existing file path.
    let (_temp_guard, db_path) = match custom_path {
        Some(base_dir) if base_dir.is_dir() => {
            let pid = std::process::id();
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path = base_dir.join(format!("openproxy-bench-{pid}-{nanos}.db"));
            (None, path)
        }
        Some(file_path) => {
            if file_path.exists() {
                return Err(format!(
                    "Target path '{}' already exists. Benchmark refuses to touch pre-existing database files.",
                    file_path.display()
                )
                .into());
            }
            (None, file_path)
        }
        None => {
            let temp = openproxy_db::testing::TempDir::new("bench-durability")?;
            let path = temp.path().join("bench.db");
            (Some(temp), path)
        }
    };

    println!(
        "Benchmarking SQLite Durability: Mode={:?}, Path={}, Transactions={}",
        mode,
        db_path.display(),
        num_txs
    );

    let pool = DbPool::open_with_options(&db_path, 2, mode)?;
    {
        let conn = pool.writer();
        conn.execute(
            "CREATE TABLE IF NOT EXISTS bench_kv (id INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT NOT NULL)",
            [],
        )?;
    }

    let mut latencies_us = Vec::with_capacity(num_txs);
    let total_start = Instant::now();
    for j in 0..num_txs {
        let tx_start = Instant::now();
        let conn = pool.writer();
        conn.execute(
            "INSERT INTO bench_kv (payload) VALUES (?1)",
            [&format!("payload_entry_{j}")],
        )?;
        latencies_us.push(tx_start.elapsed().as_micros() as u64);
    }
    let total_elapsed = total_start.elapsed();

    // Ensure database pool is completely closed before any temp cleanup
    drop(pool);

    if custom_path.is_some() && db_path.exists() {
        let _ = std::fs::remove_file(&db_path);
    }

    latencies_us.sort_unstable();
    let p50_idx = ((num_txs as f64) * 0.50).floor() as usize;
    let p99_idx = ((num_txs as f64) * 0.99).min((num_txs - 1) as f64) as usize;
    let p50_us = latencies_us[p50_idx.min(num_txs - 1)];
    let p99_us = latencies_us[p99_idx];

    let sum_us: u64 = latencies_us.iter().sum();
    let avg_us = (sum_us as f64) / (num_txs as f64);
    let tx_per_sec = (num_txs as f64) / total_elapsed.as_secs_f64();

    println!("--- Benchmark Results ---");
    println!("Synchronous: {:?}", mode);
    println!("Total Time:  {:?}", total_elapsed);
    println!("Throughput:  {:.2} tx/sec", tx_per_sec);
    println!("Avg Latency: {:.2} µs/tx", avg_us);
    println!("p50 Latency: {} µs/tx", p50_us);
    println!("p99 Latency: {} µs/tx", p99_us);

    Ok(())
}
