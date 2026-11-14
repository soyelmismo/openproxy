//! Tests for per-API-key dollar quotas (5h + week) and the plan status block.
//!
//! Spend rows are inserted directly into `usage` with explicit `created_at`
//! (SQLite "YYYY-MM-DD HH:MM:SS" UTC) so the window boundaries are stable
//! regardless of test runtime.

use super::*;
use crate::api_keys;
use openproxy_types::ids::ApiKeyId as ApiIdAlias;
use rusqlite::params;

fn fresh_conn() -> Connection {
    openproxy_db::testing::open_in_memory()
}

fn make_key(conn: &Connection, label: &str) -> i64 {
    let input = api_keys::CreateApiKeyInput {
        label: Some(label.to_string()),
        scopes: vec!["chat".to_string()],
        ..Default::default()
    };
    let (key, _plaintext) = api_keys::create(conn, input, "test").expect("create key");
    key.id.0
}

/// Insert a served-response usage row (`client_response = 1`) with a given
/// cost and age offset (seconds before now).
fn insert_spend(conn: &Connection, key_id: i64, cost: f64, age_secs: i64) {
    let created = (chrono::Utc::now() - chrono::Duration::seconds(age_secs))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    conn.execute(
        "INSERT INTO usage (request_id, trace_id, provider_id, upstream_model_id, \
          status_code, cost_usd, client_response, api_key_id, created_at) \
         VALUES (?1, ?1, 'test', 'test-model', 200, ?2, 1, ?3, ?4)",
        params![
            format!("req-{key_id}-{cost}-{age_secs}"),
            cost,
            key_id,
            created
        ],
    )
    .expect("insert usage");
}

/// Insert an attempt row that never reached the client.
fn insert_failed_attempt(conn: &Connection, key_id: i64, cost: f64) {
    conn.execute(
        "INSERT INTO usage (request_id, trace_id, provider_id, upstream_model_id, \
          status_code, cost_usd, client_response, api_key_id) \
         VALUES (?1, ?1, 'test', 'test-model', 500, ?2, 0, ?3)",
        params![format!("req-fail-{key_id}-{cost}"), cost, key_id],
    )
    .expect("insert usage");
}

/// Attach a plan (tier) to a key, with an expiry and a credited balance.
fn attach_plan(conn: &Connection, key_id: i64, name: &str, credit_usd: f64, ttl_days: i64) {
    let config = crate::credits::TierConfig {
        weekly_usd_cap: None,
        five_hour_usd_cap: None,
        allowed_models: None,
        allowed_providers: None,
        allowed_combos: None,
        only_whitelisted: None,
    };
    let plan = crate::credits::PlanCommerce {
        price_usd: Some(1.0),
        credit_usd: Some(credit_usd),
        ttl_days,
    };
    let tier = crate::credits::tiers::create(conn, name, &config, &plan).expect("create tier");
    conn.execute(
        "UPDATE api_keys SET tier_id = ?1, expires_at = datetime('now', ?2) WHERE id = ?3",
        params![tier.id, format!("+{ttl_days} days"), key_id],
    )
    .expect("attach tier");
    let units = (credit_usd * 1_000_000.0) as i64;
    let order = crate::credits::orders::create_order(
        conn,
        Some(ApiIdAlias(key_id)),
        None,
        units,
        ttl_days,
        None,
    )
    .expect("create order");
    crate::credits::ledger::credit_topup(
        conn,
        ApiIdAlias(key_id),
        order.id,
        units,
        ttl_days,
        "0xtest",
    )
    .expect("credit topup");
}

/// Anchor the fixed 5h window at an explicit age (seconds before now), so
/// the tests control the window boundaries exactly.
fn anchor_window(conn: &Connection, key_id: i64, started_secs_ago: i64) {
    conn.execute(
        "UPDATE api_key_quotas SET five_hour_started_at = datetime('now', ?1) \
         WHERE api_key_id = ?2",
        params![format!("-{started_secs_ago} seconds"), key_id],
    )
    .expect("anchor window");
}

#[test]
fn no_row_means_unlimited() {
    let conn = fresh_conn();
    let key = make_key(&conn, "no-quota");
    let caps = get_caps(&conn, ApiIdAlias(key)).expect("ok");
    assert!(caps.is_none());
    let st = status(&conn, ApiIdAlias(key)).expect("ok");
    assert!(!st.enabled);
    assert!(st.plan.is_none());
    assert!(matches!(
        check(&conn, ApiIdAlias(key)).expect("ok"),
        QuotaCheck::Allow
    ));
}

