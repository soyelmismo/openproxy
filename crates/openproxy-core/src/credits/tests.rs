//! Tests for the prepaid credits module.
//!
//! The money-path guarantees under test:
//! 1. A payment log is consumed exactly once, no matter how often it is
//!    delivered or how many threads deliver it.
//! 2. A given amount can only pay one order; concurrent duplicates lose.
//! 3. Balance can never go negative and never double-credits.
//! 4. Expiry kills the remaining balance with no refund.
//! 5. Anonymous reveal happens exactly once.

use rusqlite::params;
use crate::credits::ledger::{self, record_spend};
use crate::credits::matcher::{self, PaymentOutcome};
use crate::credits::orders;
use openproxy_db::secrets::{MasterKey};
use openproxy_types::ids::ApiKeyId;
use rusqlite::Connection;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

const USD: i64 = super::UNITS_PER_USD;

fn test_conn() -> Connection {
    let mut conn = Connection::open_in_memory().expect("open memory db");
    openproxy_db::migrations::run(&mut conn).expect("run migrations");
    conn
}

/// Insert a bare API key row so key-scoped queries have a real key to point
/// at. Returns its id.
fn seed_key(conn: &Connection, label: &str) -> ApiKeyId {
    conn.execute(
        "INSERT INTO api_keys (key_hash, label) VALUES (?1, ?2)",
        rusqlite::params![format!("hash-{label}"), label],
    )
    .expect("seed key");
    ApiKeyId(conn.last_insert_rowid())
}

fn mk() -> MasterKey {
    MasterKey::generate().expect("generate master key")
}

// ── jitter / order allocation ───────────────────────────────────────

#[test]
fn create_order_jitters_within_six_decimals_and_credits_base() {
    let conn = test_conn();
    let key = seed_key(&conn, "jit1");
    let order = orders::create_order(&conn, Some(key), None, 10 * USD, 15, None).expect("create order");
    assert_eq!(order.base_units, 10 * USD);
    assert!(order.amount_units >= 10 * USD);
    assert!(order.amount_units < 11 * USD, "jitter must stay below one dollar");
    assert_eq!(order.status, "pending");
    assert_eq!(order.ttl_days, 15);
    // Public code shape: OP-XXXX-XXXX
    assert_eq!(order.order_code.len(), 12);
    assert!(order.order_code.starts_with("OP-"));
}

#[test]
fn create_order_rejects_non_whole_dollar_base() {
    let conn = test_conn();
    let key = seed_key(&conn, "jit2");
    assert!(orders::create_order(&conn, Some(key), None, 10 * USD + 1, 15, None).is_err());
    assert!(orders::create_order(&conn, Some(key), None, 0, 15, None).is_err());
    assert!(orders::create_order(&conn, Some(key), None, 10 * USD, 0, None).is_err());
}

#[test]
fn live_orders_never_share_an_amount() {
    // 2000 sequential creations inside one dollar of base: the unique index
    // must hand out 2000 distinct amounts out of 1M possible jitters.
    let conn = test_conn();
    let key = seed_key(&conn, "jit3");
    let mut seen = std::collections::HashSet::new();
    for _ in 0..2000 {
        let o = orders::create_order(&conn, Some(key), None, 1 * USD, 15, None).expect("create");
        assert!(seen.insert(o.amount_units), "duplicate live amount {}", o.amount_units);
    }
    assert_eq!(seen.len(), 2000);
}

