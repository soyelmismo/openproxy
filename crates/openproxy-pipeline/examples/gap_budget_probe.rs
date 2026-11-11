//! Ops probe: idle-chunk (gap) budgets for the top streaming models in the copy DB.
use openproxy_pipeline::LatencyBudget;
use openproxy_types::ids::ModelRowId;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/tmp/probe_check.db".into());
    let conn = std::sync::Arc::new(parking_lot::Mutex::new(
        rusqlite::Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?,
    ));
    let lb = std::sync::Arc::new(LatencyBudget::new());
    println!(
        "{:<14} {:<12} {:>8} {:>10}  budget",
        "row", "provider", "ceiling", "gaps"
    );
    for (row, provider, ceiling) in [
        (14394928i64, "tiprouter", 30_000u64),
        (9779734, "zai", 30_000),
        (12709950, "nvidia-nim", 30_000),
    ] {
        let gaps: i64 = conn.lock().query_row(
            "SELECT COUNT(*) FROM usage WHERE model_row_id=? AND max_chunk_gap_ms IS NOT NULL AND created_at >= datetime('now','-2 days')",
            [row], |r| r.get(0))?;
        let b = lb.idle_chunk_budget_ms(provider, ModelRowId(row), None, ceiling, Some(&conn));
        match b {
            Some(ms) => println!("{row:<14} {provider:<12} {ceiling:>8} {gaps:>10}  {ms}ms (cut)"),
            None => println!("{row:<14} {provider:<12} {ceiling:>8} {gaps:>10}  None — OFF"),
        }
    }
    Ok(())
}
