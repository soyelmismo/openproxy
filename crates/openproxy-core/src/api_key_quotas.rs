//! Per-API-key dollar quotas over three windows: rolling 5h, ISO week
//! (Monday 00:00 UTC) and calendar month (1st 00:00 UTC).
//!
//! Spend is measured at official model prices: the sum of `usage.cost_usd`
//! over rows that produced a client response (`client_response = 1`). Rows
//! from failed attempts never reach the client and are excluded, so a key
//! is charged exactly once per served request.
//!
//! Semantics:
//! - No row in `api_key_quotas` for a key  -> unlimited (opt-in system).
//! - A row with a NULL cap                  -> that window has no limit.
//! - `check` runs *before* the request is dispatched: it rejects when the
//!   already-recorded spend of a window reached its cap. A single in-flight
//!   request can overshoot by its own cost, which is accepted for v1.

use crate::error::Result;
use crate::ids::ApiKeyId;
use chrono::{Datelike, Duration, NaiveDate, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

pub const WINDOW_FIVE_HOURS: &str = "5h";
pub const WINDOW_WEEK: &str = "week";
pub const WINDOW_MONTH: &str = "month";

/// Caps for one key. `None` = that window is disabled.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaCaps {
    pub monthly_usd_cap: Option<f64>,
    pub weekly_usd_cap: Option<f64>,
    pub five_hour_usd_cap: Option<f64>,
}

impl QuotaCaps {
    /// True when no window is enabled (the row carries no effective limit).
    pub fn is_empty(&self) -> bool {
        self.monthly_usd_cap.is_none()
            && self.weekly_usd_cap.is_none()
            && self.five_hour_usd_cap.is_none()
    }

    pub fn sanitized(mut self) -> Self {
        self.monthly_usd_cap = sanitize_cap(self.monthly_usd_cap);
        self.weekly_usd_cap = sanitize_cap(self.weekly_usd_cap);
        self.five_hour_usd_cap = sanitize_cap(self.five_hour_usd_cap);
        self
    }
}

fn sanitize_cap(cap: Option<f64>) -> Option<f64> {
    match cap {
        Some(v) if v.is_finite() && v >= 0.0 => Some(v),
        _ => None,
    }
}

/// Spend + cap + reset information for one window, as served to the UI.
#[derive(Debug, Clone, Serialize)]
pub struct QuotaWindowStatus {
    pub cap_usd: Option<f64>,
    pub spent_usd: f64,
    /// Fixed windows: ISO-8601 UTC instant when the window resets to zero.
    /// Rolling 5h window: `None` (capacity decays as old spend ages out).
    pub resets_at: Option<String>,
    /// Rolling 5h window only: timestamp of the oldest spend still inside
    /// the window, i.e. when capacity starts being released.
    pub oldest_spend_at: Option<String>,
}

/// Full quota status for one key.
#[derive(Debug, Clone, Serialize)]
pub struct QuotaStatus {
    /// Whether the key has a quota row at all.
    pub enabled: bool,
    pub month: QuotaWindowStatus,
    pub week: QuotaWindowStatus,
    pub five_hours: QuotaWindowStatus,
}

impl QuotaStatus {
    fn unlimited() -> Self {
        let empty = QuotaWindowStatus {
            cap_usd: None,
            spent_usd: 0.0,
            resets_at: None,
            oldest_spend_at: None,
        };
        Self {
            enabled: false,
            month: empty.clone(),
            week: empty.clone(),
            five_hours: empty,
        }
    }
}

/// A violated window, ready to be turned into a 429 by the caller.
#[derive(Debug, Clone)]
pub struct QuotaViolation {
    pub window: &'static str,
    pub spent_usd: f64,
    pub cap_usd: f64,
    pub resets_at: Option<String>,
}

// ── window boundaries ───────────────────────────────────────────────

struct Boundaries {
    month_start: String,
    week_start: String,
    five_hours_ago: String,
    next_month: String,
    next_monday: String,
}