#[test]
fn concurrent_order_creation_never_collides() {
    // Four writers racing on the same DB file: the partial unique index is
    // the arbiter, and every retry loop still lands a distinct amount.
    let dir = std::env::temp_dir().join(format!("op-credits-jit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("credits.db");
    {
        let mut c = Connection::open(&path).expect("open");
        openproxy_db::migrations::run(&mut c).expect("migrate");
        seed_key(&c, "jit4");
    }

    let created = Arc::new(AtomicI64::new(0));
    let mut handles = Vec::new();
    for _ in 0..4 {
        let path = path.clone();
        let created = Arc::clone(&created);
        handles.push(std::thread::spawn(move || {
            let conn = Connection::open(&path).expect("open writer");
            conn.busy_timeout(std::time::Duration::from_secs(10)).ok();
            for _ in 0..50 {
                match orders::create_order(&conn, Some(ApiKeyId(1)), None, 5 * USD, 15, None) {
                    Ok(_) => {
                        created.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(e) => panic!("create failed: {e}"),
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("join writer");
    }
    assert_eq!(created.load(Ordering::SeqCst), 200);

    let conn = Connection::open(&path).expect("open reader");
    let total: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM credit_orders WHERE status = 'pending'",
            [],
            |r| r.get(0),
        )
        .expect("count");
    let distinct: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT amount_units) FROM credit_orders",
            [],
            |r| r.get(0),
        )
        .expect("count distinct");
    assert_eq!(total, 200);
    assert_eq!(distinct, 200);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── payment matching: idempotency + CAS ─────────────────────────────

fn paid_order(conn: &Connection, key: ApiKeyId, usd: i64, ttl: i64) -> orders::CreditOrder {
    orders::create_order(conn, Some(key), None, usd * USD, ttl, None).expect("create order")
}

#[test]
fn payment_credits_exactly_once_and_is_idempotent() {
    let conn = test_conn();
    let key = seed_key(&conn, "pay1");
    let mk = mk();
    let order = paid_order(&conn, key, 10, 15);

    let first = matcher::apply_payment(
        &conn,
        &mk,
        "0xdeadbeef",
        3,
        1000,
        order.amount_units,
        Some("0xbuyer"),
    )
    .expect("apply");
    match first {
        PaymentOutcome::Credited {
            credited_units,
            api_key_id,
            ..
        } => {
            assert_eq!(credited_units, 10 * USD);
            assert_eq!(api_key_id, key.0);
        }
        other => panic!("expected credited, got {other:?}"),
    }
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 10 * USD);

    // Same log delivered again (restart, re-scan, duplicate delivery).
    let again = matcher::apply_payment(
        &conn,
        &mk,
        "0xdeadbeef",
        3,
        1000,
        order.amount_units,
        Some("0xbuyer"),
    )
    .expect("apply again");
    assert!(matches!(again, PaymentOutcome::AlreadySeen { .. }));
    assert_eq!(
        ledger::balance_units(&conn, key).unwrap(),
        10 * USD,
        "replay must not credit twice"
    );

    // Same order amount but a *different* tx after the order is paid: the
    // order is no longer pending, so the deposit is unmatched (never a
    // second credit).
    let other_tx = matcher::apply_payment(
        &conn,
        &mk,
        "0xfeedface",
        0,
        1001,
        order.amount_units,
        Some("0xbuyer"),
    )
    .expect("apply other");
    assert!(matches!(other_tx, PaymentOutcome::Unmatched { .. }));
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 10 * USD);
}

#[test]
fn one_amount_pays_one_order_only() {
    // Two live orders with distinct amounts; each payment credits exactly
    // the order whose amount matches.
    let conn = test_conn();
    let key = seed_key(&conn, "pay2");
    let mk = mk();
    let a = paid_order(&conn, key, 5, 15);
    let b = paid_order(&conn, key, 7, 15);
    assert_ne!(a.amount_units, b.amount_units);

    matcher::apply_payment(&conn, &mk, "0xaaa", 0, 10, a.amount_units, None).expect("pay a");
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 5 * USD);

    matcher::apply_payment(&conn, &mk, "0xbbb", 0, 11, b.amount_units, None).expect("pay b");
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 12 * USD);
}

#[test]
fn expired_order_is_not_paid() {
    let conn = test_conn();
    let key = seed_key(&conn, "pay3");
    let mk = mk();
    let order = paid_order(&conn, key, 10, 15);
    // Force the payment window closed AND past the 10-minute grace window:
    // within grace a late confirmation still credits (the money arrived for
    // an issued order); beyond grace the order is dead and the payment is
    // unmatched.
    conn.execute(
        "UPDATE credit_orders SET expires_at = datetime('now', '-11 minutes') WHERE id = ?1",
        rusqlite::params![order.id],
    )
    .expect("expire window");

    let outcome =
        matcher::apply_payment(&conn, &mk, "0xlate", 0, 10, order.amount_units, None).expect("apply");
    assert!(matches!(outcome, PaymentOutcome::Unmatched { .. }));
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 0);
}

#[test]
fn late_confirmed_payment_within_grace_still_credits() {
    // Broadcast before the deadline, confirmed 30 seconds after it: Base
    // confirms in seconds, so this is the normal late-confirmation shape.
    // The order must credit instead of stranding the buyer's money.
    let conn = test_conn();
    let key = seed_key(&conn, "pay4");
    let mk = mk();
    let order = paid_order(&conn, key, 10, 15);
    conn.execute(
        "UPDATE credit_orders SET expires_at = datetime('now', '-30 seconds') WHERE id = ?1",
        rusqlite::params![order.id],
    )
    .expect("expire window");

    let outcome =
        matcher::apply_payment(&conn, &mk, "0xlate", 0, 10, order.amount_units, None).expect("apply");
    assert!(
        matches!(
            outcome,
            PaymentOutcome::Credited { .. }
        ),
        "grace-window payment must credit, got {outcome:?}"
    );
    assert_eq!(
        ledger::balance_units(&conn, key).unwrap(),
        order.base_units
    );
}

#[test]
fn concurrent_duplicate_payments_credit_once() {
    // The stress test that matters: 8 threads deliver the SAME log 10 times
    // each against a shared DB file. Exactly one may win; the balance must
    // equal the single top-up.
    let dir = std::env::temp_dir().join(format!("op-credits-dup-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    let path = dir.join("credits.db");
    let order_amount;
    {
        let mut c = Connection::open(&path).expect("open");
        openproxy_db::migrations::run(&mut c).expect("migrate");
        let key = seed_key(&c, "dup");
        let o = paid_order(&c, key, 20, 15);
        order_amount = o.amount_units;
    }

    let winners = Arc::new(AtomicI64::new(0));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let path = path.clone();
        let winners = Arc::clone(&winners);
        handles.push(std::thread::spawn(move || {
            let conn = Connection::open(&path).expect("open");
            conn.busy_timeout(std::time::Duration::from_secs(10)).ok();
            let mk = MasterKey::generate().expect("mk");
            for _ in 0..10 {
                match matcher::apply_payment(&conn, &mk, "0xsamelog", 0, 500, order_amount, None) {
                    Ok(PaymentOutcome::Credited { .. }) => {
                        winners.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(_) => {}
                    Err(e) => panic!("apply failed: {e}"),
                }
            }
        }));
    }
    for h in handles {
        h.join().expect("join");
    }
    assert_eq!(
        winners.load(Ordering::SeqCst),
        1,
        "exactly one delivery may credit the order"
    );

    let conn = Connection::open(&path).expect("open reader");
    let balance: i64 = conn
        .query_row(
            "SELECT COALESCE(SUM(remaining_units), 0) FROM credit_transactions \
             WHERE status = 'active'",
            [],
            |r| r.get(0),
        )
        .expect("sum");
    assert_eq!(balance, 20 * USD, "no duplicate credits, no leaks");
    let _ = std::fs::remove_dir_all(&dir);
}

// ── ledger: FIFO spend + expiry ─────────────────────────────────────

#[test]
fn spend_is_fifo_by_expiry_and_cannot_overdraw() {
    let conn = test_conn();
    let key = seed_key(&conn, "spend1");
    let mk = mk();
    // Two lots: $3 expiring in 1 day, $5 expiring in 30.
    let soon = paid_order(&conn, key, 3, 1);
    matcher::apply_payment(&conn, &mk, "0xsoon", 0, 1, soon.amount_units, None).expect("pay soon");
    let late = paid_order(&conn, key, 5, 30);
    matcher::apply_payment(&conn, &mk, "0xlate", 0, 2, late.amount_units, None).expect("pay late");
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 8 * USD);

    // Spend $4: takes the whole $3 lot, then $1 from the $5 lot.
    record_spend(&conn, key, 4 * USD).expect("spend");
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 4 * USD);

    let lots = ledger::balance_view(&conn, key).unwrap().lots;
    assert_eq!(lots.len(), 1, "the soon lot must be exhausted");
    assert!((lots[0].remaining_usd - 4.0).abs() < 1e-9);

    // Overdraw is refused and consumes nothing.
    let err = record_spend(&conn, key, 5 * USD).expect_err("overdraw");
    assert!(matches!(
        err,
        openproxy_types::CoreError::InsufficientCredits { .. }
    ));
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 4 * USD);

    // Exact spend empties it.
    record_spend(&conn, key, 4 * USD).expect("exact spend");
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 0);
}

