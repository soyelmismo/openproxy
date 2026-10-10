//! The paid Coding Plan is ONE pool whose windows are conjunctive.
//!
//! `api.z.ai/api/monitor/usage/quota/limit` answers with a `data.limits` array
//! holding several independent windows over the same subscription (session /
//! weekly / monthly). They are not alternatives: the plan is spent as soon as
//! *any* one of them runs out, so the pool's remaining budget is the minimum
//! across the array. Reducing the array with an `any`/`max` would advertise
//! headroom that the subscription does not have, and every request routed on
//! that headroom would be rejected upstream.
//!
//! All figures are reported on a percentage scale, which is the only unit the
//! two upstreams agree on: token counts reset per window and are not
//! comparable across them.

use super::dual::status_pool;
use super::envelope::{as_i64, envelope_is_success, normalize_epoch};
use openproxy_types::quota::{QuotaPool, QuotaPoolStatus, QuotaSource};
use openproxy_types::{CoreError, Result};
use serde_json::Value;

/// One rolling window of a paid subscription.
struct Window {
    /// Percent of the window still available, floored to `0..=100`.
    ///
    /// Flooring rather than rounding is deliberate: a rounded-up remaining
    /// percentage would let a nearly-spent window route one more request than
    /// the subscription authorizes.
    remaining_pct: i64,
    reset_at: Option<i64>,
}

/// The single reason published for every unreadable paid snapshot.
///
/// Fixed and upstream-free: a rejected body is attacker-influenced and may
/// echo the credential that carried the request, so no fragment of it is ever
/// interpolated. The absence of a per-window detail here is the point.
fn unreadable() -> CoreError {
    CoreError::Parse("Z.ai Coding Plan returned an unreadable quota".into())
}

/// Read an epoch sent in seconds or milliseconds, fail-closed.
///
/// A timestamp that is *present but unreadable* is an error rather than
/// `None`: an unreadable expiry is uncertainty about the validity window, and
/// uncertainty must not authorize inference.
fn epoch(value: &Value, key: &str) -> Result<Option<i64>> {
    let Some(raw) = value.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    as_i64(Some(raw))
        .and_then(normalize_epoch)
        .map(Some)
        .ok_or_else(unreadable)
}

/// Remaining share of one window as an integer percent.
///
/// `percentage` is USED percent on the official 0..100 scale, including
/// values below one percent. It is never a remaining fraction. Counters, when
/// provided, are validated separately and can only lower the reported budget.
fn remaining_percent(limit: &Value) -> Option<i64> {
    let percent = match limit.get("percentage").filter(|v| !v.is_null()) {
        Some(value) => {
            let used = value
                .as_f64()
                .filter(|v| v.is_finite() && (0.0..=100.0).contains(v))?;
            Some(100.0 - used)
        }
        None => None,
    };
    let counters = match limit.get("number").filter(|v| !v.is_null()) {
        Some(value) => {
            let number = as_i64(Some(value)).filter(|v| *v > 0)?;
            let used = match limit
                .get("usage")
                .or_else(|| limit.get("used"))
                .or_else(|| limit.get("currentValue"))
            {
                Some(v) => Some(as_i64(Some(v)).filter(|v| *v >= 0)?),
                None => None,
            };
            let remaining = match limit.get("remaining").filter(|v| !v.is_null()) {
                Some(v) => Some(as_i64(Some(v)).filter(|v| *v >= 0 && *v <= number)?),
                None => None,
            };
            let derived = used.map(|v| number.saturating_sub(v).max(0));
            let budget = match (remaining, derived) {
                (Some(r), Some(d)) => r.min(d),
                (Some(r), None) | (None, Some(r)) => r,
                (None, None) => return None,
            };
            Some(budget as f64 * 100.0 / number as f64)
        }
        None => None,
    };
    let remaining = match (percent, counters) {
        (Some(p), Some(c)) => p.min(c),
        (Some(p), None) | (None, Some(p)) => p,
        (None, None) => return None,
    };
    Some(remaining.clamp(0.0, 100.0).floor() as i64)
}

fn parse_window(limit: &Value) -> Result<Window> {
    Ok(Window {
        remaining_pct: remaining_percent(limit).ok_or_else(unreadable)?,
        reset_at: epoch(limit, "nextResetTime")?,
    })
}

/// Every window of the subscription, or a failure.
///
/// An empty or absent array is an error, never an unlimited plan: the
/// subscription list already proved the account *has* a paid plan, so a
/// limits body we cannot read is missing information, and inventing a full
/// budget to fill the gap would route traffic the subscription never
/// authorized.
fn windows(data: &Value) -> Result<Vec<Window>> {
    let limits = data
        .get("limits")
        .and_then(Value::as_array)
        .ok_or_else(unreadable)?;
    if limits.is_empty() {
        return Err(unreadable());
    }
    limits.iter().map(parse_window).collect()
}

