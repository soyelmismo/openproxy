//! ZCode billing/balance is a different product from the paid Coding Plan.
use super::dual::status_pool;
use super::envelope::{as_i64, envelope_is_success, normalize_epoch};
use openproxy_types::quota::{QuotaPool, QuotaPoolStatus, QuotaSource};
use openproxy_types::{CoreError, Result};
use serde_json::Value;

fn invalid() -> CoreError {
    CoreError::Parse("ZCode Starter returned an unreadable balance".into())
}
fn text<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key)?
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
}
fn epoch(v: &Value, key: &str) -> Result<Option<i64>> {
    let Some(raw) = v.get(key).filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    as_i64(Some(raw))
        .and_then(normalize_epoch)
        .map(Some)
        .ok_or_else(invalid)
}
fn models(bucket: &Value, entitlement: Option<&Value>) -> Result<Vec<String>> {
    let caps = bucket
        .get("capabilities")
        .or_else(|| entitlement.and_then(|e| e.get("capabilities")))
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let mut ids = Vec::new();
    for cap in caps {
        let cap = cap.as_str().ok_or_else(invalid)?;
        if let Some(model) = cap
            .strip_prefix("model:")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            && !ids.iter().any(|id: &String| id.eq_ignore_ascii_case(model))
        {
            ids.push(model.to_owned());
        }
    }
    if ids.is_empty() {
        return Err(invalid());
    }
    Ok(ids)
}

pub(crate) fn parse_zcode_pools(json: &Value, now: u64) -> Result<Vec<QuotaPool>> {
    if !envelope_is_success(json) {
        return Err(invalid());
    }
    let data = json.get("data").ok_or_else(invalid)?;
    let plans = data
        .get("plans")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    let balances = data
        .get("balances")
        .and_then(Value::as_array)
        .ok_or_else(invalid)?;
    if plans.is_empty() && balances.is_empty() {
        return Ok(vec![status_pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Absent,
            None,
            now,
        )]);
    }
    if plans.is_empty() || balances.is_empty() {
        return Err(invalid());
    }
    balances
        .iter()
        .map(|bucket| parse_bucket(bucket, plans, now))
        .collect()
}