#[test]
fn expiry_kills_remaining_without_refund() {
    let conn = test_conn();
    let key = seed_key(&conn, "exp1");
    let mk = mk();
    let order = paid_order(&conn, key, 10, 15);
    matcher::apply_payment(&conn, &mk, "0xexp", 0, 1, order.amount_units, None).expect("pay");
    record_spend(&conn, key, 4 * USD).expect("spend");
    assert_eq!(ledger::balance_units(&conn, key).unwrap(), 6 * USD);

    // Age the lot past its expiry.
    conn.execute(
        "UPDATE credit_transactions SET expires_at = datetime('now', '-1 second') \
         WHERE kind = 'topup'",
        [],
    )
    .expect("age lot");

    assert_eq!(
        ledger::balance_units(&conn, key).unwrap(),
        0,
        "expired money is gone"
    );
    let killed = ledger::expire_due(&conn).expect("expire");
    assert_eq!(killed, 1);
    let remaining: i64 = conn
        .query_row(
            "SELECT remaining_units FROM credit_transactions WHERE kind = 'topup'",
            [],
            |r| r.get(0),
        )
        .expect("remaining");
    assert_eq!(remaining, 6 * USD, "the audit trail records what died");
    let status: String = conn
        .query_row(
            "SELECT status FROM credit_transactions WHERE kind = 'topup'",
            [],
            |r| r.get(0),
        )
        .expect("status");
    assert_eq!(status, "expired");

    // Spending against an expired balance is refused.
    assert!(record_spend(&conn, key, 1).is_err());
}

