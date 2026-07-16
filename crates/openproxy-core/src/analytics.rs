//! Analytics: latency percentiles (t-digest) and race statistics.
//!
//! See docs/mvp-spec.md §7 (Analytics Queries). This module backs the
//! `/admin/usage/latency` and `/admin/usage/races` admin endpoints.
//!
//! Two queries live here:
//!
//! * [`latency_percentiles`] — streams `connect_ms`, `ttft_ms`, `total_ms`,
//!   and `tokens_per_sec` for race winners (`race_lost = 0`) matching the
//!   filter, feeds them into per-metric t-digests, and returns p50/p95.
//! * [`race_stats`] — streams rows where `race_total > 1` matching the filter
//!   and aggregates totals, average winner position, and per-target wins in
//!   Rust.

use crate::error::{CoreError, Result};
use crate::usage::UsageFilter;
use rusqlite::{Connection, ToSql, params_from_iter};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use tdigest::TDigest;

/// Number of centroids per t-digest. Spec §7 prescribes 200.
const TDIGEST_CENTROIDS: usize = 200;

/// Batch size for accumulating samples before merging into the running
/// t-digest. We buffer raw `f64` values and call `merge_unsorted` once per
/// batch; this keeps the per-row merge cost amortized to O(max_size) instead
/// of O(max_size) per row.
const MERGE_BATCH: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatencyPercentiles {
    pub samples: u64,
    pub p50_connect_ms: Option<f64>,
    pub p95_connect_ms: Option<f64>,
    pub p50_ttft_ms: Option<f64>,
    pub p95_ttft_ms: Option<f64>,
    pub p50_total_ms: Option<f64>,
    pub p95_total_ms: Option<f64>,
    pub p50_tokens_per_sec: Option<f64>,
    pub p95_tokens_per_sec: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RaceStats {
    /// `COUNT(DISTINCT request_id)` over rows where `race_total > 1`.
    pub total_races: u64,
    /// `COUNT(*)` of winners (`race_lost = 0`) in races. Equals `total_races`
    /// when every race produced exactly one winner; can diverge if a race
    /// ended with all losers or with multiple winners.
    pub winners: u64,
    /// `COUNT(*)` of losers (`race_lost = 1`).
    pub losers: u64,
    /// Average `priority_order` of the winning target across races. `None`
    /// when no race winner has a resolvable `combo_target_id`. Lower is
    /// better; a value of `1.0` means the first target always wins.
    pub avg_winner_position: Option<f64>,
    /// `None` for MVP. The spec reserves this for a future metric that
    /// subtracts the winner's `ttft` from the first target's `ttft`; we do
    /// not yet persist the per-target first-byte data needed to compute it.
    pub avg_ttft_savings_ms: Option<f64>,
    /// `(combo_target_id, win_count)` ordered by `win_count` DESC, then by
    /// `combo_target_id` ASC for stable output.
    pub wins_by_target: Vec<(i64, u64)>,
}

// ---------------------------------------------------------------------------
// WHERE-clause builder
// ---------------------------------------------------------------------------
//
// We re-implement the builder locally rather than reaching into the private
// helpers of `crate::usage`. The shape of the filter is the same; we just
// want a `(where_sql, params)` tuple we can splice into our own SELECTs.

struct BuiltWhere {
    sql: String,
    params: Vec<Box<dyn ToSql>>,
}

impl BuiltWhere {
    fn from_filter(f: &UsageFilter) -> Self {
        let mut clauses: Vec<&'static str> = Vec::new();
        let mut params: Vec<Box<dyn ToSql>> = Vec::new();

        if let Some(from) = &f.from {
            clauses.push("created_at >= ?");
            params.push(Box::new(from.clone()));
        }
        if let Some(to) = &f.to {
            clauses.push("created_at < ?");
            params.push(Box::new(to.clone()));
        }
        if let Some(pid) = &f.provider_id {
            clauses.push("provider_id = ?");
            params.push(Box::new(pid.0.clone()));
        }
        if let Some(mid) = &f.model_id {
            clauses.push("upstream_model_id = ?");
            params.push(Box::new(mid.clone()));
        }
        if let Some(aid) = f.account_id {
            clauses.push("account_id = ?");
            params.push(Box::new(aid.0));
        }
        if let Some(cid) = f.combo_id {
            clauses.push("combo_id = ?");
            params.push(Box::new(cid.0));
        }

        if clauses.is_empty() {
            return Self {
                sql: String::new(),
                params,
            };
        }
        let joined = clauses.join(" AND ");
        let mut sql = String::with_capacity(joined.len() + 7);
        sql.push_str("WHERE ");
        sql.push_str(&joined);
        Self { sql, params }
    }
}

fn to_params(v: &[Box<dyn ToSql>]) -> Vec<&dyn ToSql> {
    v.iter().map(|b| b.as_ref() as &dyn ToSql).collect()
}

// ---------------------------------------------------------------------------
// T-digest plumbing
// ---------------------------------------------------------------------------

/// Streaming accumulator that buffers samples in a `Vec` and merges into the
/// running t-digest every [`MERGE_BATCH`] samples. Exposes a final digest
/// that can be queried for percentiles.
struct StreamingDigest {
    digest: TDigest,
    buffer: Vec<f64>,
    /// Count of samples accepted, including those already merged.
    count: u64,
}

impl StreamingDigest {
    fn new() -> Self {
        Self {
            digest: TDigest::new_with_size(TDIGEST_CENTROIDS),
            buffer: Vec::with_capacity(MERGE_BATCH),
            count: 0,
        }
    }

    fn push(&mut self, value: f64) {
        // Track every accepted sample, even those still sitting in the
        // buffer, so `count` reflects the total we will eventually query.
        self.count += 1;
        self.buffer.push(value);
        if self.buffer.len() >= MERGE_BATCH {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.buffer.is_empty() {
            return;
        }
        let batch = std::mem::take(&mut self.buffer);
        // `merge_unsorted` clones the centroids it needs; with a 1024-element
        // batch and a 200-centroid digest, the cost stays bounded.
        self.digest = self.digest.merge_unsorted(batch);
        self.buffer.reserve(MERGE_BATCH);
    }

    fn quantile(&mut self, q: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        // Ensure any in-flight buffered values are merged before reading.
        self.flush();
        Some(self.digest.estimate_quantile(q))
    }
}

// ---------------------------------------------------------------------------
// latency_percentiles
// ---------------------------------------------------------------------------

/// Compute p50 and p95 latency percentiles for the four timing metrics.
///
/// Only race winners (`race_lost = 0`) contribute, per spec §7. Each metric
/// is independently fed into a 200-centroid t-digest and percentiles are
/// extracted at the end. A metric that has zero non-null samples reports
/// `None` for its percentiles.
pub fn latency_percentiles(conn: &Connection, f: &UsageFilter) -> Result<LatencyPercentiles> {
    let w = BuiltWhere::from_filter(f);

    // The base `w.sql` is "WHERE ..." or empty. We always add predicates
    // that restrict to successful, non-race-lost rows:
    //   - `race_lost = 0`: exclude race losers (workers that errored
    //     because another worker won the race).
    //   - `status_code < 400`: exclude error and timeout rows. Timeouts
    //     (502), client disconnects (499), rate limits (429), and any
    //     other non-2xx response have latency numbers that would skew
    //     the percentiles. Only successful responses should count.
    //
    // We start from the filter clauses (if any), strip the leading "WHERE "
    // and rebuild a fully-qualified WHERE that ANDs the filter with our
    // fixed predicates.
    let mut clauses: Vec<String> = Vec::new();
    if !w.sql.is_empty() {
        let bare = w.sql.trim_start_matches("WHERE ").to_string();
        clauses.push(format!("({})", bare));
    }
    clauses.push("race_lost = 0".to_string());
    clauses.push("status_code < 400".to_string());

    let where_clause = format!("WHERE {}", clauses.join(" AND "));

    let mut sql = String::new();
    write!(
        &mut sql,
        "SELECT connect_ms, ttft_ms, total_ms, tokens_per_sec \
         FROM usage {}",
        where_clause,
    )
    .expect("writing to String never fails");

    let mut stmt = conn.prepare(&sql).map_err(crate::error::map_db_error)?;

    let params_slice = to_params(&w.params);

    let mut connect = StreamingDigest::new();
    let mut ttft = StreamingDigest::new();
    let mut total = StreamingDigest::new();
    let mut tps = StreamingDigest::new();
    let mut rows_seen: u64 = 0;

    let mut rows = stmt
        .query(params_from_iter(params_slice))
        .map_err(crate::error::map_db_error)?;

    while let Some(row) = rows.next().map_err(crate::error::map_db_error)? {
        rows_seen += 1;
        // Each metric is independently nullable; we simply skip nulls per
        // column. We never abort on nulls because the schema allows them
        // (e.g. `ttft_ms` is NULL when the request failed before the first
        // byte, `tokens_per_sec` is NULL when C3's guard fires).
        if let Some(v) = row
            .get::<_, Option<i64>>(0)
            .map_err(|e| map_row_err(e, "connect_ms"))?
        {
            connect.push(v as f64);
        }
        if let Some(v) = row
            .get::<_, Option<i64>>(1)
            .map_err(|e| map_row_err(e, "ttft_ms"))?
        {
            ttft.push(v as f64);
        }
        if let Some(v) = row
            .get::<_, Option<i64>>(2)
            .map_err(|e| map_row_err(e, "total_ms"))?
        {
            total.push(v as f64);
        }
        if let Some(v) = row
            .get::<_, Option<f64>>(3)
            .map_err(|e| map_row_err(e, "tokens_per_sec"))?
        {
            tps.push(v);
        }
    }

    // `samples` is the count of raw rows that matched the WHERE clause (i.e.
    // distinct `usage.id` rows, not the deduped count of timing samples). It
    // tells the caller "this many winner rows were scanned"; the per-metric
    // p50/p95 may be derived from a smaller sample set when nulls are
    // present. The WHERE clause restricts to race_lost=0 AND status_code<400
    // so only successful non-race-lost rows are counted.
    Ok(LatencyPercentiles {
        samples: rows_seen,
        p50_connect_ms: connect.quantile(0.50),
        p95_connect_ms: connect.quantile(0.95),
        p50_ttft_ms: ttft.quantile(0.50),
        p95_ttft_ms: ttft.quantile(0.95),
        p50_total_ms: total.quantile(0.50),
        p95_total_ms: total.quantile(0.95),
        p50_tokens_per_sec: tps.quantile(0.50),
        p95_tokens_per_sec: tps.quantile(0.95),
    })
}

fn map_row_err(e: rusqlite::Error, column: &'static str) -> CoreError {
    CoreError::Database {
        message: format!("read latency_percentiles column {}: {}", column, e),
        source: Some(Box::new(e)),
    }
}

// ---------------------------------------------------------------------------
// race_stats
// ---------------------------------------------------------------------------

/// Compute race statistics over rows where `race_total > 1`.
///
/// We stream rows (rather than relying on a single SQL aggregate) for two
/// reasons: (1) we need to join `combo_targets` to read `priority_order`, and
/// (2) `wins_by_target` is a per-target histogram that's easier to maintain
/// in a `HashMap` than in SQL.
///
/// `avg_ttft_savings_ms` is always `None` in the MVP — we do not yet persist
/// the per-target first-byte timings required to compute it.
pub fn race_stats(conn: &Connection, f: &UsageFilter) -> Result<RaceStats> {
    let w = BuiltWhere::from_filter(f);

    // Build the WHERE clause and append the `race_total > 1` predicate that
    // defines "this row is part of a race" (rows with `race_total = 1` are
    // sequential, non-race rows and are excluded).
    let mut clauses: Vec<String> = Vec::new();
    if !w.sql.is_empty() {
        // Qualify all column references with `usage.` prefix to avoid
        // "ambiguous column name" errors when JOINing with combo_targets
        // (both tables have provider_id, combo_target_id, etc.).
        let bare = w.sql.trim_start_matches("WHERE ").to_string();
        let qualified = bare
            .replace("created_at", "usage.created_at")
            .replace("provider_id", "usage.provider_id")
            .replace("upstream_model_id", "usage.upstream_model_id")
            .replace("account_id", "usage.account_id")
            .replace("combo_id", "usage.combo_id")
            .replace("api_key_id", "usage.api_key_id")
            .replace("race_total", "usage.race_total")
            .replace("race_lost", "usage.race_lost");
        clauses.push(format!("({})", qualified));
    }
    clauses.push("usage.race_total > 1".to_string());
    let where_clause = format!("WHERE {}", clauses.join(" AND "));

    let mut sql = String::new();
    write!(
        &mut sql,
        "SELECT usage.request_id, usage.race_lost, usage.combo_target_id, ct.priority_order \
         FROM usage \
         LEFT JOIN combo_targets AS ct ON ct.id = usage.combo_target_id \
         {}",
        where_clause,
    )
    .expect("writing to String never fails");

    let mut stmt = conn.prepare(&sql).map_err(crate::error::map_db_error)?;

    let params_slice = to_params(&w.params);

    let mut winners: u64 = 0;
    let mut losers: u64 = 0;

    // Sum and count for `avg_winner_position`. We sum `priority_order` over
    // race winners whose `combo_target_id` resolved to a non-null row in
    // `combo_targets`; rows where the LEFT JOIN yielded NULL are excluded
    // from the average but still count as a winner for `winners`.
    let mut winner_pos_sum: f64 = 0.0;
    let mut winner_pos_n: u64 = 0;

    // (request_id, has_any_row) → we only need to know the set of distinct
    // request_ids to count `total_races`. A small `HashSet<String>` is fine
    // for the MVP; if this turns into a bottleneck we'd swap to a HyperLogLog.
    let mut race_ids: std::collections::HashSet<String> = std::collections::HashSet::new();

    // (combo_target_id, win_count). NULL combo_target_ids are skipped —
    // they cannot be reported as a meaningful target identifier.
    let mut wins_by_target: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();

    let mut rows = stmt
        .query(params_from_iter(params_slice))
        .map_err(crate::error::map_db_error)?;

    while let Some(row) = rows.next().map_err(crate::error::map_db_error)? {
        let request_id: String = row.get(0).map_err(crate::error::map_db_error)?;
        let race_lost: i64 = row.get(1).map_err(crate::error::map_db_error)?;
        let combo_target_id: Option<i64> = row.get(2).map_err(crate::error::map_db_error)?;
        let priority_order: Option<i64> = row.get(3).map_err(crate::error::map_db_error)?;

        race_ids.insert(request_id);

        if race_lost == 0 {
            winners += 1;
            if let (Some(_tid), Some(pos)) = (combo_target_id, priority_order) {
                winner_pos_sum += pos as f64;
                winner_pos_n += 1;
            }
            if let Some(tid) = combo_target_id {
                *wins_by_target.entry(tid).or_insert(0) += 1;
            }
        } else {
            losers += 1;
        }
    }

    let avg_winner_position = if winner_pos_n > 0 {
        Some(winner_pos_sum / winner_pos_n as f64)
    } else {
        None
    };

    // Sort wins_by_target DESC by count, then ASC by target id for a stable
    // ordering that matches the spec's contract.
    let mut wins_by_target_sorted: Vec<(i64, u64)> = wins_by_target.into_iter().collect();
    wins_by_target_sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

    Ok(RaceStats {
        total_races: race_ids.len() as u64,
        winners,
        losers,
        avg_winner_position,
        avg_ttft_savings_ms: None,
        wins_by_target: wins_by_target_sorted,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_db::migrations;
    use rusqlite::{Connection, params};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = base.join(format!("openproxy-analytics-test-{}-{}", pid, nanos));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn fresh_conn() -> (Connection, PathBuf) {
        let dir = tempdir();
        let path = dir.join("analytics-test.db");
        let mut conn = Connection::open(&path).expect("open");
        openproxy_db::migrations::run(&mut conn).expect("migrate");
        (conn, path)
    }

    /// Insert a usage row with explicit race-related fields so each test can
    /// shape its own distribution. Uses status_code=200 by default.
    // ponytail: [Demasiados argumentos] -> [Refactorizar a struct en el futuro]
    fn insert(
        conn: &Connection,
        request_id: &str,
        trace_id: &str,
        provider: &str,
        model: &str,
        connect_ms: Option<i64>,
        ttft_ms: Option<i64>,
        total_ms: i64,
        tokens_per_sec: Option<f64>,
        race_total: i64,
        race_lost: bool,
        combo_target_id: Option<i64>,
    ) {
        conn.execute(
            "INSERT INTO usage (\
                request_id, trace_id, attempt, provider_id, account_id, \
                upstream_model_id, combo_target_id, prompt_tokens, \
                completion_tokens, cost_usd, connect_ms, ttft_ms, total_ms, \
                tokens_per_sec, status_code, error_msg, error_msg_redacted, \
                race_total, race_lost, created_at\
             ) VALUES (\
                ?1, ?2, 1, ?3, NULL, ?4, ?5, 0, 0, 0.0, ?6, ?7, ?8, ?9, 200, \
                NULL, NULL, ?10, ?11, datetime('now')\
             )",
            params![
                request_id,
                trace_id,
                provider,
                model,
                combo_target_id,
                connect_ms,
                ttft_ms,
                total_ms,
                tokens_per_sec,
                race_total,
                race_lost as i64,
            ],
        )
        .expect("insert");
    }

    /// Like `insert` but with an explicit `status_code` — used to test
    /// error-row exclusion in latency percentiles.
    // ponytail: [Demasiados argumentos] -> [Refactorizar a struct en el futuro]
    fn insert_with_status(
        conn: &Connection,
        request_id: &str,
        trace_id: &str,
        provider: &str,
        model: &str,
        connect_ms: Option<i64>,
        ttft_ms: Option<i64>,
        total_ms: i64,
        race_lost: bool,
        status_code: i64,
    ) {
        conn.execute(
            "INSERT INTO usage (\
                request_id, trace_id, attempt, provider_id, account_id, \
                upstream_model_id, combo_target_id, prompt_tokens, \
                completion_tokens, cost_usd, connect_ms, ttft_ms, total_ms, \
                tokens_per_sec, status_code, error_msg, error_msg_redacted, \
                race_total, race_lost, created_at\
             ) VALUES (\
                ?1, ?2, 1, ?3, NULL, ?4, NULL, 0, 0, 0.0, ?5, ?6, ?7, \
                NULL, ?8, NULL, NULL, 1, ?9, datetime('now')\
             )",
            params![
                request_id,
                trace_id,
                provider,
                model,
                connect_ms,
                ttft_ms,
                total_ms,
                status_code,
                race_lost as i64,
            ],
        )
        .expect("insert_with_status");
    }

    // -----------------------------------------------------------------------
    // 1. latency_percentiles_uniform_distribution
    // -----------------------------------------------------------------------
    #[test]
    fn latency_percentiles_uniform_distribution() {
        let (conn, _p) = fresh_conn();

        // 100 rows: connect_ms in 0..100, with ttft/total/tps absent.
        for v in 0..100i64 {
            insert(
                &conn,
                &format!("req-{}", v),
                &format!("trace-{}", v),
                "openrouter",
                "openai/gpt-4o",
                Some(v),
                None,
                0,
                None,
                1,
                false,
                None,
            );
        }

        let r = latency_percentiles(&conn, &UsageFilter::default()).expect("latency");
        assert_eq!(r.samples, 100, "100 winner rows scanned");
        // 0..100 is 0..=99 — p50 of 0..=99 is ~49.5, p95 is ~94.05. We give a
        // generous ±5 ms tolerance to absorb t-digest approximation error
        // (200 centroids over 100 samples is overkill but spec-prescribed).
        let p50 = r.p50_connect_ms.expect("p50 connect present");
        let p95 = r.p95_connect_ms.expect("p95 connect present");
        assert!((p50 - 49.5).abs() < 5.0, "p50 ≈ 49.5, got {}", p50);
        assert!((p95 - 94.05).abs() < 5.0, "p95 ≈ 94.05, got {}", p95);
        // The other metrics were never populated in this fixture:
        //   - ttft_ms: column is nullable; we wrote NULL → no samples.
        //   - tokens_per_sec: column is nullable; we wrote NULL → no samples.
        // `total_ms` is NOT NULL with a default of 0, so every row has
        // total_ms=0 and the percentile collapses to 0.0. We just verify
        // the metric is present and equals 0.
        assert_eq!(r.p50_ttft_ms, None);
        assert_eq!(r.p95_ttft_ms, None);
        assert_eq!(r.p50_tokens_per_sec, None);
        assert_eq!(r.p95_tokens_per_sec, None);
        assert_eq!(r.p50_total_ms, Some(0.0));
        assert_eq!(r.p95_total_ms, Some(0.0));
    }

    // -----------------------------------------------------------------------
    // 2. latency_percentiles_skips_losers
    // -----------------------------------------------------------------------
    #[test]
    fn latency_percentiles_skips_losers() {
        let (conn, _p) = fresh_conn();

        // 10 winners with connect_ms = 1000..1010.
        for i in 0..10i64 {
            insert(
                &conn,
                &format!("w-{}", i),
                &format!("wt-{}", i),
                "openrouter",
                "openai/gpt-4o",
                Some(1000 + i),
                None,
                0,
                None,
                1,
                false,
                None,
            );
        }
        // 20 losers with connect_ms = 0..20 — they must not influence p50.
        for i in 0..20i64 {
            insert(
                &conn,
                &format!("l-{}", i),
                &format!("lt-{}", i),
                "openrouter",
                "openai/gpt-4o",
                Some(i),
                None,
                0,
                None,
                1,
                true, // race_lost
                None,
            );
        }

        let r = latency_percentiles(&conn, &UsageFilter::default()).expect("latency");
        assert_eq!(
            r.samples, 10,
            "only winners counted in `samples` (race_lost=0 filter)"
        );
        let p50 = r.p50_connect_ms.expect("p50 connect present");
        let p95 = r.p95_connect_ms.expect("p95 connect present");
        // Winner distribution: 1000..=1009 → p50 ~1004.5, p95 ~1009.05.
        assert!(
            (p50 - 1004.5).abs() < 5.0,
            "p50 should reflect winners (~1004.5), got {}",
            p50
        );
        assert!(
            (p95 - 1009.05).abs() < 5.0,
            "p95 should reflect winners (~1009.05), got {}",
            p95
        );
    }

    // -----------------------------------------------------------------------
    // 3. latency_percentiles_handles_nulls
    // -----------------------------------------------------------------------
    #[test]
    fn latency_percentiles_handles_nulls() {
        let (conn, _p) = fresh_conn();

        // 5 rows with ttft_ms=NULL, 5 rows with ttft_ms=Some(200). Mixing
        // null and present values must not crash and must report percentiles
        // derived only from the non-null subset.
        for i in 0..5i64 {
            insert(
                &conn,
                &format!("n-{}", i),
                &format!("nt-{}", i),
                "openrouter",
                "openai/gpt-4o",
                Some(50),
                None, // ttft NULL
                1000,
                None,
                1,
                false,
                None,
            );
        }
        for i in 0..5i64 {
            insert(
                &conn,
                &format!("v-{}", i),
                &format!("vt-{}", i),
                "openrouter",
                "openai/gpt-4o",
                Some(50),
                Some(200),
                1000,
                Some(10.0),
                1,
                false,
                None,
            );
        }

        let r = latency_percentiles(&conn, &UsageFilter::default()).expect("latency");
        assert_eq!(r.samples, 10, "all 10 winner rows scanned");
        // connect_ms and total_ms have 10 samples each.
        assert!(r.p50_connect_ms.is_some());
        assert!(r.p95_total_ms.is_some());
        // ttft_ms has 5 samples (all 200) → p50 = p95 = 200.
        let p50_ttft = r.p50_ttft_ms.expect("p50 ttft present");
        let p95_ttft = r.p95_ttft_ms.expect("p95 ttft present");
        assert!(
            (p50_ttft - 200.0).abs() < 1.0,
            "p50 ttft ≈ 200, got {}",
            p50_ttft
        );
        assert!(
            (p95_ttft - 200.0).abs() < 1.0,
            "p95 ttft ≈ 200, got {}",
            p95_ttft
        );
        // tokens_per_sec has 5 samples (all 10.0).
        let p50_tps = r.p50_tokens_per_sec.expect("p50 tps present");
        assert!(
            (p50_tps - 10.0).abs() < 0.5,
            "p50 tps ≈ 10, got {}",
            p50_tps
        );
    }

    // -----------------------------------------------------------------------
    // 3b. latency_percentiles_excludes_errors
    // -----------------------------------------------------------------------
    #[test]
    fn latency_percentiles_excludes_errors() {
        // Regression test for the "row #null" / timeout-in-percentiles bug.
        // Before the fix, timeout rows (status_code=502, connect_ms=10000)
        // were counted as "winners" (race_lost=0) and polluted p50/p95.
        let (conn, _p) = fresh_conn();

        // 10 successful rows: connect_ms = 100..110
        for i in 0..10i64 {
            insert_with_status(
                &conn,
                &format!("ok-{}", i),
                &format!("t-{}", i),
                "openrouter",
                "openai/gpt-4o",
                Some(100 + i),
                Some(200 + i),
                500 + i,
                false,
                200,
            );
        }
        // 5 error rows (timeouts): connect_ms = 10000 — these must NOT
        // influence the percentiles even though race_lost=0.
        for i in 0..5i64 {
            insert_with_status(
                &conn,
                &format!("err-{}", i),
                &format!("et-{}", i),
                "openrouter",
                "openai/gpt-4o",
                Some(10000),
                None,
                10000,
                false,
                502,
            );
        }
        // 3 error rows (client disconnects): status_code=499
        for i in 0..3i64 {
            insert_with_status(
                &conn,
                &format!("disc-{}", i),
                &format!("dt-{}", i),
                "openrouter",
                "openai/gpt-4o",
                Some(5000),
                None,
                5000,
                false,
                499,
            );
        }

        let r = latency_percentiles(&conn, &UsageFilter::default()).expect("latency");
        // Only the 10 successful rows (status_code=200) should be counted.
        assert_eq!(r.samples, 10, "error rows (502, 499) excluded from count");

        // connect_ms: 100..110 → p50 ≈ 104.5, p95 ≈ 109.05
        let p50 = r.p50_connect_ms.expect("p50 connect present");
        let p95 = r.p95_connect_ms.expect("p95 connect present");
        assert!(
            (p50 - 104.5).abs() < 5.0,
            "p50 should reflect only successes (~104.5), got {}",
            p50
        );
        assert!(
            (p95 - 109.05).abs() < 5.0,
            "p95 should reflect only successes (~109.05), got {}",
            p95
        );
        // If errors leaked through, p95 would be ~10000 — far outside tolerance.
    }

    // -----------------------------------------------------------------------
    // 4. race_stats_counts_races
    // -----------------------------------------------------------------------
    #[test]
    fn race_stats_counts_races() {
        let (conn, _p) = fresh_conn();

        // 3 races × (1 winner + 1 loser) = 6 rows total, 3 distinct
        // request_ids, race_total = 2.
        for i in 0..3i64 {
            // winner
            insert(
                &conn,
                &format!("r-{}", i),
                &format!("wt-{}", i),
                "openrouter",
                "openai/gpt-4o",
                Some(50),
                Some(200),
                1000,
                None,
                2,
                false,
                None,
            );
            // loser
            insert(
                &conn,
                &format!("r-{}", i),
                &format!("lt-{}", i),
                "openrouter",
                "openai/gpt-4o",
                Some(60),
                None,
                1500,
                None,
                2,
                true,
                None,
            );
        }

        let s = race_stats(&conn, &UsageFilter::default()).expect("race_stats");
        assert_eq!(s.total_races, 3, "3 distinct request_ids in races");
        assert_eq!(s.winners, 3, "3 winners");
        assert_eq!(s.losers, 3, "3 losers");
    }

    // -----------------------------------------------------------------------
    // 5. race_stats_ignores_non_races
    // -----------------------------------------------------------------------
    #[test]
    fn race_stats_ignores_non_races() {
        let (conn, _p) = fresh_conn();

        // 5 sequential (race_total=1) rows — must not contribute.
        for i in 0..5i64 {
            insert(
                &conn,
                &format!("seq-{}", i),
                &format!("seqt-{}", i),
                "openrouter",
                "openai/gpt-4o",
                Some(50),
                Some(200),
                1000,
                None,
                1, // not a race
                false,
                None,
            );
        }
        // 1 race to ensure the function returns non-zero when races exist.
        insert(
            &conn,
            "race-only",
            "race-only-w",
            "openrouter",
            "openai/gpt-4o",
            Some(50),
            Some(200),
            1000,
            None,
            2,
            false,
            None,
        );
        insert(
            &conn,
            "race-only",
            "race-only-l",
            "openrouter",
            "openai/gpt-4o",
            Some(60),
            None,
            1500,
            None,
            2,
            true,
            None,
        );

        let s = race_stats(&conn, &UsageFilter::default()).expect("race_stats");
        assert_eq!(s.total_races, 1, "only the race counted");
        assert_eq!(s.winners, 1);
        assert_eq!(s.losers, 1);
    }

    // -----------------------------------------------------------------------
    // 6. race_stats_wins_by_target
    // -----------------------------------------------------------------------
    #[test]
    fn race_stats_wins_by_target() {
        let (conn, _p) = fresh_conn();

        // Need combo_targets rows to resolve priority_order via the LEFT JOIN.
        // The schema requires a combo + combo_targets with valid FKs.
        conn.execute(
            "INSERT INTO providers (id, name, base_url, auth_type, format) \
             VALUES ('openrouter', 'OpenRouter', 'https://x', 'bearer', 'openai')",
            [],
        )
        .expect("provider");
        conn.execute(
            "INSERT INTO combos (name, strategy, race_size) VALUES ('c1', 'priority', 2)",
            [],
        )
        .expect("combo");
        let combo_id: i64 = conn
            .query_row("SELECT id FROM combos WHERE name='c1'", [], |r| r.get(0))
            .expect("combo id");
        conn.execute(
            "INSERT INTO models (provider_id, model_id, target_format) \
             VALUES ('openrouter', 'openai/gpt-4o', 'openai')",
            [],
        )
        .expect("model");
        let model_row_id: i64 = conn
            .query_row(
                "SELECT id FROM models WHERE provider_id='openrouter'",
                [],
                |r| r.get(0),
            )
            .expect("model id");
        // Two targets with priority 10 and 20.
        conn.execute(
            "INSERT INTO combo_targets (combo_id, provider_id, account_id, model_row_id, priority_order) \
             VALUES (?1, 'openrouter', NULL, ?2, 10)",
            params![combo_id, model_row_id],
        )
        .expect("target 1");
        let target5: i64 = conn
            .query_row(
                "SELECT id FROM combo_targets WHERE priority_order=10",
                [],
                |r| r.get(0),
            )
            .expect("target5");
        conn.execute(
            "INSERT INTO combo_targets (combo_id, provider_id, account_id, model_row_id, priority_order) \
             VALUES (?1, 'openrouter', NULL, ?2, 20)",
            params![combo_id, model_row_id],
        )
        .expect("target 2");
        let target7: i64 = conn
            .query_row(
                "SELECT id FROM combo_targets WHERE priority_order=20",
                [],
                |r| r.get(0),
            )
            .expect("target7");

        // Race 1: target5 wins.
        insert(
            &conn,
            "race-A",
            "race-A-w",
            "openrouter",
            "openai/gpt-4o",
            Some(50),
            Some(200),
            1000,
            None,
            2,
            false,
            Some(target5),
        );
        insert(
            &conn,
            "race-A",
            "race-A-l",
            "openrouter",
            "openai/gpt-4o",
            Some(60),
            None,
            1500,
            None,
            2,
            true,
            Some(target7),
        );
        // Race 2: target5 wins again.
        insert(
            &conn,
            "race-B",
            "race-B-w",
            "openrouter",
            "openai/gpt-4o",
            Some(50),
            Some(200),
            1000,
            None,
            2,
            false,
            Some(target5),
        );
        insert(
            &conn,
            "race-B",
            "race-B-l",
            "openrouter",
            "openai/gpt-4o",
            Some(60),
            None,
            1500,
            None,
            2,
            true,
            Some(target7),
        );
        // Race 3: target7 wins.
        insert(
            &conn,
            "race-C",
            "race-C-w",
            "openrouter",
            "openai/gpt-4o",
            Some(50),
            Some(200),
            1000,
            None,
            2,
            false,
            Some(target7),
        );
        insert(
            &conn,
            "race-C",
            "race-C-l",
            "openrouter",
            "openai/gpt-4o",
            Some(60),
            None,
            1500,
            None,
            2,
            true,
            Some(target5),
        );

        let s = race_stats(&conn, &UsageFilter::default()).expect("race_stats");
        assert_eq!(s.total_races, 3);
        assert_eq!(s.winners, 3);
        assert_eq!(s.losers, 3);

        // target5 has 2 wins, target7 has 1 → ordered by count DESC.
        assert_eq!(s.wins_by_target.len(), 2);
        assert_eq!(s.wins_by_target[0], (target5, 2));
        assert_eq!(s.wins_by_target[1], (target7, 1));

        // avg_winner_position: target5 has priority 10 (x2), target7 has
        // priority 20 (x1). Mean = (10 + 10 + 20) / 3 = 13.333...
        let avg = s.avg_winner_position.expect("avg_winner_position present");
        assert!(
            (avg - (40.0 / 3.0)).abs() < 1e-9,
            "avg_winner_position = 40/3, got {}",
            avg
        );

        // MVP reserves this metric; we always return None.
        assert_eq!(s.avg_ttft_savings_ms, None);
    }
}