fn sqlite_ts(dt: &chrono::NaiveDateTime) -> String {
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn iso_ts(dt: &chrono::NaiveDateTime) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[allow(clippy::expect_used)]
fn boundaries() -> Boundaries {
    let now = Utc::now();
    let today = now.date_naive();
    let month_start = NaiveDate::from_ymd_opt(today.year(), today.month(), 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .expect("valid month start");
    let (ny, nm) = if today.month() == 12 {
        (today.year() + 1, 1)
    } else {
        (today.year(), today.month() + 1)
    };
    let next_month = NaiveDate::from_ymd_opt(ny, nm, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .expect("valid next month");
    let days_from_monday = today.weekday().num_days_from_monday() as i64;
    let week_start = (today - Duration::days(days_from_monday))
        .and_hms_opt(0, 0, 0)
        .expect("valid week start");
    let next_monday = (today + Duration::days(7 - days_from_monday))
        .and_hms_opt(0, 0, 0)
        .expect("valid next monday");
    let five_hours_ago = (now - Duration::hours(5)).naive_utc();
    Boundaries {
        month_start: sqlite_ts(&month_start),
        week_start: sqlite_ts(&week_start),
        five_hours_ago: sqlite_ts(&five_hours_ago),
        next_month: iso_ts(&next_month),
        next_monday: iso_ts(&next_monday),
    }
}

// ── CRUD ────────────────────────────────────────────────────────────

pub fn get_caps(conn: &Connection, key_id: ApiKeyId) -> Result<Option<QuotaCaps>> {
    let caps = conn
        .query_row(
            "SELECT monthly_usd_cap, weekly_usd_cap, five_hour_usd_cap \
             FROM api_key_quotas WHERE api_key_id = ?1",
            params![key_id.0],
            |row| {
                Ok(QuotaCaps {
                    monthly_usd_cap: row.get(0)?,
                    weekly_usd_cap: row.get(1)?,
                    five_hour_usd_cap: row.get(2)?,
                })
            },
        )
        .optional()
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(caps)
}

pub fn upsert(conn: &Connection, key_id: ApiKeyId, caps: &QuotaCaps) -> Result<()> {
    conn.execute(
        "INSERT INTO api_key_quotas \
         (api_key_id, monthly_usd_cap, weekly_usd_cap, five_hour_usd_cap, updated_at) \
         VALUES (?1, ?2, ?3, ?4, datetime('now')) \
         ON CONFLICT(api_key_id) DO UPDATE SET \
           monthly_usd_cap = excluded.monthly_usd_cap, \
           weekly_usd_cap = excluded.weekly_usd_cap, \
           five_hour_usd_cap = excluded.five_hour_usd_cap, \
           updated_at = datetime('now')",
        params![
            key_id.0,
            caps.monthly_usd_cap,
            caps.weekly_usd_cap,
            caps.five_hour_usd_cap
        ],
    )
    .map_err(openproxy_db::error::map_db_error)?;
    Ok(())
}

pub fn delete(conn: &Connection, key_id: ApiKeyId) -> Result<()> {
    conn.execute(
        "DELETE FROM api_key_quotas WHERE api_key_id = ?1",
        params![key_id.0],
    )
    .map_err(openproxy_db::error::map_db_error)?;
    Ok(())
}

// ── spend + enforcement ─────────────────────────────────────────────

/// Full status for one key: caps + spend per window + reset info.
pub fn status(conn: &Connection, key_id: ApiKeyId) -> Result<QuotaStatus> {
    let Some(caps) = get_caps(conn, key_id)? else {
        return Ok(QuotaStatus::unlimited());
    };
    let b = boundaries();
    let (month_spent, week_spent, five_hour_spent, oldest_5h): (f64, f64, f64, Option<String>) =
        conn.query_row(
            "SELECT \
               COALESCE((SELECT SUM(u.cost_usd) FROM usage u \
                  WHERE u.api_key_id = ?1 AND u.client_response = 1 AND u.created_at >= ?2), 0.0), \
               COALESCE((SELECT SUM(u.cost_usd) FROM usage u \
                  WHERE u.api_key_id = ?1 AND u.client_response = 1 AND u.created_at >= ?3), 0.0), \
               COALESCE((SELECT SUM(u.cost_usd) FROM usage u \
                  WHERE u.api_key_id = ?1 AND u.client_response = 1 AND u.created_at >= ?4), 0.0), \
               (SELECT MIN(u.created_at) FROM usage u \
                  WHERE u.api_key_id = ?1 AND u.client_response = 1 \
                    AND u.cost_usd > 0 AND u.created_at >= ?4)",
            params![key_id.0, b.month_start, b.week_start, b.five_hours_ago],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(QuotaStatus {
        enabled: true,
        month: QuotaWindowStatus {
            cap_usd: caps.monthly_usd_cap,
            spent_usd: month_spent,
            resets_at: Some(b.next_month),
            oldest_spend_at: None,
        },
        week: QuotaWindowStatus {
            cap_usd: caps.weekly_usd_cap,
            spent_usd: week_spent,
            resets_at: Some(b.next_monday),
            oldest_spend_at: None,
        },
        five_hours: QuotaWindowStatus {
            cap_usd: caps.five_hour_usd_cap,
            spent_usd: five_hour_spent,
            resets_at: None,
            oldest_spend_at: oldest_5h,
        },
    })
}

/// Pre-flight check. Returns `Some(violation)` when the key already spent
/// its cap in any enabled window (largest window first).
pub fn check(conn: &Connection, key_id: ApiKeyId) -> Result<Option<QuotaViolation>> {
    let st = status(conn, key_id)?;
    if !st.enabled {
        return Ok(None);
    }
    let violated = [
        (WINDOW_MONTH, &st.month),
        (WINDOW_WEEK, &st.week),
        (WINDOW_FIVE_HOURS, &st.five_hours),
    ]
    .into_iter()
    .find_map(|(name, w)| match w.cap_usd {
        Some(cap) if w.spent_usd >= cap => Some(QuotaViolation {
            window: name,
            spent_usd: w.spent_usd,
            cap_usd: cap,
            resets_at: w.resets_at.clone(),
        }),
        _ => None,
    });
    Ok(violated)
}

#[cfg(test)]
mod tests;
