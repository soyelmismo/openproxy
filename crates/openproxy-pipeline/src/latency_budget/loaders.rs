//! DB loaders of the latency predictor: recent successful samples per
//! (provider, model_row) level and recorded timeout cuts for the
//! anti-starvation relax. Pure queries; the cache in `super` calls these
//! from `load_sync` and from the background refresh.

use rusqlite::Connection;

use super::BudgetKey;
use super::stats::Sample;

/// How many recent successful rows per target feed the snapshot.
pub(super) const SAMPLE_LIMIT: i64 = 400;
/// History window for successes and cuts alike.
pub(super) const HISTORY_WINDOW_DAYS: i64 = 7;

/// Session history window: only the last ten minutes of that session feed
/// it, so a cache that expired six minutes ago stops paying for the warm
/// regime it had before.
pub(super) const SESSION_WINDOW_MINUTES: i64 = 10;

/// Cut rows (`headers`-phase timeouts) considered for the relax.
pub(super) const CENSORED_LIMIT: i64 = 100;

/// Pull recent successful TTFT samples for one key from `usage`. The global
/// level aggregates every row of the target (sessioned or not); a session
/// level reads only its own rows, over the shorter session window.
pub(super) fn load_success_rows(conn: &Connection, key: &BudgetKey) -> Option<Vec<Sample>> {
    let (window, session_filter) = match key.session_hash {
        Some(_) => (
            format!("-{SESSION_WINDOW_MINUTES} minutes"),
            "AND session_hash = ?5",
        ),
        None => (format!("-{HISTORY_WINDOW_DAYS} days"), ""),
    };
    let sql = format!(
        "SELECT ttft_ms, prompt_tokens, prompt_tokens_estimated, \
                (julianday('now') - julianday(created_at)) * 24.0 \
         FROM usage \
         WHERE provider_id = ?1 AND model_row_id = ?2 \
           AND status_code >= 200 AND status_code < 400 \
           AND ttft_ms IS NOT NULL AND ttft_ms > 0 \
           AND created_at >= datetime('now', ?3) \
           {session_filter} \
         ORDER BY id DESC LIMIT ?4"
    );
    let mut stmt = conn.prepare(&sql).ok()?;
    let out: Vec<Sample> = match key.session_hash {
        Some(hash) => {
            let rows = stmt
                .query_map(
                    rusqlite::params![
                        key.provider,
                        key.model_row_id,
                        window,
                        SAMPLE_LIMIT,
                        hash as i64
                    ],
                    map_success_row,
                )
                .ok()?;
            rows.filter_map(|r| r.ok()).collect()
        }
        None => {
            let rows = stmt
                .query_map(
                    rusqlite::params![key.provider, key.model_row_id, window, SAMPLE_LIMIT],
                    map_success_row,
                )
                .ok()?;
            rows.filter_map(|r| r.ok()).collect()
        }
    };
    if out.is_empty() { None } else { Some(out) }
}

pub(super) fn map_success_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Sample> {
    Ok(Sample {
        value_ms: (row.get::<_, Option<f64>>(0)?.unwrap_or(0.0) as u64).max(1),
        prompt_tokens: row.get::<_, Option<i64>>(1)?.unwrap_or(0).max(0) as u64,
        estimated: row.get::<_, Option<i64>>(2)?.unwrap_or(0) != 0,
        age_hours: row.get::<_, Option<f64>>(3)?.unwrap_or(0.0).max(0.0),
    })
}

/// Largest `headers`-phase timeout for this target within the window. Rows
/// look like `upstream timeout in phase ttft after 8274ms` (correction 1);
/// legacy rows may still say `in phase headers after ...`.
pub(super) fn load_censored_max(conn: &Connection, key: &BudgetKey) -> u64 {
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
           AND (error_msg LIKE '%in phase ttft after %ms%' \
                OR error_msg LIKE '%in phase headers after %ms%') \
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
                if let Some(ms) = parse_headers_timeout_ms(&msg) {
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
                if let Some(ms) = parse_headers_timeout_ms(&msg) {
                    max_ms = max_ms.max(ms);
                }
            }
        }
    }
    max_ms
}

/// A recorded `headers`-phase timeout. Production names the phase by its
/// config hint: streaming errors say `ttft` (`streaming::pipeline`
/// renames `Headers`), older/unary rows say `headers`. Both parse.
pub(super) fn parse_headers_timeout_ms(error_msg: &str) -> Option<u64> {
    let marker = ["in phase ttft after ", "in phase headers after "]
        .iter()
        .find_map(|m| error_msg.find(m).map(|idx| (idx, m.len())))?;
    let rest = &error_msg[marker.0 + marker.1..];
    let end = rest.find("ms")?;
    rest[..end].trim().parse::<u64>().ok()
}