#[test]
fn history_exposes_the_on_chain_pointer() {
    let conn = test_conn();
    let key = seed_key(&conn, "hist1");
    let mk = mk();
    let order = paid_order(&conn, key, 10, 15);
    matcher::apply_payment(&conn, &mk, "0xaudit", 7, 999, order.amount_units, None).expect("pay");

    let rows = ledger::history(&conn, key, 50).expect("history");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "topup");
    assert_eq!(rows[0].tx_hash.as_deref(), Some("0xaudit"));
    assert_eq!(rows[0].order_id, Some(order.id));
    assert_eq!(rows[0].amount_units, 10 * USD);
}

#[test]
fn balance_view_reports_lots_and_credit_based() {
    let conn = test_conn();
    let key = seed_key(&conn, "view1");
    assert!(!ledger::is_credit_based(&conn, key).unwrap());
    let mk = mk();
    let order = paid_order(&conn, key, 10, 15);
    matcher::apply_payment(&conn, &mk, "0xview", 0, 1, order.amount_units, None).expect("pay");
    let view = ledger::balance_view(&conn, key).unwrap();
    assert!(view.credit_based);
    assert!((view.balance_usd - 10.0).abs() < 1e-9);
    assert_eq!(view.lots.len(), 1);
    assert!((view.lots[0].amount_usd - 10.0).abs() < 1e-9);
}

// ── anonymous checkout: mint + reveal once ──────────────────────────

#[test]
fn anonymous_payment_mints_key_and_reveals_once() {
    let conn = test_conn();
    let mk = mk();
    let order =
        orders::create_order(&conn, None, Some("jordan.lee18@outlook.com"), 10 * USD, 15, None)
            .expect("create anon order");
    let outcome = matcher::apply_payment(
        &conn,
        &mk,
        "0xanon",
        0,
        42,
        order.amount_units,
        Some("0xbuyer"),
    )
    .expect("apply");
    let (minted, key_id) = match outcome {
        PaymentOutcome::Credited {
            minted_key: Some(k),
            api_key_id,
            ..
        } => (k, api_key_id),
        other => panic!("expected minted key, got {other:?}"),
    };
    assert!(minted.starts_with("op_live_"));

    // The key is a real chat-scoped row with the balance attached.
    let scopes: String = conn
        .query_row(
            "SELECT scopes_json FROM api_keys WHERE id = ?1",
            rusqlite::params![key_id],
            |r| r.get(0),
        )
        .expect("scopes");
    assert!(scopes.contains("chat"));
    assert_eq!(
        ledger::balance_units(&conn, ApiKeyId(key_id)).unwrap(),
        10 * USD
    );

    // Reveal once: first call returns it, second returns None (wiped).
    let first = matcher::reveal_minted_key(&conn, &mk, &order.order_code)
        .expect("reveal")
        .expect("present");
    assert_eq!(first, minted);
    let second = matcher::reveal_minted_key(&conn, &mk, &order.order_code).expect("reveal 2");
    assert!(second.is_none(), "plaintext must not be recoverable twice");
}

