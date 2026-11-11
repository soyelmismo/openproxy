//! Learned idle-chunk budgets for streaming bodies.
//!
//! The TTFT predictor (`super`) bounds how long a target may take to first
//! byte. The complementary failure mode is a stream that opens, starts
//! emitting, and then goes silent mid-generation: that gap is bounded by the
//! resolved `idle_chunk` timeout, which is a global constant (30s in the
//! live deployment) with no per-target notion at all.
//!
//! This module teaches the predictor the *gap* distribution:
//!
//! - `UpstreamBodyStream` measures survived gaps between content/progress
//!   chunks (`ChunkGapTracker`) and records them on the usage row
//!   (`max_chunk_gap_ms` / `avg_chunk_gap_ms`).
//! - Here, the pooled gap statistics of a target (or one of its sessions)
//!   produce `budget = max(mean + 3σ, p99) + MARGIN_CHUNK_MS`, floored and
//!   clamped to the resolved ceiling like the TTFT budget.
//! - Anti-starvation mirrors the TTFT path: recorded `idle_chunk` cuts
//!   (`upstream timeout in phase idle_chunk after Nms`) relax the next
//!   budget by [`CENSORED_RELAX_MULT`], turning the predictor off once the
//!   relax reaches the ceiling.
//!
//! Gaps are pooled into ONE distribution instead of token buckets: chunk
//! pacing is an upstream property (SSE cadence, proxy buffering), not a
//! request-size property. Fewer buckets means the minimum-sample gate
//! ([`MIN_BUCKET_SAMPLES`]) is reached sooner.

use openproxy_types::ids::ModelRowId;
use parking_lot::Mutex;
use rusqlite::Connection;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::loaders::{CENSORED_LIMIT, HISTORY_WINDOW_DAYS, SAMPLE_LIMIT, SESSION_WINDOW_MINUTES};
use super::stats::{BucketStats, MIN_BUCKET_SAMPLES, Sample, weighted_stats};
use super::{BudgetKey, CENSORED_RELAX_MULT, LatencyBudget, TargetStats};

/// Gaps are long-tailed by nature (one reasoning segment can legitimately
/// hold the stream for tens of seconds even on healthy targets); the floor
/// keeps the predictor from cutting a stream that is merely pacing slowly.
const FLOOR_CHUNK_MS: u64 = 10_000;
/// Wider fixed margin than TTFT: a cut mid-stream is far more expensive than
/// a slightly longer wait, and the gap distribution noisier than TTFT.
const MARGIN_CHUNK_MS: u64 = 2_000;

impl LatencyBudget {
    /// Learned idle-chunk budget for one target (or session), or `None` to
    /// keep the resolved `idle_chunk` timeout untouched. `ceiling_ms` is the
    /// resolved idle-chunk timeout after defaults/overrides/floors.
    ///
    /// Mirrors [`super::LatencyBudget::ttft_budget_ms`]: session snapshot
    /// first, global fallback. Gaps are not bucketed by prompt tokens, so
    /// there is no `est_prompt_tokens` argument.
    pub fn idle_chunk_budget_ms(
        self: &Arc<Self>,
        provider: &str,
        model_row_id: ModelRowId,
        session_hash: Option<u64>,
        ceiling_ms: u64,
        sync_conn: Option<&Mutex<Connection>>,
    ) -> Option<u64> {
        if ceiling_ms == 0 {
            // Nothing to shorten; a zero ceiling is not a prediction.
            return None;
        }
        if let Some(hash) = session_hash {
            let key = BudgetKey::session(provider, model_row_id.0, hash);
            if let Some(stats) = self.snapshot(&key, sync_conn)
                && let Some(budget) = budget_from_gap_stats(&stats, ceiling_ms)
            {
                return Some(budget);
            }
        }
        let key = BudgetKey::global(provider, model_row_id.0);
        let stats = self.snapshot(&key, sync_conn)?;
        budget_from_gap_stats(&stats, ceiling_ms)
    }
}

/// Resolves the learned gap budget of one level, or `None` when the level
/// has no trustworthy gap data or its budget reaches the ceiling.
pub(super) fn budget_from_gap_stats(stats: &TargetStats, ceiling_ms: u64) -> Option<u64> {
    let gap = stats.gap_stats.as_ref()?;
    let mut budget_ms = gap.budget_ms_with(FLOOR_CHUNK_MS, MARGIN_CHUNK_MS);
    // Anti-starvation relax (same contract as the TTFT path): every recorded
    // idle-chunk cut raises the floor of the next budget; once the relax
    // reaches the ceiling the predictor turns itself off for this level.
    let censored = stats.chunk_censored_max_ms.load(Ordering::Relaxed);
    if censored > 0 {
        let relaxed = (censored as f64 * CENSORED_RELAX_MULT) as u64;
        budget_ms = budget_ms.max(relaxed);
    }
    (budget_ms < ceiling_ms).then_some(budget_ms)
}

