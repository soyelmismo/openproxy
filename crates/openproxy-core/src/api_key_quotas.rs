//! Per-API-key dollar quotas over two windows: rolling 5h and ISO week
//! (Monday 00:00 UTC).
//!
//! The monthly bound of a plan is NOT a quota window: it is the credited
//! balance the buyer paid for. Spending draws the balance down to zero, so a
//! separate monthly cap would be a second source of truth fighting the plan.
//! Only 5h and week remain as optional caps.
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
use chrono::{Datelike, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

pub const WINDOW_WEEK: &str = "week";
pub const WINDOW_FIVE_HOURS: &str = "5h";

/// Caps for one key. `None` = that window is disabled.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuotaCaps {
    pub weekly_usd_cap: Option<f64>,
    pub five_hour_usd_cap: Option<f64>,
}

impl QuotaCaps {
    /// True when no window is enabled (the row carries no effective limit).
    pub fn is_empty(&self) -> bool {
        self.weekly_usd_cap.is_none() && self.five_hour_usd_cap.is_none()
    }
}

/// Spend + cap + reset information for one window, as served to the UI.
#[derive(Debug, Clone, Serialize)]
pub struct QuotaWindowStatus {
    pub cap_usd: Option<f64>,
    pub spent_usd: f64,
    /// Fixed windows: ISO-8601 UTC instant when the window resets to zero.
    /// `None` when the window has no cap (nothing to reset) and for the
    /// rolling 5h window, whose capacity decays as old spend ages out.
    pub resets_at: Option<String>,
    /// Rolling 5h window only: timestamp of the oldest spend still inside
    /// the window, i.e. when capacity starts being released.
    pub oldest_spend_at: Option<String>,
}

/// The plan attached to a key: what the buyer paid for and how much of it
/// is left. This is the monthly bound — the balance itself.
#[derive(Debug, Clone, Serialize)]
pub struct PlanStatus {
    pub name: String,
    /// Credited balance the plan grants (the maximum the key can spend).
    pub credit_usd: f64,
    /// Balance still available, in USD.
    pub balance_usd: f64,
    /// ISO-8601 UTC instant the plan expires, when it has a lifetime.
    pub expires_at: Option<String>,
}

/// Full quota status for one key.
#[derive(Debug, Clone, Serialize)]
pub struct QuotaStatus {
    /// Whether the key has an effective window cap (5h or week).
    pub enabled: bool,
    pub week: QuotaWindowStatus,
    pub five_hours: QuotaWindowStatus,
    /// Plan (tier) attached to the key, if any. Its balance is the monthly
    /// limit: the panel shows it as a percentage bar with the expiry.
    pub plan: Option<PlanStatus>,
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
            week: empty.clone(),
            five_hours: empty,
            plan: None,
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
    week_start: String,
    five_hours_ago: String,
    next_monday: String,
}

fn sqlite_ts(dt: &chrono::NaiveDateTime) -> String {
    dt.format("%Y-%m-%d %H:%M:%S").to_string()
}