#[test]
fn session_order_credits_the_existing_key_without_minting() {
    let conn = test_conn();
    let key = seed_key(&conn, "sess1");
    let mk = mk();
    let order = paid_order(&conn, key, 15, 15);
    let outcome = matcher::apply_payment(&conn, &mk, "0xsess", 0, 1, order.amount_units, None)
        .expect("apply");
    match outcome {
        PaymentOutcome::Credited {
            minted_key,
            api_key_id,
            ..
        } => {
            assert!(minted_key.is_none(), "session top-up must not mint a key");
            assert_eq!(api_key_id, key.0);
        }
        other => panic!("expected credited, got {other:?}"),
    }
    let key_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM api_keys", [], |r| r.get(0))
        .expect("count");
    assert_eq!(key_count, 1, "no extra key created");
}

// ── tiers + leveraged packages ──────────────────────────────────────

fn tier_config(models: &[&str], monthly: f64) -> crate::credits::TierConfig {
    crate::credits::TierConfig {
        monthly_usd_cap: Some(monthly),
        weekly_usd_cap: None,
        five_hour_usd_cap: None,
        allowed_models: Some(models.iter().map(|m| m.to_string()).collect()),
        allowed_providers: None,
        allowed_combos: None,
        only_whitelisted: None,
    }
}

/// Sellable plan commerce: what the buyer pays, what they get, how long it lives.
fn plan(price: f64, credit: f64, ttl: i64) -> crate::credits::PlanCommerce {
    crate::credits::PlanCommerce {
        price_usd: Some(price),
        credit_usd: Some(credit),
        ttl_days: ttl,
    }
}

/// A draft plan (no price): not listed for sale.
fn draft_plan() -> crate::credits::PlanCommerce {
    crate::credits::PlanCommerce {
        price_usd: None,
        credit_usd: None,
        ttl_days: 30,
    }
}