#[test]
fn upsert_roundtrip() {
    let conn = fresh_conn();
    let key = make_key(&conn, "quota");
    let caps = QuotaCaps {
        weekly_usd_cap: None,
        five_hour_usd_cap: Some(1.5),
    };
    upsert(&conn, ApiIdAlias(key), &caps).expect("upsert");
    let stored = get_caps(&conn, ApiIdAlias(key)).expect("get").expect("row");
    assert_eq!(stored, caps);
    // Re-upsert replaces (idempotent).
    let caps2 = QuotaCaps {
        weekly_usd_cap: Some(10.0),
        five_hour_usd_cap: None,
    };
    upsert(&conn, ApiIdAlias(key), &caps2).expect("upsert");
    let stored = get_caps(&conn, ApiIdAlias(key)).expect("get").expect("row");
    assert_eq!(stored, caps2);
    conn.execute(
        "DELETE FROM api_key_quotas WHERE api_key_id = ?1",
        params![key],
    )
    .expect("delete");
    assert!(get_caps(&conn, ApiIdAlias(key)).expect("get").is_none());
}

#[test]
fn spend_counts_only_inside_the_anchored_window() {
    let conn = fresh_conn();
    let key = make_key(&conn, "spend");
    upsert(
        &conn,
        ApiIdAlias(key),
        &QuotaCaps {
            weekly_usd_cap: Some(5.0),
            five_hour_usd_cap: Some(1.0),
        },
    )
    .expect("upsert");
    // The window opened 2h ago: it covers [now-2h, now+3h).
    anchor_window(&conn, key, 2 * 3600);
    insert_spend(&conn, key, 1.20, 6 * 3600); // 6h ago: BEFORE the anchor
    insert_spend(&conn, key, 0.80, 3600); // 1h ago: inside the window
    insert_failed_attempt(&conn, key, 99.0); // never served: excluded
    let st = status(&conn, ApiIdAlias(key)).expect("ok");
    assert!(st.enabled);
    assert!((st.week.spent_usd - 2.0).abs() < 1e-9, "the week saw both");
    assert!(
        (st.five_hours.spent_usd - 0.80).abs() < 1e-9,
        "the fixed window counts from the anchor on, got {}",
        st.five_hours.spent_usd
    );
    // The oldest spend still counting is the 1h-old one: 6h-old spend
    // predates the anchor and is outside the window.
    let oldest = chrono::NaiveDateTime::parse_from_str(
        &st.five_hours.oldest_spend_at.expect("the anchored oldest spend"),
        "%Y-%m-%d %H:%M:%S",
    )
    .expect("sqlite timestamp");
    let age = chrono::Utc::now().naive_utc().timestamp() - oldest.timestamp();
    assert!(
        (3_000..=4_200).contains(&age),
        "oldest spend ≈ 1h old, got {age}s"
    );
    // 0.80 < 1.0 cap -> allowed.
    assert!(matches!(
        check(&conn, ApiIdAlias(key)).expect("ok"),
        QuotaCheck::Allow
    ));
}

#[test]
fn check_reports_violated_window() {
    let conn = fresh_conn();
    let key = make_key(&conn, "violation");
    upsert(
        &conn,
        ApiIdAlias(key),
        &QuotaCaps {
            weekly_usd_cap: None,
            five_hour_usd_cap: Some(0.50),
        },
    )
    .expect("upsert");
    // The window opened 30 minutes ago and the spend exhausted it.
    anchor_window(&conn, key, 30 * 60);
    insert_spend(&conn, key, 0.75, 60);
    let QuotaCheck::Violated(v) = check(&conn, ApiIdAlias(key)).expect("ok") else {
        panic!("expected a violation");
    };
    assert_eq!(v.window, WINDOW_FIVE_HOURS);
    assert!((v.cap_usd - 0.50).abs() < 1e-9);
    assert!((v.spent_usd - 0.75).abs() < 1e-9);
    // Fixed window: the whole cap frees at anchor + 5h, i.e. ~4h30 from now.
    let resets = v.resets_at.expect("the anchored reset instant");
    let t = chrono::DateTime::parse_from_rfc3339(&resets).expect("iso instant");
    let secs = t.timestamp() - chrono::Utc::now().timestamp();
    assert!(
        (16_100..=16_300).contains(&secs),
        "anchor (30m ago) + 5h ≈ {secs}s from now"
    );
}

