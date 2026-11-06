//! Latency percentiles (t-digest) and race statistics, backing
//! `/admin/usage/latency` and `/admin/usage/races`. Spec: docs/mvp-spec.md §7.
//!
//! [`latency_percentiles`] streams the latency columns of race winners
//! (`race_lost = 0`) into per-metric t-digests and returns p50/p95.
//! [`race_stats`] streams rows with `race_total > 1` and aggregates totals,
//! average winner position and per-target wins in Rust.

#[cfg(test)]
mod tests;

use crate::error::{CoreError, Result};
use crate::usage::UsageFilter;
use rusqlite::{Connection, ToSql, params_from_iter};
use serde::{Deserialize, Serialize};
use tdigest::TDigest;

/// Centroids per t-digest. Spec §7 prescribes 200.
const TDIGEST_CENTROIDS: usize = 200;

/// Samples buffered before merging into the running t-digest. Raw `f64` values
/// go into one `merge_unsorted` per batch, amortizing the O(max_size) merge over
/// the whole batch instead of paying it per row.
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
    /// `COUNT(DISTINCT request_id)` over rows with `race_total > 1`.
    pub total_races: u64,
    /// `COUNT(*)` of winners (`race_lost = 0`). Equals `total_races` when every
    /// race produced exactly one winner; diverges when a race ends with no
    /// winner or several.
    pub winners: u64,
    /// `COUNT(*)` of losers (`race_lost = 1`).
    pub losers: u64,
    /// Average `priority_order` of the winning target. `None` when no winner has
    /// a resolvable `combo_target_id`. `1.0` means the first target always wins.
    pub avg_winner_position: Option<f64>,
    /// Always `None`: the spec reserves it for the winner's `ttft` minus the first
    /// target's `ttft`, and that per-target first-byte data is not persisted yet.
    pub avg_ttft_savings_ms: Option<f64>,
    /// `(combo_target_id, win_count)` ordered by `win_count` DESC, then
    /// `combo_target_id` ASC for stable output.
    pub wins_by_target: Vec<(i64, u64)>,
}

struct BuiltWhere<'a> {
    sql: String,
    params: Vec<&'a dyn ToSql>,
}

/// NOT IN clauses shared by every query that qualifies the same columns
/// (the race join qualifies them with `usage.` via string replace, so the
/// bare column name must appear first in each clause).
fn push_exclusions<'a>(
    clauses: &mut Vec<String>,
    params: &mut Vec<&'a dyn ToSql>,
    f: &'a UsageFilter,
) {
    // Public-scope queries must never match internal provider/model columns:
    // the exclusions live in the visibility clause (public names only).
    if f.scope_public.is_some() {
        return;
    }
    for (col, values) in [
        ("upstream_model_id", &f.exclude_models),
        ("provider_id", &f.exclude_providers),
    ] {
        if values.is_empty() {
            continue;
        }
        let placeholders = std::iter::repeat("?")
            .take(values.len())
            .collect::<Vec<_>>()
            .join(", ");
        clauses.push(format!("{col} NOT IN ({placeholders})"));
        for v in values {
            params.push(v);
        }
    }
}

/// Public-sandbox visibility clause (same semantics as
/// `usage::analytics::filter::push_public_visibility`, but bound to this
/// module's borrowed-param builder): combo rows are public via the combo
/// alias, direct rows via the key's whitelisted catalog entries.
fn push_public_visibility<'a>(
    clauses: &mut Vec<String>,
    params: &mut Vec<&'a dyn ToSql>,
    scope: &'a crate::usage::analytics::filter::PublicScope,
) {
    let mut push_list = |sql: &mut String, col: &str, values: &'a [String], negated: bool| {
        if values.is_empty() {
            return;
        }
        let placeholders = std::iter::repeat("?")
            .take(values.len())
            .collect::<Vec<_>>()
            .join(", ");
        sql.push_str(&format!(
            " AND {col} {} ({placeholders})",
            if negated { "NOT IN" } else { "IN" }
        ));
        for v in values {
            params.push(v);
        }
    };
    let excluded = &scope.excluded_public;

    let mut combo_sub = String::from(
        "(combo_id IS NOT NULL AND combo_id > 0 AND combo_id IN \
         (SELECT c.id FROM combos c WHERE 1=1",
    );
    push_list(&mut combo_sub, "c.name", excluded, true);
    combo_sub.push_str("))");

    let mut direct_sub = String::from(
        "((combo_id IS NULL OR combo_id <= 0) AND model_row_id IS NOT NULL \
         AND model_row_id IN (SELECT m.id FROM models m WHERE 1=1",
    );
    push_list(&mut direct_sub, "m.model_id", excluded, true);
    if let Some(providers) = &scope.allowed_providers {
        push_list(&mut direct_sub, "m.provider_id", providers, false);
    }
    if let Some(models) = &scope.allowed_models {
        push_list(&mut direct_sub, "m.model_id", models, false);
    }
    direct_sub.push_str("))");

    clauses.push(format!("({combo_sub} OR {direct_sub})"));
}