#[test]
fn tier_crud_and_apply_forces_the_whitelist_gate() {
    let conn = test_conn();
    let key = seed_key(&conn, "tiered");

    let t = crate::credits::tiers::create(&conn, "flash", &tier_config(&["m-a", "m-b"], 30.0), &draft_plan())
        .expect("create tier");
    assert_eq!(t.key_count, 0);
    assert!(t.active);

    // Validation: blank name rejected, duplicate rejected by UNIQUE.
    assert!(crate::credits::tiers::create(&conn, "  ", &tier_config(&["m"], 1.0), &draft_plan()).is_err());
    assert!(crate::credits::tiers::create(&conn, "flash", &tier_config(&["m"], 1.0), &draft_plan()).is_err());

    crate::credits::tiers::apply_to_key(&conn, key, &t.config).expect("apply");
    let models: String = conn
        .query_row("SELECT allowed_models_json FROM api_keys WHERE id = ?1", [key.0], |r| r.get(0))
        .expect("models");
    assert_eq!(models, r#"["m-a","m-b"]"#);
    let gate: i64 = conn
        .query_row("SELECT only_whitelisted FROM api_keys WHERE id = ?1", [key.0], |r| r.get(0))
        .expect("gate");
    // Money invariant: an allowlist is a hard gate, not advisory.
    assert_eq!(gate, 1, "allowlist must force only_whitelisted");
    let caps = crate::api_key_quotas::get_caps(&conn, key).expect("caps").expect("caps row");
    assert_eq!(caps.monthly_usd_cap, Some(30.0));

    // Update replaces the payload wholesale.
    crate::credits::tiers::update(&conn, t.id, Some("flash2"), Some(&tier_config(&["m-c"], 40.0)), Some(false), None)
        .expect("update");
    let t2 = crate::credits::tiers::get(&conn, t.id).expect("get").expect("tier");
    assert_eq!(t2.name, "flash2");
    assert!(!t2.active);
    assert_eq!(t2.config.allowed_models.as_deref(), Some(&["m-c".to_string()][..]));

    // Unused tier deletes cleanly.
    crate::credits::tiers::delete(&conn, t.id).expect("delete");
    assert!(crate::credits::tiers::get(&conn, t.id).expect("get").is_none());
}

#[test]
fn tier_allowlist_partitions_combos_from_models() {
    let conn = test_conn();
    let key = seed_key(&conn, "part");

    // A live combo row — the bare-name detection target.
    conn.execute(
        "INSERT INTO combos (name, strategy) VALUES ('autos', 'priority')",
        [],
    )
    .expect("combo row");

    // The admin UI sends ONE merged selection: `combo:autos` rides inside
    // allowed_models. The stored tier must end up canonical: models in
    // models_json, combo NAMES in combos_json, never a combo in models.
    let t = crate::credits::tiers::create(
        &conn,
        "mixed",
        &crate::credits::TierConfig {
            allowed_models: Some(vec!["combo:autos".into(), "deepseek-v4.1-flash".into()]),
            allowed_combos: None,
            ..tier_config(&["deepseek-v4.1-flash"], 10.0)
        },
        &draft_plan(),
    )
    .expect("create mixed tier");
    let row_models: String = conn
        .query_row("SELECT models_json FROM credit_tiers WHERE id = ?1", [t.id], |r| r.get(0))
        .expect("row models");
    let row_combos: String = conn
        .query_row("SELECT combos_json FROM credit_tiers WHERE id = ?1", [t.id], |r| r.get(0))
        .expect("row combos");
    assert_eq!(row_models, r#"["deepseek-v4.1-flash"]"#, "combo must leave models_json");
    assert_eq!(row_combos, r#"["autos"]"#, "combo name must land in combos_json");

    // Applying to a key resolves the combo NAME to the numeric id the
    // routing gate consumes, and the allowlist still forces the hard gate.
    crate::credits::tiers::apply_to_key(&conn, key, &t.config).expect("apply");
    let key_combos: String = conn
        .query_row("SELECT allowed_combos_json FROM api_keys WHERE id = ?1", [key.0], |r| r.get(0))
        .expect("key combos");
    assert_eq!(key_combos, "[1]", "combo name must resolve to its numeric id");
    let gate: i64 = conn
        .query_row("SELECT only_whitelisted FROM api_keys WHERE id = ?1", [key.0], |r| r.get(0))
        .expect("gate");
    assert_eq!(gate, 1, "allowlist must force only_whitelisted");
}

#[test]
fn leveraged_plan_charges_less_than_it_credits() {
    let conn = test_conn();
    let key = seed_key(&conn, "lev");
    let mk = mk();

    // The tier IS the product: price 1, credit 6 (6x), lives 7 days.
    let t = crate::credits::tiers::create(&conn, "flash", &tier_config(&["m-1"], 25.0), &plan(1.0, 6.0, 7))
        .expect("tier");

    // The buyer is charged the PRICE (whole dollar + jitter); the order
    // records the CREDIT (6x) for the matcher to pay out.
    let order = orders::create_order(&conn, Some(key), None, 1 * USD, t.ttl_days, Some(t.id))
        .expect("order");
    assert_eq!(order.credit_units, 6 * USD, "credit_units must carry the leverage");
    assert!(order.amount_units >= 1 * USD && order.amount_units < 2 * USD);

    let outcome = matcher::apply_payment(&conn, &mk, "0xlev", 0, 1, order.amount_units, None)
        .expect("apply");
    let credited = match outcome {
        PaymentOutcome::Credited { credited_units, .. } => credited_units,
        other => panic!("expected credited, got {other:?}"),
    };
    assert_eq!(credited, 6 * USD, "the balance must be the leveraged credit");

    // The tier shaping landed on the key together with the credit.
    let models: String = conn
        .query_row("SELECT allowed_models_json FROM api_keys WHERE id = ?1", [key.0], |r| r.get(0))
        .expect("models");
    assert_eq!(models, r#"["m-1"]"#);
    let gate: i64 = conn
        .query_row("SELECT only_whitelisted FROM api_keys WHERE id = ?1", [key.0], |r| r.get(0))
        .expect("gate");
    assert_eq!(gate, 1);
    let caps = crate::api_key_quotas::get_caps(&conn, key).expect("caps").expect("caps row");
    assert_eq!(caps.monthly_usd_cap, Some(25.0));
}

#[test]
fn sweep_deletes_keys_idle_for_six_months() {
    let conn = test_conn();
    let idle = seed_key(&conn, "idle-8mo");
    let used_recently = seed_key(&conn, "used-3mo-ago");
    let young = seed_key(&conn, "young");
    let operator = seed_key(&conn, "operator");

    conn.execute(
        "UPDATE api_keys SET scopes_json = '[\"chat\",\"manage\"]' WHERE id = ?1",
        params![operator.0],
    )
    .unwrap();

    // Age three keys beyond the window; keep `young` inside it.
    conn.execute(
        "UPDATE api_keys SET created_at = datetime('now', '-8 months')
         WHERE id IN (?1, ?2, ?3)",
        params![idle.0, used_recently.0, operator.0],
    )
    .unwrap();

    // `used_recently` has a request 3 months ago (inside the window) plus a
    // stale one from 9 months ago: the recent one saves it.
    for (rid, age) in [("recent", "-3 months"), ("stale", "-9 months")] {
        conn.execute(
            "INSERT INTO usage (api_key_id, request_id, trace_id, attempt, provider_id, 
             upstream_model_id, status_code, total_ms, created_at)
             VALUES (?1, ?2, 't', 1, 'p', 'm', 200, 10, datetime('now', ?3))",
            params![used_recently.0, rid, age],
        )
        .unwrap();
    }

    // The idle key holds a small balance that must die with it.
    conn.execute(
        "INSERT INTO credit_transactions (api_key_id, kind, amount_units, status)
         VALUES (?1, 'topup', 500, 'active')",
        params![idle.0],
    )
    .unwrap();
    // ...and carries a quota row (must cascade).
    crate::api_key_quotas::upsert(
        &conn,
        idle,
        &crate::api_key_quotas::QuotaCaps {
            monthly_usd_cap: Some(5.0),
            weekly_usd_cap: None,
            five_hour_usd_cap: None,
        },
    )
    .unwrap();

    let swept = crate::credits::sweep::sweep_idle_keys(&conn, 100).expect("sweep");
    assert_eq!(swept.len(), 1, "only the truly idle key is swept");
    assert_eq!(swept[0].id, idle.0);
    assert_eq!(swept[0].balance_units, 500, "the report carries the dead balance");

    // Gone: key row, quota row, ledger rows.
    let alive: i64 = conn
        .query_row("SELECT COUNT(*) FROM api_keys WHERE id = ?1", params![idle.0], |r| r.get(0))
        .unwrap();
    assert_eq!(alive, 0);
    let quotas: i64 = conn
        .query_row("SELECT COUNT(*) FROM api_key_quotas WHERE api_key_id = ?1", params![idle.0], |r| r.get(0))
        .unwrap();
    assert_eq!(quotas, 0);
    let ledger: i64 = conn
        .query_row("SELECT COUNT(*) FROM credit_transactions WHERE api_key_id = ?1", params![idle.0], |r| r.get(0))
        .unwrap();
    assert_eq!(ledger, 0);

    // Survivors: young, recently-used, and the manage-scoped operator key.
    let survivors: i64 = conn
        .query_row("SELECT COUNT(*) FROM api_keys", [], |r| r.get(0))
        .unwrap();
    assert_eq!(survivors, 3);
    assert!(crate::credits::sweep::is_idle(&conn, used_recently.0).unwrap() == false);
    assert!(crate::credits::sweep::is_idle(&conn, young.0).unwrap() == false);
    assert!(crate::credits::sweep::is_idle(&conn, operator.0).unwrap() == false);
}

#[test]
fn sweep_respects_limit_per_pass() {
    let conn = test_conn();
    let mut ids = Vec::new();
    for n in 0..3 {
        let key = seed_key(&conn, &format!("old-{n}"));
        conn.execute(
            "UPDATE api_keys SET created_at = datetime('now', '-8 months') WHERE id = ?1",
            params![key.0],
        )
        .unwrap();
        ids.push(key);
    }
    let swept = crate::credits::sweep::sweep_idle_keys(&conn, 2).expect("sweep");
    assert_eq!(swept.len(), 2, "limit caps the pass");
    let left: i64 = conn
        .query_row("SELECT COUNT(*) FROM api_keys WHERE created_at <= datetime('now', '-6 months')", [], |r| r.get(0))
        .unwrap();
    assert_eq!(left, 1, "the remaining idle key waits for the next pass");
}