/// The subscription the account is actually paying for, if any.
///
/// * `Ok(None)` — the list was read successfully and holds no `VALID`,
///   in-period entry. That is an affirmative statement that no plan is
///   active, which is the only thing that may be published as
///   [`QuotaPoolStatus::Absent`].
/// * `Err(_)` — the list itself is malformed. Absence must be *confirmed*,
///   never assumed from a body we could not read.
fn active_subscription(subscription: &Value) -> Result<Option<&Value>> {
    let data = subscription
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(unreadable)?;
    let mut active = None;
    for entry in data {
        let status = entry
            .get("status")
            .and_then(Value::as_str)
            .ok_or_else(unreadable)?;
        if ![
            "VALID",
            "EXPIRED",
            "INVALID",
            "CANCELLED",
            "CANCELED",
            "INACTIVE",
        ]
        .iter()
        .any(|s| status.eq_ignore_ascii_case(s))
        {
            return Err(unreadable());
        }
        let in_period = match entry.get("inCurrentPeriod") {
            Some(v) => v.as_bool().ok_or_else(unreadable)?,
            None => true,
        };
        if status.eq_ignore_ascii_case("VALID") && in_period && active.is_none() {
            active = Some(entry);
        }
    }
    Ok(active)
}

/// Build the pool from an active subscription and its readable limits body.
///
/// Plan name and per-model detail are delegated to the legacy parser so both
/// quota paths name and decompose a Coding Plan identically; only the
/// aggregation across windows is new here.
fn plan_pool(subscription: &Value, limits: &Value, now: u64) -> Result<QuotaPool> {
    let data = limits
        .get("data")
        .filter(|d| d.is_object())
        .ok_or_else(unreadable)?;
    let windows = windows(data)?;

    // Conjunctive: the tightest window governs the whole pool.
    let binding = windows
        .iter()
        .min_by_key(|w| w.remaining_pct)
        .ok_or_else(unreadable)?;
    let remaining = binding.remaining_pct;
    let reset_at = binding
        .reset_at
        .or_else(|| windows.iter().filter_map(|w| w.reset_at).min());

    let expires_at = epoch(
        active_subscription(subscription)?.ok_or_else(unreadable)?,
        "periodEndTime",
    )?;
    let status = match expires_at {
        Some(expires) if expires as u64 <= now => QuotaPoolStatus::Expired,
        _ if remaining <= 0 => QuotaPoolStatus::Exhausted,
        _ => QuotaPoolStatus::Active,
    };

    let legacy = super::parse_zai_coding_plan_quota(Some(subscription), Some(limits))
        .ok_or_else(unreadable)?;

    Ok(QuotaPool {
        id: "coding-plan".into(),
        source: QuotaSource::CodingPlan,
        plan_name: legacy.plan_name,
        status,
        unit: "percentage".into(),
        used: Some(100 - remaining),
        limit: Some(100),
        remaining: Some(remaining),
        reset_at: reset_at.map(|v| v.to_string()),
        expires_at: expires_at.map(|v| v.to_string()),
        starts_at: None,
        // Empty means the whole catalog, which is exactly the Coding Plan's
        // contract: it is an account-wide entitlement, not a per-model grant.
        model_ids: Vec::new(),
        model_details: legacy.model_details,
        fetch_error: None,
        last_fetched_at: now.to_string(),
    })
}