impl<'a> BuiltWhere<'a> {
    fn from_filter(f: &'a UsageFilter) -> Self {
        let mut clauses: Vec<String> = Vec::new();
        let mut params: Vec<&'a dyn ToSql> = Vec::new();

        if let Some(from) = &f.from {
            // Qualified: public-scope queries JOIN tables that also carry a
            // `created_at` column.
            clauses.push("usage.created_at >= datetime(?)".into());
            params.push(from);
        }
        if let Some(to) = &f.to {
            clauses.push("usage.created_at < datetime(?)".into());
            params.push(to);
        }
        // Tenant isolation is enforced here as well: latency/race queries
        // were previously ignoring api_key_id, which leaked cross-key stats.
        if let Some(kid) = &f.api_key_id {
            clauses.push("api_key_id = ?".into());
            params.push(&kid.0);
        }
        if let Some(scope) = &f.scope_public {
            // Public sandbox: internal provider/model columns are never
            // matched (they are not even filterable) and the visibility
            // clause restricts rows to the public projection space.
            if let Some(mid) = &f.model_id {
                clauses.push(
                    "(combo_id IN (SELECT c.id FROM combos c WHERE c.name = ?) OR \
                     (model_row_id IS NOT NULL AND model_row_id IN \
                     (SELECT m.id FROM models m WHERE m.model_id = ?)))"
                        .into(),
                );
                params.push(mid);
                params.push(mid);
            }
            if let Some(aid) = &f.account_id {
                clauses.push("account_id = ?".into());
                params.push(&aid.0);
            }
            if let Some(cid) = &f.combo_id {
                clauses.push("combo_id = ?".into());
                params.push(&cid.0);
            }
            push_public_visibility(&mut clauses, &mut params, scope);
        } else {
            if let Some(pid) = &f.provider_id {
                clauses.push("provider_id = ?".into());
                params.push(&pid.0);
            }
            if let Some(mid) = &f.model_id {
                clauses.push("upstream_model_id = ?".into());
                params.push(mid);
            }
            if let Some(aid) = &f.account_id {
                clauses.push("account_id = ?".into());
                params.push(&aid.0);
            }
            if let Some(cid) = &f.combo_id {
                clauses.push("combo_id = ?".into());
                params.push(&cid.0);
            }
            push_exclusions(&mut clauses, &mut params, f);
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

    let sql = format!(
        "SELECT connect_ms, ttft_ms, total_ms, tokens_per_sec \
         FROM usage {where_clause}"
    );

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

fn build_race_where_clause(where_sql: &str) -> String {
    let mut clauses: Vec<String> = Vec::new();
    if !where_sql.is_empty() {
        let bare = where_sql.trim_start_matches("WHERE ");
        // Qualify bare columns for the race join. Already-qualified tokens
        // (usage.created_at from the builder, m.provider_id from the public
        // sandbox subqueries) are masked out so the replace stays idempotent.
        let qualified = bare
            .replace("usage.created_at", "\u{1}A\u{1}")
            .replace("m.provider_id", "\u{1}B\u{1}")
            .replace("created_at", "usage.created_at")
            .replace("model_row_id", "usage.model_row_id")
            .replace("provider_id", "usage.provider_id")
            .replace("upstream_model_id", "usage.upstream_model_id")
            .replace("account_id", "usage.account_id")
            .replace("combo_id", "usage.combo_id")
            .replace("api_key_id", "usage.api_key_id")
            .replace("race_total", "usage.race_total")
            .replace("race_lost", "usage.race_lost")
            .replace("\u{1}A\u{1}", "usage.created_at")
            .replace("\u{1}B\u{1}", "m.provider_id");
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