#[test]
fn the_first_request_opens_the_window_and_the_expired_one_frees_the_cap() {
    let conn = fresh_conn();
    let key = make_key(&conn, "anchor");
    upsert(
        &conn,
        ApiIdAlias(key),
        &QuotaCaps {
            weekly_usd_cap: None,
            five_hour_usd_cap: Some(0.50),
        },
    )
    .expect("upsert");
    // Exhausted spend inside a window that already closed: at 10:05 the cap
    // was gone, the window that opened at 10:00 expires at 15:00.
    anchor_window(&conn, key, 6 * 3600);
    insert_spend(&conn, key, 0.75, 60 * 60);
    // The expired window does not run a countdown and does not reject: the
    // caller re-anchors and the fresh window starts at zero.
    assert!(
        matches!(
            check(&conn, ApiIdAlias(key)).expect("ok"),
            QuotaCheck::ReanchorNeeded
        ),
        "an expired window must ask for a re-anchor"
    );
    assert!(
        anchor_five_hour_window(&conn, ApiIdAlias(key)).expect("ok"),
        "the expired window re-anchored"
    );
    assert!(
        !anchor_five_hour_window(&conn, ApiIdAlias(key)).expect("ok"),
        "a live window is never re-anchored twice"
    );
    assert!(matches!(
        check(&conn, ApiIdAlias(key)).expect("ok"),
        QuotaCheck::Allow
    ));
}

#[test]
fn a_key_without_requests_has_no_running_window() {
    let conn = fresh_conn();
    let key = make_key(&conn, "idle");
    upsert(
        &conn,
        ApiIdAlias(key),
        &QuotaCaps {
            weekly_usd_cap: None,
            five_hour_usd_cap: Some(0.50),
        },
    )
    .expect("upsert");
    // No anchor: nothing counts and nothing runs — the first request opens it.
    let st = status(&conn, ApiIdAlias(key)).expect("ok");
    assert!((st.five_hours.spent_usd - 0.0).abs() < 1e-9);
    assert!(st.five_hours.resets_at.is_none(), "no window, no countdown");
    assert!(
        matches!(
            check(&conn, ApiIdAlias(key)).expect("ok"),
            QuotaCheck::ReanchorNeeded
        ),
        "the check asks the caller to open the window"
    );
    assert!(anchor_five_hour_window(&conn, ApiIdAlias(key)).expect("ok"));
    let st = status(&conn, ApiIdAlias(key)).expect("ok");
    assert!(st.five_hours.resets_at.is_some(), "the window now runs");
}

#[test]
fn week_violation_carries_reset_time() {
    let conn = fresh_conn();
    let key = make_key(&conn, "week");
    upsert(
        &conn,
        ApiIdAlias(key),
        &QuotaCaps {
            weekly_usd_cap: Some(1.0),
            five_hour_usd_cap: None,
        },
    )
    .expect("upsert");
    insert_spend(&conn, key, 1.0, 10);
    let QuotaCheck::Violated(v) = check(&conn, ApiIdAlias(key)).expect("ok") else {
        panic!("expected a violation");
    };
    assert_eq!(v.window, WINDOW_WEEK);
    assert!(v.resets_at.is_some());
    // ISO-8601 shape: YYYY-MM-DDTHH:MM:SSZ
    let r = v.resets_at.unwrap();
    assert_eq!(r.len(), 20);
    assert!(r.ends_with('Z'));
}

#[test]
fn uncapped_windows_report_no_reset_and_disabled() {
    let conn = fresh_conn();
    let key = make_key(&conn, "uncapped");
    upsert(
        &conn,
        ApiIdAlias(key),
        &QuotaCaps {
            weekly_usd_cap: None,
            five_hour_usd_cap: None,
        },
    )
    .expect("upsert");
    let st = status(&conn, ApiIdAlias(key)).expect("ok");
    // No effective cap: nothing to count down, panel must not claim a reset.
    assert!(!st.enabled);
    assert!(st.week.resets_at.is_none());
    assert!(st.five_hours.resets_at.is_none());
    assert!(matches!(
        check(&conn, ApiIdAlias(key)).expect("ok"),
        QuotaCheck::Allow
    ));
}