/// Pools published for the paid Coding Plan source.
///
/// Exactly one pool, always: the plan is account-wide, so there is nothing to
/// partition by. A failed envelope or unreadable window set is an error the
/// caller turns into [`QuotaPoolStatus::Unavailable`]; only a successfully
/// read subscription list with nothing active in it is [`QuotaPoolStatus::Absent`].
pub(crate) fn parse_coding_plan_pools(
    subscription: &Value,
    limits: &Value,
    now: u64,
) -> Result<Vec<QuotaPool>> {
    if !envelope_is_success(subscription) {
        return Err(unreadable());
    }
    if active_subscription(subscription)?.is_none() {
        // Confirmed absence outranks the limits read: with no plan to meter,
        // the limits endpoint legitimately answers "no coding plan" with a
        // failed envelope, and that must not read as an unreadable snapshot.
        return Ok(vec![status_pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Absent,
            None,
            now,
        )]);
    }
    if !envelope_is_success(limits) {
        return Err(unreadable());
    }
    Ok(vec![plan_pool(subscription, limits, now)?])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const NOW: u64 = 1_727_000_000;

    /// Subscription and limits bodies shaped exactly as the two upstreams send
    /// them: `productName`/`periodEndTime` in millis, and a `data.limits`
    /// array carrying the session, weekly and monthly windows at once.
    fn bodies() -> (Value, Value) {
        let subscription = json!({
            "code": 200, "msg": "Operation successful", "success": true,
            "data": [{
                "id": 8888, "productId": "coding-plan-pro",
                "productName": "Z.ai Coding Plan Pro", "status": "VALID",
                "inCurrentPeriod": true,
                "periodStartTime": 1726000000000_u64, "periodEndTime": 1728600000000_u64
            }]
        });
        let limits = json!({
            "code": 0, "success": true,
            "data": {
                "level": "Pro",
                "limits": [
                    { "type": "TIME_LIMIT", "number": 100000000, "usage": 250000,
                      "remaining": 99750000, "percentage": 0.25,
                      "nextResetTime": 1728600000000_u64,
                      "usageDetails": [
                          { "modelCode": "GLM-5.3", "usage": 150000 },
                          { "modelCode": "GLM-5.3-Flash", "usage": 100000 }
                      ] },
                    { "type": "WEEKLY", "number": 500, "usage": 300,
                      "remaining": 200, "nextResetTime": 1729000000000_u64 },
                    { "type": "MONTHLY", "number": 2000, "usage": 1800,
                      "remaining": 200, "nextResetTime": 1730000000000_u64 }
                ]
            }
        });
        (subscription, limits)
    }

    #[test]
    fn one_paid_pool_reports_the_minimum_of_every_window() {
        let (subscription, limits) = bodies();
        let pools = parse_coding_plan_pools(&subscription, &limits, NOW).unwrap();
        assert_eq!(pools.len(), 1);

        let pool = &pools[0];
        assert_eq!(pool.source, QuotaSource::CodingPlan);
        assert_eq!(pool.plan_name.as_deref(), Some("Z.ai Coding Plan Pro"));
        assert_eq!(pool.unit, "percentage");
        assert_eq!(pool.limit, Some(100));
        // Session 99.75%, weekly 40%, monthly 10% -> the monthly window binds.
        assert_eq!(pool.remaining, Some(10));
        assert_eq!(pool.used, Some(90));
        assert_eq!(pool.status, QuotaPoolStatus::Active);
        assert_eq!(pool.reset_at.as_deref(), Some("1730000000"));
        assert_eq!(pool.expires_at.as_deref(), Some("1728600000"));
        assert_eq!(pool.fetch_error, None);
        assert_eq!(pool.last_fetched_at, NOW.to_string());

        // Account-wide: no model list authorizes every catalog model.
        assert!(pool.model_ids.is_empty());
        assert!(pool.is_usable_for_model(NOW, "glm-5.3"));
        assert!(pool.is_usable_for_model(NOW, "glm-5.3-flash"));

        // Per-model detail is preserved from the legacy decomposition.
        let details = pool.model_details.as_deref().expect("details present");
        assert_eq!(details.len(), 2);
        assert_eq!(details[0].model_id, "GLM-5.3");
        assert_eq!(details[0].session_used, 150000);
        assert_eq!(details[1].model_id, "GLM-5.3-Flash");
    }

    #[test]
    fn a_spent_weekly_or_monthly_window_exhausts_the_whole_plan() {
        for spent in ["WEEKLY", "MONTHLY"] {
            let (subscription, mut limits) = bodies();
            let array = limits["data"]["limits"].as_array_mut().unwrap();
            let window = array
                .iter_mut()
                .find(|w| w["type"] == spent)
                .expect("window present");
            window["usage"] = json!(window["number"].as_i64().unwrap());
            window["remaining"] = json!(0);

            let pools = parse_coding_plan_pools(&subscription, &limits, NOW).unwrap();
            assert_eq!(pools[0].remaining, Some(0), "{spent} window binds");
            assert_eq!(pools[0].status, QuotaPoolStatus::Exhausted);
            assert!(!pools[0].is_usable_for_model(NOW, "glm-5.3"));
            // The untouched session window must not be mistaken for headroom.
            assert!(pools[0].reports_exhaustion_for_model(NOW, "glm-5.3"));
        }
    }

    #[test]
    fn an_active_plan_past_its_period_is_expired_not_exhausted() {
        let (subscription, limits) = bodies();
        let pools = parse_coding_plan_pools(&subscription, &limits, 1_728_600_000).unwrap();
        assert_eq!(pools[0].status, QuotaPoolStatus::Expired);
        assert_eq!(pools[0].remaining, Some(10));
    }

    #[test]
    fn confirmed_absence_survives_a_limits_body_that_reports_no_plan() {
        let subscription =
            json!({"code":200,"msg":"Operation successful","data":[],"success":true});
        // With no plan to meter the limits endpoint answers "no coding plan".
        let limits = json!({"code":500,"success":false,"data":null,"msg":"no coding plan"});
        let pools = parse_coding_plan_pools(&subscription, &limits, NOW).unwrap();
        assert_eq!(pools.len(), 1);
        assert_eq!(pools[0].status, QuotaPoolStatus::Absent);
        assert_eq!(pools[0].remaining, None);
        assert_eq!(pools[0].fetch_error, None);
    }

    #[test]
    fn absence_is_only_ever_claimed_from_a_readable_subscription_list() {
        // Expired/inactive entries are still an affirmative "no active plan".
        let lapsed = json!({"code":200,"success":true,"data":[
            {"productName":"Z.ai Coding Plan Pro","status":"EXPIRED","inCurrentPeriod":true}]});
        assert_eq!(
            parse_coding_plan_pools(&lapsed, &bodies().1, NOW).unwrap()[0].status,
            QuotaPoolStatus::Absent
        );

        // A failed subscription envelope, or one with no list at all, proves
        // nothing about the plan and must fail closed instead.
        for subscription in [
            json!({"code":401,"success":false,"msg":"unauthorized"}),
            json!({"code":500,"success":false}),
            json!({"code":200,"success":true}),
            json!({"code":200,"success":true,"data":[{}]}),
            json!({"code":200,"success":true,"data":[{"status":"UNKNOWN"}]}),
            json!({"code":200,"success":true,"data":[{"status":"VALID","inCurrentPeriod":"bad"}]}),
            json!([]),
        ] {
            assert!(
                parse_coding_plan_pools(&subscription, &bodies().1, NOW).is_err(),
                "{subscription} must not read as absent"
            );
        }
    }

    #[test]
    fn an_active_plan_with_unreadable_windows_is_never_unlimited() {
        let (subscription, _) = bodies();
        for data in [
            json!({"level":"Pro"}),
            json!({"level":"Pro","limits":[]}),
            json!({"level":"Pro","limits":[{"type":"WEEKLY"}]}),
            json!({"level":"Pro","limits":[{"type":"WEEKLY","number":0,"usage":0}]}),
            json!({"level":"Pro","limits":[{"type":"WEEKLY","number":500,"usage":"soon"}]}),
        ] {
            let limits = json!({"code":0,"success":true,"data":data});
            assert!(
                parse_coding_plan_pools(&subscription, &limits, NOW).is_err(),
                "{data} must not invent a full budget"
            );
        }
    }

    #[test]
    fn a_failed_limits_envelope_is_unreadable_rather_than_absent() {
        let (subscription, _) = bodies();
        for limits in [
            json!({"code":500,"success":false,"msg":"internal error"}),
            json!({"code":401,"success":false}),
            json!([]),
            json!({"code":200,"success":true,"data":[]}),
        ] {
            assert!(
                parse_coding_plan_pools(&subscription, &limits, NOW).is_err(),
                "{limits} must not read as absent"
            );
        }
    }

    #[test]
    fn official_percentage_is_used_percent_not_remaining_or_a_fraction() {
        for (used, remaining) in [(0.0, 100), (0.25, 99), (1.0, 99), (80.0, 20), (100.0, 0)] {
            assert_eq!(
                remaining_percent(&json!({"percentage":used})),
                Some(remaining)
            );
        }
        assert_eq!(
            remaining_percent(&json!({"percentage":0,"number":100,"usage":100,"remaining":0})),
            Some(0)
        );
        for value in [
            json!({"percentage":-1}),
            json!({"percentage":101}),
            json!({"percentage":"invalid"}),
            json!({"number":100,"usage":-1}),
        ] {
            assert_eq!(remaining_percent(&value), None);
        }
    }

    #[test]
    fn token_counters_and_percentages_agree_on_the_same_window() {
        let (subscription, mut limits) = bodies();
        limits["data"]["limits"][1]["percentage"] = json!(60);
        let pools = parse_coding_plan_pools(&subscription, &limits, NOW).unwrap();
        assert_eq!(pools[0].remaining, Some(10));

        // A smaller explicit remaining counter is conservative, even if the
        // snapshot contains contradictory redundant usage counters.
        limits["data"]["limits"][2]["remaining"] = json!(180);
        assert_eq!(
            parse_coding_plan_pools(&subscription, &limits, NOW).unwrap()[0].remaining,
            Some(9)
        );
    }
}