/// Pooled gap distribution of one level, or `None` below the sample gate.
/// Called from `load_sync`, so every snapshot refresh feeds it too.
pub(super) fn load_gap_stats(
    conn: &Connection,
    key: &BudgetKey,
    half_life_h: f64,
) -> Option<BucketStats> {
    let rows = load_gap_rows(conn, key)?;
    let weighted: Vec<(u64, f64)> = rows
        .iter()
        .map(|r| (r.value_ms, 0.5f64.powf(r.age_hours / half_life_h)))
        .collect();
    (weighted.len() >= MIN_BUCKET_SAMPLES).then(|| weighted_stats(&weighted))
}

/// Survived-gap rows for one level: successful streaming requests that
/// recorded their largest inter-chunk gap.
fn load_gap_rows(conn: &Connection, key: &BudgetKey) -> Option<Vec<Sample>> {
    let (window, session_filter) = match key.session_hash {
        Some(_) => (
            format!("-{SESSION_WINDOW_MINUTES} minutes"),
            "AND session_hash = ?5",
        ),
        None => (format!("-{HISTORY_WINDOW_DAYS} days"), ""),
    };
    let sql = format!(
        "SELECT max_chunk_gap_ms, prompt_tokens, prompt_tokens_estimated, \
                (julianday('now') - julianday(created_at)) * 24.0 \
         FROM usage \
         WHERE provider_id = ?1 AND model_row_id = ?2 \
           AND status_code >= 200 AND status_code < 400 \
           AND max_chunk_gap_ms IS NOT NULL AND max_chunk_gap_ms > 0 \
           AND created_at >= datetime('now', ?3) \
           {session_filter} \
         ORDER BY id DESC LIMIT ?4"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return None;
    };
    let out: Vec<Sample> = match key.session_hash {
        Some(hash) => {
            let Ok(rows) = stmt.query_map(
                rusqlite::params![
                    key.provider,
                    key.model_row_id,
                    window,
                    SAMPLE_LIMIT,
                    hash as i64
                ],
                map_gap_row,
            ) else {
                return None;
            };
            rows.filter_map(|r| r.ok()).collect()
        }
        None => {
            let Ok(rows) = stmt.query_map(
                rusqlite::params![key.provider, key.model_row_id, window, SAMPLE_LIMIT],
                map_gap_row,
            ) else {
                return None;
            };
            rows.filter_map(|r| r.ok()).collect()
        }
    };
    (!out.is_empty()).then_some(out)
}

fn map_gap_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Sample> {
    Ok(Sample {
        value_ms: (row.get::<_, Option<i64>>(0)?.unwrap_or(0) as u64).max(1),
        prompt_tokens: row.get::<_, Option<i64>>(1)?.unwrap_or(0).max(0) as u64,
        estimated: row.get::<_, Option<i64>>(2)?.unwrap_or(0) != 0,
        age_hours: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0).max(0.0),
    })
}

/// Largest `idle_chunk`-phase timeout for this target within the window.
/// Rows look like `upstream timeout in phase idle_chunk after 30000ms`.
pub(super) fn load_chunk_censored_max(conn: &Connection, key: &BudgetKey) -> u64 {
    let (window, session_filter) = match key.session_hash {
        Some(_) => (
            format!("-{SESSION_WINDOW_MINUTES} minutes"),
            "AND session_hash = ?5",
        ),
        None => (format!("-{HISTORY_WINDOW_DAYS} days"), ""),
    };
    let sql = format!(
        "SELECT error_msg FROM usage \
         WHERE provider_id = ?1 AND model_row_id = ?2 \
           AND error_msg LIKE '%in phase idle_chunk after %ms%' \
           AND created_at >= datetime('now', ?3) \
           {session_filter} \
         ORDER BY id DESC LIMIT ?4"
    );
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return 0;
    };
    let mut max_ms: u64 = 0;
    match key.session_hash {
        Some(hash) => {
            let Ok(rows) = stmt.query_map(
                rusqlite::params![
                    key.provider,
                    key.model_row_id,
                    window,
                    CENSORED_LIMIT,
                    hash as i64
                ],
                |r| r.get::<_, Option<String>>(0),
            ) else {
                return 0;
            };
            for msg in rows.filter_map(|r| r.ok()).flatten() {
                if let Some(ms) = parse_idle_chunk_timeout_ms(&msg) {
                    max_ms = max_ms.max(ms);
                }
            }
        }
        None => {
            let Ok(rows) = stmt.query_map(
                rusqlite::params![key.provider, key.model_row_id, window, CENSORED_LIMIT],
                |r| r.get::<_, Option<String>>(0),
            ) else {
                return 0;
            };
            for msg in rows.filter_map(|r| r.ok()).flatten() {
                if let Some(ms) = parse_idle_chunk_timeout_ms(&msg) {
                    max_ms = max_ms.max(ms);
                }
            }
        }
    }
    max_ms
}

/// Parses `upstream timeout in phase idle_chunk after 30000ms` into 30000.
fn parse_idle_chunk_timeout_ms(error_msg: &str) -> Option<u64> {
    let marker = "in phase idle_chunk after ";
    let idx = error_msg.find(marker)?;
    let rest = &error_msg[idx + marker.len()..];
    let end = rest.find("ms")?;
    rest[..end].trim().parse::<u64>().ok()
}