#[test]
fn plan_status_reports_balance_credit_and_expiry() {
    let conn = fresh_conn();
    let key = make_key(&conn, "plan");
    attach_plan(&conn, key, "Lite", 3.0, 7);
    let st = status(&conn, ApiIdAlias(key)).expect("ok");
    let plan = st.plan.expect("plan present");
    assert_eq!(plan.name, "Lite");
    assert!((plan.credit_usd - 3.0).abs() < 1e-9);
    assert!((plan.balance_usd - 3.0).abs() < 1e-9);
    let exp = plan.expires_at.expect("expiry");
    assert!(exp.ends_with('Z') && exp.contains('T'), "ISO-8601: {exp}");
}

#[test]
fn plan_balance_follows_spend() {
    let conn = fresh_conn();
    let key = make_key(&conn, "plan-spend");
    attach_plan(&conn, key, "Lite", 3.0, 7);
    crate::credits::ledger::record_spend(&conn, ApiIdAlias(key), 1_000_000).expect("spend");
    let st = status(&conn, ApiIdAlias(key)).expect("ok");
    let plan = st.plan.expect("plan present");
    assert!(
        (plan.balance_usd - 2.0).abs() < 1e-9,
        "balance after $1 spend"
    );
}

#[test]
fn plan_expiry_comes_from_the_lot_not_the_key() {
    // The key survives its plan: `api_keys.expires_at` is NULL for
    // plan-managed keys, and the countdown must still come from the credit.
    let conn = fresh_conn();
    let key = make_key(&conn, "plan-lot-expiry");
    attach_plan(&conn, key, "Lite", 3.0, 7);
    conn.execute(
        "UPDATE api_keys SET expires_at = NULL WHERE id = ?1",
        params![key],
    )
    .expect("clear key expiry");
    let st = status(&conn, ApiIdAlias(key)).expect("ok");
    let plan = st.plan.expect("plan present");
    assert!(
        plan.expires_at.is_some(),
        "plan expiry must come from the active credit lot"
    );
}

#[test]
fn assigning_a_plan_grants_its_leveraged_credit_once() {
    use crate::credits::tiers::{self, PlanCommerce, TierConfig};

    let conn = fresh_conn();
    let key = make_key(&conn, "plan-grant");
    let tier = tiers::create(
        &conn,
        "Lite",
        &TierConfig::default(),
        &PlanCommerce {
            price_usd: Some(5.0),
            credit_usd: Some(15.0),
            ttl_days: 7,
        },
    )
    .expect("create tier");
    tiers::set_key_tier(&conn, ApiIdAlias(key), Some(tier.id)).expect("link tier");

    assert!(
        tiers::grant_plan(&conn, ApiIdAlias(key), &tier).expect("grant"),
        "first assignment credits the plan"
    );
    let st = status(&conn, ApiIdAlias(key)).expect("ok");
    let plan = st.plan.expect("plan present");
    assert_eq!(plan.credit_usd, 15.0, "leverage: 5 paid -> 15 credit");
    assert_eq!(plan.balance_usd, 15.0);
    assert!(plan.expires_at.is_some(), "the grant carries the plan TTL");

    // Re-saving the same plan is idempotent: no second lot, no double credit.
    assert!(
        !tiers::grant_plan(&conn, ApiIdAlias(key), &tier).expect("re-grant"),
        "same plan already live"
    );
    assert_eq!(
        crate::credits::ledger::balance_units(&conn, ApiIdAlias(key)).expect("balance"),
        15_000_000
    );
}

#[test]
fn a_new_plan_supersedes_the_previous_grant() {
    use crate::credits::tiers::{self, PlanCommerce, TierConfig};

    let conn = fresh_conn();
    let key = make_key(&conn, "plan-swap");
    let make = |name: &str, credit: f64| {
        tiers::create(
            &conn,
            name,
            &TierConfig::default(),
            &PlanCommerce {
                price_usd: Some(5.0),
                credit_usd: Some(credit),
                ttl_days: 7,
            },
        )
        .expect("create tier")
    };
    let lite = make("Lite", 15.0);
    let pro = make("Pro", 30.0);

    tiers::grant_plan(&conn, ApiIdAlias(key), &lite).expect("grant lite");
    tiers::set_key_tier(&conn, ApiIdAlias(key), Some(pro.id)).expect("link pro");
    assert!(tiers::grant_plan(&conn, ApiIdAlias(key), &pro).expect("grant pro"));

    let plan = status(&conn, ApiIdAlias(key))
        .expect("ok")
        .plan
        .expect("plan");
    assert_eq!(plan.name, "Pro");
    assert_eq!(
        plan.balance_usd, 30.0,
        "the old grant is expired, not stacked"
    );
}