fn parse_bucket(bucket: &Value, plans: &[Value], now: u64) -> Result<QuotaPool> {
    let user_plan = text(bucket, "user_plan_id").ok_or_else(invalid)?;
    let plan = plans
        .iter()
        .find(|p| text(p, "user_plan_id") == Some(user_plan))
        .ok_or_else(invalid)?;
    let plan_id = text(plan, "plan_id").ok_or_else(invalid)?;
    if text(bucket, "plan_id") != Some(plan_id) {
        return Err(invalid());
    }
    let entitlement_id = text(bucket, "entitlement_id").ok_or_else(invalid)?;
    let entitlement = plan
        .get("entitlements")
        .and_then(Value::as_array)
        .and_then(|all| {
            all.iter()
                .find(|e| text(e, "entitlement_id") == Some(entitlement_id))
        });
    let model_ids = models(bucket, entitlement)?;
    let total = as_i64(bucket.get("total_units"))
        .filter(|v| *v >= 0)
        .ok_or_else(invalid)?;
    let used = as_i64(bucket.get("used_units"))
        .filter(|v| *v >= 0)
        .ok_or_else(invalid)?;
    let remaining = as_i64(bucket.get("remaining_units"))
        .filter(|v| *v >= 0)
        .ok_or_else(invalid)?;
    let available = match bucket.get("available_units") {
        Some(v) => as_i64(Some(v)).filter(|v| *v >= 0).ok_or_else(invalid)?,
        None => remaining,
    };
    if used > total
        || remaining > total
        || available > remaining
        || used.checked_add(remaining) != Some(total)
    {
        return Err(invalid());
    }
    let mut ends = Vec::new();
    for (value, key) in [
        (plan, "ends_at"),
        (bucket, "expires_at"),
        (bucket, "period_end"),
    ] {
        if let Some(end) = epoch(value, key)? {
            ends.push(end);
        }
    }
    let expires = ends.into_iter().min().ok_or_else(invalid)?;
    let mut starts = Vec::new();
    for (value, key) in [(plan, "starts_at"), (bucket, "period_start")] {
        if let Some(start) = epoch(value, key)? {
            starts.push(start);
        }
    }
    if let Some(e) = entitlement
        && let Some(start) = epoch(e, "effective_at")?
    {
        starts.push(start);
    }
    let starts_at = starts.into_iter().max();
    if starts_at.is_some_and(|s| s >= expires) {
        return Err(invalid());
    }
    let raw_status = text(plan, "status").ok_or_else(invalid)?;
    let status = if expires as u64 <= now || raw_status.eq_ignore_ascii_case("expired") {
        QuotaPoolStatus::Expired
    } else if !raw_status.eq_ignore_ascii_case("active") {
        return Err(invalid());
    } else if available == 0 {
        QuotaPoolStatus::Exhausted
    } else {
        QuotaPoolStatus::Active
    };
    let unit = text(bucket, "unit_type").ok_or_else(invalid)?;
    if !matches!(unit, "token" | "tokens") {
        return Err(invalid());
    }
    Ok(QuotaPool {
        id: text(bucket, "bucket_id").ok_or_else(invalid)?.to_owned(),
        source: QuotaSource::ZcodeStarter,
        plan_name: text(plan, "name").map(ToOwned::to_owned),
        status,
        unit: unit.to_owned(),
        used: Some(used),
        limit: Some(total),
        remaining: Some(available),
        reset_at: None,
        expires_at: Some(expires.to_string()),
        starts_at: starts_at.map(|v| v.to_string()),
        model_ids,
        model_details: None,
        fetch_error: None,
        last_fetched_at: now.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn balance() -> Value {
        json!({"code":0,"data":{"plans":[{"user_plan_id":"plan-user","plan_id":"starter","name":"Weekend Build",
            "status":"active","starts_at":500,"ends_at":2000,"entitlements":[{"entitlement_id":"ent","effective_at":800,
            "capabilities":["model:glm-5.3-flash"]}]}],"balances":[{"bucket_id":"bucket","user_plan_id":"plan-user",
            "plan_id":"starter","entitlement_id":"ent","unit_type":"token","capabilities":["model:glm-5.3-flash"],
            "total_units":300000000,"used_units":25718,"remaining_units":299974282,"available_units":299974282,
            "period_start":500,"period_end":2000,"expires_at":2000}]}})
    }
    #[test]
    fn genuine_starter_balance_preserves_exact_tokens_and_capabilities() {
        let pools = parse_zcode_pools(&balance(), 1000).unwrap();
        let p = &pools[0];
        assert_eq!(p.remaining, Some(299974282));
        assert_eq!(p.used, Some(25718));
        assert_eq!(p.plan_name.as_deref(), Some("Weekend Build"));
        assert!(p.is_usable_for_model(1000, "GLM-5.3-Flash"));
        assert!(!p.is_usable_for_model(1000, "glm-5.3"));
        assert!(!p.is_usable_for_model(700, "glm-5.3-flash"));
        assert_eq!(
            parse_zcode_pools(&balance(), 2000).unwrap()[0].status,
            QuotaPoolStatus::Expired
        );
    }
    #[test]
    fn absent_requires_both_complete_empty_lists() {
        assert_eq!(
            parse_zcode_pools(&json!({"data":{"plans":[],"balances":[]}}), 1000).unwrap()[0].status,
            QuotaPoolStatus::Absent
        );
        for v in [
            json!({"data":{"plans":[]}}),
            json!({"success":false,"data":{"plans":[],"balances":[]}}),
            json!({}),
        ] {
            assert!(parse_zcode_pools(&v, 1000).is_err());
        }
    }
    #[test]
    fn malformed_unknown_and_inconsistent_data_fail_closed() {
        for (key, value) in [
            ("capabilities", json!([])),
            ("used_units", json!(-1)),
            ("expires_at", json!("tomorrow")),
            ("user_plan_id", json!("other")),
            ("remaining_units", json!(300000001)),
        ] {
            let mut v = balance();
            v["data"]["balances"][0][key] = value;
            assert!(parse_zcode_pools(&v, 1000).is_err(), "{key}");
        }
    }
    #[test]
    fn exhausted_reserved_and_numeric_strings_are_explicit() {
        let mut v = balance();
        v["data"]["balances"][0]["available_units"] = json!(0);
        assert_eq!(
            parse_zcode_pools(&v, 1000).unwrap()[0].status,
            QuotaPoolStatus::Exhausted
        );
        v["data"]["balances"][0]["available_units"] = json!("299974282");
        assert!(parse_zcode_pools(&v, 1000).unwrap()[0].is_usable_for_model(1000, "glm-5.3-flash"));
    }
}
