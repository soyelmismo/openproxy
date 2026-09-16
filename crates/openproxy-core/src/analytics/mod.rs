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

#[cfg(test)]
mod tests;

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

struct BuiltWhere<'a> {
    sql: String,
    params: Vec<&'a dyn ToSql>,
}

impl<'a> BuiltWhere<'a> {
    fn from_filter(f: &'a UsageFilter) -> Self {
        let mut clauses: Vec<&'static str> = Vec::new();
        let mut params: Vec<&'a dyn ToSql> = Vec::new();

        if let Some(from) = &f.from {
            clauses.push("created_at >= ?");
            params.push(from);
        }
        if let Some(to) = &f.to {
            clauses.push("created_at < ?");
            params.push(to);
        }
        if let Some(pid) = &f.provider_id {
            clauses.push("provider_id = ?");
            params.push(&pid.0);
        }
        if let Some(mid) = &f.model_id {
            clauses.push("upstream_model_id = ?");
            params.push(mid);
        }
        if let Some(aid) = &f.account_id {
            clauses.push("account_id = ?");
            params.push(&aid.0);
        }
        if let Some(cid) = &f.combo_id {
            clauses.push("combo_id = ?");
            params.push(&cid.0);
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

// ---------------------------------------------------------------------------
// T-digest plumbing
// ---------------------------------------------------------------------------

struct StreamingDigest {
    digest: TDigest,
    buffer: Vec<f64>,
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
        self.digest = self.digest.merge_unsorted(batch);
        self.buffer.reserve(MERGE_BATCH);
    }

    fn quantile(&mut self, q: f64) -> Option<f64> {
        if self.count == 0 {
            return None;
        }
        self.flush();
        self.digest.estimate_quantile(q)
    }
}

// ---------------------------------------------------------------------------
// latency_percentiles
// ---------------------------------------------------------------------------

pub fn latency_percentiles(conn: &Connection, f: &UsageFilter) -> Result<LatencyPercentiles> {
    let w = BuiltWhere::from_filter(f);

    let mut clauses: Vec<String> = Vec::new();
    if !w.sql.is_empty() {
        let bare = w.sql.trim_start_matches("WHERE ");
        clauses.push(format!("({bare})"));
    }
    clauses.push("race_lost = 0".to_string());
    clauses.push("status_code >= 200 AND status_code < 400".to_string());
    clauses.push("(error_msg IS NULL OR error_msg != 'predict_skipped')".to_string());

    let where_clause = format!("WHERE {}", clauses.join(" AND "));

    let mut sql = String::new();
    write!(
        &mut sql,
        "SELECT connect_ms, ttft_ms, total_ms, tokens_per_sec \
         FROM usage {where_clause}",
    )
    .expect("writing to String never fails");

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let mut connect = StreamingDigest::new();
    let mut ttft = StreamingDigest::new();
    let mut total = StreamingDigest::new();
    let mut tps = StreamingDigest::new();
    let mut rows_seen: u64 = 0;

    let mut rows = stmt
        .query(params_from_iter(w.params.iter().copied()))
        .map_err(openproxy_db::error::map_db_error)?;

    while let Some(row) = rows.next().map_err(openproxy_db::error::map_db_error)? {
        rows_seen += 1;
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
        message: format!("read latency_percentiles column {column}: {e}"),
        source: Some(std::sync::Arc::new(e)),
    }
}

// ---------------------------------------------------------------------------
// race_stats
// ---------------------------------------------------------------------------

fn build_race_where_clause(where_sql: &str) -> String {
    let mut clauses: Vec<String> = Vec::new();
    if !where_sql.is_empty() {
        let bare = where_sql.trim_start_matches("WHERE ");
        let qualified = bare
            .replace("created_at", "usage.created_at")
            .replace("provider_id", "usage.provider_id")
            .replace("upstream_model_id", "usage.upstream_model_id")
            .replace("account_id", "usage.account_id")
            .replace("combo_id", "usage.combo_id")
            .replace("api_key_id", "usage.api_key_id")
            .replace("race_total", "usage.race_total")
            .replace("race_lost", "usage.race_lost");
        clauses.push(format!("({qualified})"));
    }
    clauses.push("usage.race_total > 1".to_string());
    format!("WHERE {}", clauses.join(" AND "))
}

#[derive(Default)]
struct RaceStatsAccumulator {
    winners: u64,
    losers: u64,
    winner_pos_sum: f64,
    winner_pos_n: u64,
    race_ids: std::collections::HashSet<String>,
    wins_by_target: std::collections::HashMap<i64, u64>,
}

impl RaceStatsAccumulator {
    fn process_row(&mut self, row: &rusqlite::Row<'_>) -> rusqlite::Result<()> {
        let request_id: String = row.get(0)?;
        let race_lost: i64 = row.get(1)?;
        let combo_target_id: Option<i64> = row.get(2)?;
        let priority_order: Option<i64> = row.get(3)?;

        self.race_ids.insert(request_id);

        if race_lost == 0 {
            self.winners += 1;
            if let (Some(_tid), Some(pos)) = (combo_target_id, priority_order) {
                self.winner_pos_sum += pos as f64;
                self.winner_pos_n += 1;
            }
            if let Some(tid) = combo_target_id {
                *self.wins_by_target.entry(tid).or_insert(0) += 1;
            }
        } else {
            self.losers += 1;
        }
        Ok(())
    }

    fn finish(self) -> RaceStats {
        let avg_winner_position = if self.winner_pos_n > 0 {
            Some(self.winner_pos_sum / self.winner_pos_n as f64)
        } else {
            None
        };

        let mut wins_by_target_sorted: Vec<(i64, u64)> = self.wins_by_target.into_iter().collect();
        wins_by_target_sorted.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));

        RaceStats {
            total_races: self.race_ids.len() as u64,
            winners: self.winners,
            losers: self.losers,
            avg_winner_position,
            avg_ttft_savings_ms: None,
            wins_by_target: wins_by_target_sorted,
        }
    }
}

pub fn race_stats(conn: &Connection, f: &UsageFilter) -> Result<RaceStats> {
    let w = BuiltWhere::from_filter(f);
    let where_clause = build_race_where_clause(&w.sql);

    let sql = format!(
        "SELECT usage.request_id, usage.race_lost, usage.combo_target_id, ct.priority_order \
         FROM usage \
         LEFT JOIN combo_targets AS ct ON ct.id = usage.combo_target_id \
         {where_clause}"
    );

    let mut stmt = conn
        .prepare(&sql)
        .map_err(openproxy_db::error::map_db_error)?;

    let mut acc = RaceStatsAccumulator::default();
    let mut rows = stmt
        .query(params_from_iter(w.params.iter().copied()))
        .map_err(openproxy_db::error::map_db_error)?;

    while let Some(row) = rows.next().map_err(openproxy_db::error::map_db_error)? {
        acc.process_row(row)
            .map_err(openproxy_db::error::map_db_error)?;
    }

    Ok(acc.finish())
}