fn iso_ts(dt: &chrono::NaiveDateTime) -> String {
    dt.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn boundaries() -> Boundaries {
    let now = Utc::now();
    let today = now.date_naive();
    let days_from_monday = today.weekday().num_days_from_monday() as i64;
    let week_start = (today - Duration::days(days_from_monday)).and_time(chrono::NaiveTime::MIN);
    let next_monday =
        (today + Duration::days(7 - days_from_monday)).and_time(chrono::NaiveTime::MIN);
    let five_hours_ago = (now - Duration::hours(5)).naive_utc();
    Boundaries {
        week_start: sqlite_ts(&week_start),
        five_hours_ago: sqlite_ts(&five_hours_ago),
        next_monday: iso_ts(&next_monday),
    }
}

// ── CRUD ────────────────────────────────────────────────────────────

pub fn get_caps(conn: &Connection, key_id: ApiKeyId) -> Result<Option<QuotaCaps>> {
    let caps = conn
        .query_row(
            "SELECT weekly_usd_cap, five_hour_usd_cap \
             FROM api_key_quotas WHERE api_key_id = ?1",
            params![key_id.0],
            |row| {
                Ok(QuotaCaps {
                    weekly_usd_cap: row.get(0)?,
                    five_hour_usd_cap: row.get(1)?,
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
         (api_key_id, weekly_usd_cap, five_hour_usd_cap, updated_at) \
         VALUES (?1, ?2, ?3, datetime('now')) \
         ON CONFLICT(api_key_id) DO UPDATE SET \
           weekly_usd_cap = excluded.weekly_usd_cap, \
           five_hour_usd_cap = excluded.five_hour_usd_cap, \
           updated_at = datetime('now')",
        params![key_id.0, caps.weekly_usd_cap, caps.five_hour_usd_cap],
    )
    .map_err(openproxy_db::error::map_db_error)?;
    Ok(())
}

// ── plan (tier) status ──────────────────────────────────────────────

/// Plan attached to a key: tier name + credited amount + live balance.
/// `None` for keys without a tier (operator/legacy keys).
pub fn plan_status(conn: &Connection, key_id: ApiKeyId) -> Result<Option<PlanStatus>> {
    // The plan's lifetime belongs to the credit lot it funded, not to the key
    // row: a key survives its plan, and `api_keys.expires_at` is the legacy
    // key-expiry column (usually NULL for plan-managed keys). Fall back to it
    // only for keys whose lifetime was set directly.
    let row = conn
        .query_row(
            "SELECT t.name, t.credit_usd, k.expires_at, \
               (SELECT MAX(l.expires_at) FROM credit_transactions l \
                 WHERE l.api_key_id = k.id AND l.status IN ('active', 'expired') \
                   AND l.kind IN ('topup', 'adjust', 'grant') \
                   AND l.expires_at IS NOT NULL) \
             FROM api_keys k JOIN credit_tiers t ON t.id = k.tier_id \
             WHERE k.id = ?1",
            params![key_id.0],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<f64>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(openproxy_db::error::map_db_error)?;
    let Some((name, credit_usd, key_expires_at, lot_expires_at)) = row else {
        return Ok(None);
    };
    let balance_units = crate::credits::ledger::balance_units(conn, key_id)?;
    let balance_usd = balance_units as f64 / 1_000_000.0;
    Ok(Some(PlanStatus {
        name,
        credit_usd: credit_usd.unwrap_or(balance_usd),
        balance_usd,
        expires_at: lot_expires_at.or(key_expires_at).map(|ts| {
            // SQLite UTC stamps; the UI wants ISO-8601.
            if ts.contains('T') {
                ts
            } else {
                format!("{}Z", ts.replace(' ', "T"))
            }
        }),
    }))
}

// ── spend + enforcement ─────────────────────────────────────────────

/// Full status for one key: caps + spend per window + reset info + plan.
pub fn status(conn: &Connection, key_id: ApiKeyId) -> Result<QuotaStatus> {
    let plan = plan_status(conn, key_id)?;
    let Some(caps) = get_caps(conn, key_id)? else {
        return Ok(QuotaStatus {
            plan,
            ..QuotaStatus::unlimited()
        });
    };
    let b = boundaries();
    let (week_spent, five_hour_spent, oldest_5h): (f64, f64, Option<String>) = conn
        .query_row(
            "SELECT \
               COALESCE((SELECT SUM(u.cost_usd) FROM usage u \
                  WHERE u.api_key_id = ?1 AND u.client_response = 1 AND u.created_at >= ?2), 0.0), \
               COALESCE((SELECT SUM(u.cost_usd) FROM usage u \
                  WHERE u.api_key_id = ?1 AND u.client_response = 1 AND u.created_at >= ?3), 0.0), \
               (SELECT MIN(u.created_at) FROM usage u \
                  WHERE u.api_key_id = ?1 AND u.client_response = 1 \
                    AND u.cost_usd > 0 AND u.created_at >= ?3)",
            params![key_id.0, b.week_start, b.five_hours_ago],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(openproxy_db::error::map_db_error)?;
    let week = QuotaWindowStatus {
        cap_usd: caps.weekly_usd_cap,
        spent_usd: week_spent,
        // Only a capped window has something to reset: an uncapped window
        // printing "resets in ..." is exactly the phantom the UI must not show.
        resets_at: caps.weekly_usd_cap.map(|_| b.next_monday),
        oldest_spend_at: None,
    };
    let five_hours = QuotaWindowStatus {
        cap_usd: caps.five_hour_usd_cap,
        spent_usd: five_hour_spent,
        resets_at: None,
        oldest_spend_at: oldest_5h,
    };
    Ok(QuotaStatus {
        enabled: !caps.is_empty(),
        week,
        five_hours,
        plan,
    })
}

/// Pre-flight check. Returns `Some(violation)` when the key already spent
/// its cap in any enabled window (largest window first).
pub fn check(conn: &Connection, key_id: ApiKeyId) -> Result<Option<QuotaViolation>> {
    let st = status(conn, key_id)?;
    if !st.enabled {
        return Ok(None);
    }
    let violated = [(WINDOW_WEEK, &st.week), (WINDOW_FIVE_HOURS, &st.five_hours)]
        .into_iter()
        .find_map(|(name, w)| match w.cap_usd {
            Some(cap) if w.spent_usd >= cap => {
                // The 5h window is rolling: it frees up five hours after the
                // OLDEST spend still inside it, not at a fixed boundary.
                // Deriving the instant lets the client honor a real
                // Retry-After instead of polling blindly.
                let resets_at = w.resets_at.clone().or_else(|| {
                    w.oldest_spend_at.as_ref().map(|oldest| {
                        chrono::NaiveDateTime::parse_from_str(oldest, "%Y-%m-%d %H:%M:%S")
                            .map(|naive| naive + chrono::Duration::hours(5))
                            .map(|t| t.format("%Y-%m-%dT%H:%M:%SZ").to_string())
                            .unwrap_or_else(|_| "rolling window".into())
                    })
                });
                Some(QuotaViolation {
                    window: name,
                    spent_usd: w.spent_usd,
                    cap_usd: cap,
                    resets_at,
                })
            }
            _ => None,
        });
    Ok(violated)
}

#[cfg(test)]
mod tests;
