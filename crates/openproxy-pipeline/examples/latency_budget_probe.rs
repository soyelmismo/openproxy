//! Ops probe: runs the production `LatencyBudget` logic against a *copy* of a
//! live database to answer "why is the predictor not cutting this target?".
//!
//! Usage:
//!   cargo run -p openproxy-pipeline --example latency_budget_probe -- /path/to/data.db
//!
//! Read-only against the copy; never touches the live DB.

use std::time::Duration;

use openproxy_pipeline::LatencyBudget;
use openproxy_types::ids::ModelRowId;

/// (provider, model_row_id, resolved ceiling ms, est prompt tokens) — pairs
/// observed cutting at their resolved ceiling in production.
const CASES: &[(&str, i64, u64, u32)] = &[
    ("wusrouter", 14874931, 15_000, 8_000),
    ("wusrouter", 14874931, 15_000, 100_000),
    ("xkiro", 12524389, 20_000, 8_000),
    ("gwarden", 13131365, 20_000, 8_000),
    ("zai", 9779734, 20_000, 8_000),
    ("stepfun", 9874199, 60_000, 8_000),
    ("tiprouter", 14394928, 60_000, 8_000),
    ("antseed", 15172980, 60_000, 8_000),
];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/probe_budget.db".into());
    let conn =
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let conn = parking_lot::Mutex::new(conn);
    let budget = std::sync::Arc::new(LatencyBudget::new());

    println!("provider           row     est  ceiling  budget");
    for (provider, row, ceiling, est) in CASES {
        let b = budget.ttft_budget_ms(
            provider,
            ModelRowId(*row),
            None,
            *est,
            *ceiling,
            Some(&conn),
        );
        match b {
            Some(ms) => println!(
                "{provider:<12} {row:>9} {est:>7} {ceiling:>8}  {ms}ms (cut at {ms}, ceiling {ceiling})"
            ),
            None => println!("{provider:<12} {row:>9} {est:>7} {ceiling:>8}  None — predictor OFF"),
        }
    }

    // Session level with a hash that has no rows: must fall back to global.
    let fb = budget.ttft_budget_ms(
        "wusrouter",
        ModelRowId(14874931),
        Some(0xDEADBEEF),
        8_000,
        15_000,
        Some(&conn),
    );
    println!("\nsession fallback (unknown hash): {fb:?}");
    let _ = Duration::from_millis(0);
    Ok(())
}
