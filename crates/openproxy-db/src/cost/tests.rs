use super::*;
use crate::conn::DbPool;
use openproxy_types::endpoint::EndpointKind;
use openproxy_types::ids::{ProviderId, RequestId};
use openproxy_types::usage::UsageInput;

fn test_input(
    prompt: u32,
    completion: u32,
    ttft: Option<u64>,
    total: u64,
    flags: u8,
) -> UsageInput {
    UsageInput {
        request_id: RequestId::new(),
        trace_id: "trace-test".to_string(),
        attempt: 1,
        provider_id: ProviderId::new("test"),
        account_id: None,
        combo_id: None,
        combo_target_id: None,
        model_row_id: None,
        upstream_model_id: "test-model".to_string(),
        prompt_tokens: Some(prompt),
        completion_tokens: Some(completion),
        connect_ms: None,
        ttft_ms: ttft,
        total_ms: total,
        status_code: 200,
        error_msg: None,
        race_total: 1,
        api_key_id: None,
        request_body_json: None,
        response_body_json: None,
        request_headers: None,
        response_headers: None,
        error_message: None,
        race_attempts: 1,
        stop_reason: None,
        compression_savings_pct: None,
        compression_techniques: None,
        pii_redacted: None,
        endpoint_kind: EndpointKind::Chat,
        proxy_url: None,
        proxy_status: None,
        cached_tokens: None,
        flags,
        audio_ms: None,
        session_hash: None,
        max_chunk_gap_ms: None,
        avg_chunk_gap_ms: None,
    }
}

#[test]
fn test_compute_cost_and_tps() {
    let price = crate::pricing::Price {
        input_per_1m: 1.0,
        output_per_1m: 2.0,
        // An explicit cached rate, in dollars per 1M like the other two.
        cached_per_1m: Some(0.5),
        kind: crate::pricing::PriceKind::Chat,
        audio_per_hour: None,
    };
    let mut input = test_input(
        100,
        200,
        Some(100),
        1000,
        USAGE_FLAG_IS_STREAMING | USAGE_FLAG_STREAM_COMPLETE | USAGE_FLAG_CLIENT_RESPONSE,
    );
    let (cost, tps) = compute(Some(price), &input);
    assert_eq!(cost, Some(0.0005));
    // 200 * 1000 / 1000 = 200.0
    assert_eq!(tps, Some(200.0));

    input.cached_tokens = Some(100);
    let (cost_cached, _) = compute(Some(price), &input);
    assert!(cost_cached.is_some_and(|c| (c - 0.00045).abs() < 1e-9));

    // Without a cached rate there is no discount: cache reads bill at input.
    let no_cached_rate = crate::pricing::Price {
        cached_per_1m: None,
        ..price
    };
    let (cost_plain, _) = compute(Some(no_cached_rate), &input);
    assert_eq!(cost_plain, Some(0.0005));

    input.total_ms = 0;
    assert_eq!(
        compute(Some(crate::pricing::Price::default()), &input).1,
        None
    );

    input.total_ms = 1000;
    input.completion_tokens = Some(0);
    assert_eq!(
        compute(Some(crate::pricing::Price::default()), &input).1,
        None
    );
}

#[test]
fn test_redact_error_msg() {
    let raw_msg = "Error connecting to sk-1234567890abcdef and x-api-key: my-secret-key and Authorization: Bearer my-bearer-token.";
    let redacted = redact_error_msg(raw_msg);
    assert!(!redacted.contains("sk-1234567890abcdef") && redacted.contains("sk-[REDACTED]"));
    assert!(!redacted.contains("my-secret-key") && redacted.contains("x-api-key: [REDACTED]"));
    assert!(
        !redacted.contains("my-bearer-token")
            && redacted.contains("Authorization: Bearer [REDACTED]")
    );
    let long_msg = "a".repeat(2100);
    let redacted2 = redact_error_msg(&long_msg);
    assert!(redacted2.ends_with("...[truncated]") && redacted2.len() <= 2048 + 14);
}

#[test]
fn test_record() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-test").expect("open pool");
    let conn = pool.writer();
    let mut input = test_input(
        100,
        200,
        Some(100),
        1100,
        USAGE_FLAG_IS_STREAMING | USAGE_FLAG_STREAM_COMPLETE | USAGE_FLAG_CLIENT_RESPONSE,
    );
    input.connect_ms = Some(10);
    input.error_msg = Some("test error sk-1234567890abcdef".to_string());
    let rowid = record(&conn, &input).expect("record failed");
    let count: i64 = conn
        .query_row(
            "SELECT count(*) FROM usage WHERE id = ?1",
            rusqlite::params![rowid.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let error_msg_redacted: String = conn
        .query_row(
            "SELECT error_msg_redacted FROM usage WHERE id = ?1",
            rusqlite::params![rowid.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(error_msg_redacted, "test error sk-[REDACTED]");
}

#[test]
fn record_with_retry_succeeds_first_attempt_on_quiescent_db() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-retry").expect("open pool");
    let input = test_input(10, 20, None, 100, USAGE_FLAG_CLIENT_RESPONSE);
    assert!(record_with_retry(&pool.writer(), &input).is_ok());
}

// Asserts the real credit-debit path: payments-only (the public build's
// debit is a no-op stub).
#[cfg(feature = "payments")]
#[test]
fn billable_insert_debits_and_callback_does_not_debit_twice() {
    let pool = DbPool::test_pool_with_prefix("checkout-billable-insert").unwrap();
    let conn = pool.writer();
    conn.execute(
        "INSERT INTO api_keys(key_hash) VALUES('insert-billing-fixture')",
        [],
    )
    .unwrap();
    let key = conn.last_insert_rowid();
    conn.execute("INSERT INTO credit_transactions(api_key_id,kind,amount_units,remaining_units) VALUES(?1,'grant',1000000,1000000)", [key]).unwrap();
    let mut input = test_input(1_000_000, 0, None, 100, USAGE_FLAG_CLIENT_RESPONSE);
    input.provider_id = ProviderId::new("openrouter");
    input.upstream_model_id = "openai/gpt-4o-mini".into();
    input.api_key_id = Some(openproxy_types::ids::ApiKeyId(key));
    let id = record(&conn, &input).unwrap();
    assert_eq!(
        crate::credits_ledger::balance_units(&conn, key).unwrap(),
        850000
    );
    mark_client_response(&conn, id).unwrap();
    assert_eq!(
        crate::credits_ledger::balance_units(&conn, key).unwrap(),
        850000
    );
}

#[test]
fn test_client_response_and_winner_lifecycle() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-client-resp").expect("open pool");
    let conn = pool.writer();
    let target_id = openproxy_types::ids::ComboTargetId(42);

    let mut input_winner = test_input(10, 20, None, 100, USAGE_FLAG_CLIENT_RESPONSE);
    input_winner.combo_target_id = Some(target_id);
    let rowid1 = record(&conn, &input_winner).expect("insert winner");
    let (cr1, ww1): (i64, i64) = conn
        .query_row(
            "SELECT client_response, was_winner FROM usage WHERE id = ?1",
            rusqlite::params![rowid1.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(cr1, 1);
    assert_eq!(ww1, 1);

    let mut input_fail = input_winner.clone();
    input_fail.request_id = RequestId::new();
    input_fail.flags = 0;
    let rowid2 = record(&conn, &input_fail).expect("insert non-winner");
    let (cr2, ww2): (i64, i64) = conn
        .query_row(
            "SELECT client_response, was_winner FROM usage WHERE id = ?1",
            rusqlite::params![rowid2.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(cr2, 0);
    assert_eq!(ww2, 0);

    mark_winner_usage_row(&conn, &input_fail.request_id.to_string(), 1, target_id)
        .expect("mark_winner_usage_row");
    let (cr2_after, ww2_after): (i64, i64) = conn
        .query_row(
            "SELECT client_response, was_winner FROM usage WHERE id = ?1",
            rusqlite::params![rowid2.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(cr2_after, 1);
    assert_eq!(ww2_after, 1);

    let mut input_3 = input_winner;
    input_3.request_id = RequestId::new();
    input_3.flags = 0;
    let rowid3 = record(&conn, &input_3).expect("insert row 3");
    mark_client_response(&conn, rowid3).expect("mark_client_response");
    let (cr3, ww3): (i64, i64) = conn
        .query_row(
            "SELECT client_response, was_winner FROM usage WHERE id = ?1",
            rusqlite::params![rowid3.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(cr3, 1);
    assert_eq!(ww3, 1);
}

#[test]
fn test_record_row_returns_id_and_row_without_lock_contention() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-record-row").expect("open pool");
    let conn = pool.writer();
    let input = test_input(50, 150, Some(80), 800, USAGE_FLAG_CLIENT_RESPONSE);
    let (usage_id, row) = record_row(&conn, &input).expect("record_row succeeded");
    assert_eq!(usage_id, row.id);
    assert_eq!(row.prompt_tokens, Some(50));
    assert_eq!(row.completion_tokens, Some(150));
    assert_eq!(row.ttft_ms, Some(80));
}

#[test]
fn combo_own_price_bills_the_request_not_the_member_model() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-combo-price").expect("open pool");
    let conn = pool.writer();
    // The routed model has an official (expensive) price.
    conn.execute(
        "INSERT INTO model_capabilities_sync \
         (provider_id, model_id, pricing_input_per_1m, pricing_output_per_1m) \
         VALUES ('test', 'test-model', 10.0, 30.0)",
        [],
    )
    .expect("seed model price");
    // The combo carries its own product price.
    conn.execute(
        "INSERT INTO combos (id, name, strategy, pricing_input_per_1m, pricing_output_per_1m) \
         VALUES (900, 'priced', 'priority', 1.0, 2.0), \
                (901, 'inherits', 'priority', NULL, NULL)",
        [],
    )
    .expect("seed combos");

    let mut priced = test_input(1_000_000, 1_000_000, None, 1000, USAGE_FLAG_CLIENT_RESPONSE);
    priced.combo_id = Some(openproxy_types::ids::ComboId(900));
    let (priced_id, _) = record_row(&conn, &priced).expect("record priced combo row");
    let priced_cost: f64 = conn
        .query_row(
            "SELECT cost_usd FROM usage WHERE id = ?1",
            rusqlite::params![priced_id.0],
            |r| r.get(0),
        )
        .unwrap();
    // 1M input at 1.0 + 1M output at 2.0 = 3.0 — never the model's 40.0.
    assert!(
        (priced_cost - 3.0).abs() < 1e-9,
        "combo price must win, got {priced_cost}"
    );

    // A combo without its own price still bills the target model's rate.
    let mut inherits = test_input(1_000_000, 1_000_000, None, 1000, USAGE_FLAG_CLIENT_RESPONSE);
    inherits.combo_id = Some(openproxy_types::ids::ComboId(901));
    let (inherits_id, _) = record_row(&conn, &inherits).expect("record inheriting row");
    let inherits_cost: f64 = conn
        .query_row(
            "SELECT cost_usd FROM usage WHERE id = ?1",
            rusqlite::params![inherits_id.0],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        (inherits_cost - 40.0).abs() < 1e-9,
        "unpriced combo must fall back to the model rate, got {inherits_cost}"
    );
}

#[test]
fn test_record_with_deleted_api_key_does_not_fail_foreign_key() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-del-key").expect("open pool");
    let conn = pool.writer();
    let mut input = test_input(100, 200, Some(50), 500, USAGE_FLAG_CLIENT_RESPONSE);
    input.api_key_id = Some(openproxy_types::ids::ApiKeyId(99999999));

    let res = record(&conn, &input);
    assert!(res.is_ok(), "insert must succeed: {:?}", res.err());
    let rowid = res.unwrap();
    let saved_key_id: Option<i64> = conn
        .query_row(
            "SELECT api_key_id FROM usage WHERE id = ?1",
            rusqlite::params![rowid.0],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(saved_key_id, None, "orphaned api_key_id must be NULL");
}

// Asserts the real credit-debit path: payments-only (see the sibling test).
#[cfg(feature = "payments")]
#[test]
fn attributed_insert_failure_cannot_fall_back_to_anonymous_usage() {
    let pool = DbPool::test_pool_with_prefix("checkout-no-anonymous-fallback").unwrap();
    let conn = pool.writer();
    conn.execute(
        "INSERT INTO api_keys(key_hash) VALUES('attributed-fixture')",
        [],
    )
    .unwrap();
    let key = conn.last_insert_rowid();
    conn.execute("INSERT INTO credit_transactions(api_key_id,kind,amount_units,remaining_units) VALUES(?1,'grant',1000000,1000000)", [key]).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_attributed_usage BEFORE INSERT ON usage WHEN NEW.api_key_id IS NOT NULL BEGIN SELECT RAISE(ABORT,'injected attributed insert failure'); END").unwrap();
    let mut input = test_input(1000, 100, None, 100, USAGE_FLAG_CLIENT_RESPONSE);
    input.provider_id = ProviderId::new("openrouter");
    input.upstream_model_id = "openai/gpt-4o-mini".into();
    input.api_key_id = Some(openproxy_types::ids::ApiKeyId(key));

    assert!(record(&conn, &input).is_err());
    let rows: i64 = conn
        .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        rows, 0,
        "failed attributed insert must not become anonymous"
    );
    assert_eq!(
        crate::credits_ledger::balance_units(&conn, key).unwrap(),
        1_000_000
    );

    conn.execute_batch("DROP TRIGGER reject_attributed_usage")
        .unwrap();
    let id = record(&conn, &input).unwrap();
    let saved: i64 = conn
        .query_row("SELECT api_key_id FROM usage WHERE id=?1", [id.0], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(saved, key);
    assert!(crate::credits_ledger::balance_units(&conn, key).unwrap() < 1_000_000);
}

/// The projected row must carry the durable charge evidence, not a guess.
fn projected_row(conn: &rusqlite::Connection, id: i64) -> RecentUsageRow {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT {RECENT_USAGE_COLS} FROM usage WHERE id = ?1"
        ))
        .unwrap();
    stmt.query_row(rusqlite::params![id], map_recent_usage_from_row)
        .unwrap()
}

#[cfg(feature = "payments")]
#[test]
fn charged_units_records_what_the_debit_moved_and_leaves_the_price_alone() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-charged-units").unwrap();
    let conn = pool.writer();
    conn.execute("INSERT INTO api_keys(key_hash) VALUES('charged-units')", [])
        .unwrap();
    let key = conn.last_insert_rowid();
    conn.execute("INSERT INTO credit_transactions(api_key_id,kind,amount_units,remaining_units) VALUES(?1,'grant',1000000,1000000)", [key])
        .unwrap();
    let mut input = test_input(1_000_000, 0, None, 100, USAGE_FLAG_CLIENT_RESPONSE);
    input.provider_id = ProviderId::new("openrouter");
    input.upstream_model_id = "openai/gpt-4o-mini".into();
    input.api_key_id = Some(openproxy_types::ids::ApiKeyId(key));

    let (id, row) = record_row(&conn, &input).unwrap();
    // 1M prompt tokens at 0.15 USD/1M = 0.15 USD = 150000 micro-USD.
    let cost = row.cost_usd.expect("the row is priced");
    assert!((cost - 0.15).abs() < 1e-9, "unexpected cost {cost}");
    assert_eq!(row.charged_units, Some(150_000));
    assert!(row.credits_debited);
    assert_eq!(
        row.charge_state(),
        openproxy_types::usage::ChargeState::Charged
    );

    let stored = projected_row(&conn, id.0);
    assert_eq!(stored.charged_units, Some(150_000));
    assert!(stored.credits_debited);
    assert_eq!(
        crate::credits_ledger::balance_units(&conn, key).unwrap(),
        850_000
    );
    // The calculated price is never rewritten by the charge bookkeeping.
    assert_eq!(stored.cost_usd, row.cost_usd);
}

/// The exact shape of the row that motivated the column: a priced 200 for a key
/// that holds no prepaid credits. The debit path runs, charges nothing, and the
/// row must say "not charged" instead of echoing the price as if it had been
/// billed.
#[cfg(feature = "payments")]
#[test]
fn priced_row_on_a_key_without_prepaid_credits_is_not_charged() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-no-ledger").unwrap();
    let conn = pool.writer();
    conn.execute("INSERT INTO api_keys(key_hash) VALUES('no-ledger')", [])
        .unwrap();
    let key = conn.last_insert_rowid();
    let mut input = test_input(1_000_000, 0, None, 100, USAGE_FLAG_CLIENT_RESPONSE);
    input.provider_id = ProviderId::new("openrouter");
    input.upstream_model_id = "openai/gpt-4o-mini".into();
    input.api_key_id = Some(openproxy_types::ids::ApiKeyId(key));

    let (id, row) = record_row(&conn, &input).unwrap();
    assert!(row.cost_usd.is_some_and(|c| c > 0.0), "the row is priced");
    assert_eq!(
        row.charged_units,
        Some(0),
        "nothing left the balance, so nothing was charged"
    );
    assert!(row.credits_debited, "the debit path still ran");
    assert_eq!(
        row.charge_state(),
        openproxy_types::usage::ChargeState::NotCharged
    );

    let stored = projected_row(&conn, id.0);
    assert_eq!(stored.charged_units, Some(0));
    assert_eq!(crate::credits_ledger::balance_units(&conn, key).unwrap(), 0);
}

/// Historical rows have no recorded amount: marker clear means no debit ever
/// ran (not charged), marker set means the amount is unrecoverable (unknown).
/// Neither may be answered with `cost_usd`.
#[test]
fn historical_rows_report_not_charged_or_unknown_never_the_price() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-historical").unwrap();
    let conn = pool.writer();
    let mut input = test_input(100, 200, Some(50), 500, 0);
    input.provider_id = ProviderId::new("openrouter");
    input.upstream_model_id = "openai/gpt-4o-mini".into();
    let (id, row) = record_row(&conn, &input).unwrap();
    assert_eq!(row.charged_units, None, "no debit was attempted");
    assert!(!row.credits_debited);
    assert_eq!(
        row.charge_state(),
        openproxy_types::usage::ChargeState::NotCharged
    );

    // Simulate a row billed before migration 132 existed.
    conn.execute(
        "UPDATE usage SET credits_debited = 1, charged_units = NULL WHERE id = ?1",
        [id.0],
    )
    .unwrap();
    let historical = projected_row(&conn, id.0);
    assert_eq!(historical.charged_units, None);
    assert!(historical.credits_debited);
    assert_eq!(
        historical.charge_state(),
        openproxy_types::usage::ChargeState::Unknown,
        "the marker alone cannot prove an amount"
    );
}

/// Política marketplace-cancel: una fila 499 (cancel del cliente) factura los
/// prompt tokens a precio de input uncached (no llegó el bloque de usage con
/// el reparto de caché) + los tokens entregados en vuelo. Los demás errores
/// siguen en coste 0.
#[test]
fn status_499_bills_full_uncached_input_and_partial_output() {
    let price = Some(crate::pricing::Price {
        input_per_1m: 3.0,
        output_per_1m: 15.0,
        cached_per_1m: Some(0.3),
        kind: crate::pricing::PriceKind::Chat,
        ..Default::default()
    });
    let mut input = test_input(1000, 500, None, 1, 0);
    input.status_code = 499;
    input.cached_tokens = None;
    let (cost, _) = super::compute(price, &input);
    let expected = 1000.0 / 1_000_000.0 * 3.0 + 500.0 / 1_000_000.0 * 15.0;
    assert!(
        cost.expect("499 must bill").close_to(expected),
        "cost {cost:?} != expected {expected}"
    );

    // Con cached declarado (llegó usage final: no es un cancel a medio
    // stream) el 2xx sí descuenta caché; el 499 lo ignora siempre.
    let mut delivered = test_input(1000, 500, None, 1, 0);
    delivered.status_code = 200;
    delivered.cached_tokens = Some(800);
    let (cost_ok, _) = super::compute(price, &delivered);
    let expected_cached =
        200.0 / 1_000_000.0 * 3.0 + 800.0 / 1_000_000.0 * 0.3 + 500.0 / 1_000_000.0 * 15.0;
    assert!(
        cost_ok.expect("2xx must bill").close_to(expected_cached),
        "cost {cost_ok:?} != expected {expected_cached}"
    );

    // Un upstream caído no se cobra al usuario.
    let mut upstream_err = test_input(1000, 500, None, 1, 0);
    upstream_err.status_code = 502;
    let (cost_err, _) = super::compute(price, &upstream_err);
    assert_eq!(cost_err, Some(0.0), "upstream errors must stay free");
}

trait CloseTo {
    fn close_to(&self, other: f64) -> bool;
}
impl CloseTo for f64 {
    fn close_to(&self, other: f64) -> bool {
        (self - other).abs() < 1e-9
    }
}

// --- Combo pricing schedules -------------------------------------------------

use openproxy_types::pricing_schedule::{
    OffPeakDate, PeakClock, PeakWindow, PricingRate, PricingSchedule,
};

/// A peak window that covers the whole week, so a record-time billing test is
/// independent of the wall clock. `00:00`–`23:59` plus the overnight
/// `23:59`–`00:00` tail leaves no minute uncovered.
fn always_peak_schedule(off_peak_dates: Vec<OffPeakDate>) -> PricingSchedule {
    PricingSchedule {
        timezone: "UTC".to_string(),
        peak: PricingRate {
            input_per_1m: 6.0,
            output_per_1m: 24.0,
            cached_per_1m: Some(3.0),
        },
        off_peak: PricingRate {
            input_per_1m: 2.0,
            output_per_1m: 8.0,
            cached_per_1m: None,
        },
        peak_windows: vec![
            PeakWindow {
                days: vec![1, 2, 3, 4, 5, 6, 7],
                start: PeakClock::parse("00:00").expect("start"),
                end: PeakClock::parse("23:59").expect("end"),
            },
            PeakWindow {
                days: vec![1, 2, 3, 4, 5, 6, 7],
                start: PeakClock::parse("23:59").expect("start"),
                end: PeakClock::parse("00:00").expect("end"),
            },
        ],
        off_peak_dates,
    }
}

fn seed_scheduled_combo(conn: &rusqlite::Connection, id: i64, schedule: &PricingSchedule) {
    let json = serde_json::to_string(schedule).expect("serialize schedule");
    conn.execute(
        "INSERT INTO combos (id, name, strategy, pricing_schedule) \
         VALUES (?1, ?2, 'priority', ?3)",
        rusqlite::params![id, format!("sched-{id}"), json],
    )
    .expect("seed scheduled combo");
}

fn cost_of(conn: &rusqlite::Connection, usage_id: i64) -> Option<f64> {
    conn.query_row(
        "SELECT cost_usd FROM usage WHERE id = ?1",
        rusqlite::params![usage_id],
        |r| r.get(0),
    )
    .expect("read cost")
}

#[test]
fn scheduled_combo_bills_the_peak_band_with_cached_split() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-sched-peak").expect("open pool");
    let conn = pool.writer();
    seed_scheduled_combo(&conn, 950, &always_peak_schedule(Vec::new()));

    let mut input = test_input(1_000_000, 1_000_000, None, 1000, USAGE_FLAG_CLIENT_RESPONSE);
    input.combo_id = Some(openproxy_types::ids::ComboId(950));
    input.cached_tokens = Some(500_000);
    let (usage_id, _) = record_row(&conn, &input).expect("record scheduled row");

    // 500k uncached at 6.0 + 500k cached at 3.0 + 1M output at 24.0 = 28.5.
    let cost = cost_of(&conn, usage_id.0).expect("priced row");
    assert!((cost - 28.5).abs() < 1e-9, "peak band cost was {cost}");
}

#[test]
fn explicit_off_peak_date_bills_the_off_peak_band() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-sched-offpeak").expect("open pool");
    let conn = pool.writer();
    let today =
        OffPeakDate::parse(&chrono::Utc::now().format("%Y-%m-%d").to_string()).expect("today");
    seed_scheduled_combo(&conn, 952, &always_peak_schedule(vec![today]));

    let mut input = test_input(1_000_000, 1_000_000, None, 1000, USAGE_FLAG_CLIENT_RESPONSE);
    input.combo_id = Some(openproxy_types::ids::ComboId(952));
    input.cached_tokens = Some(500_000);
    let (usage_id, _) = record_row(&conn, &input).expect("record off-peak row");

    // 500k uncached at 2.0 + 500k cached at the same 2.0 (no cached rate) + 1M
    // output at 8.0 = 10.0, never the peak band.
    let cost = cost_of(&conn, usage_id.0).expect("priced row");
    assert!((cost - 10.0).abs() < 1e-9, "off-peak band cost was {cost}");
}

#[test]
fn a_failing_upstream_on_a_scheduled_combo_stays_free() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-sched-error").expect("open pool");
    let conn = pool.writer();
    seed_scheduled_combo(&conn, 953, &always_peak_schedule(Vec::new()));

    let mut input = test_input(1_000_000, 1_000_000, None, 1000, USAGE_FLAG_CLIENT_RESPONSE);
    input.combo_id = Some(openproxy_types::ids::ComboId(953));
    input.status_code = 502;
    let (usage_id, _) = record_row(&conn, &input).expect("record failed row");
    assert_eq!(cost_of(&conn, usage_id.0), Some(0.0));
}

#[test]
fn backfill_reprices_a_scheduled_combo_at_each_rows_own_band() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-sched-backfill").expect("open pool");
    let conn = pool.writer();
    seed_scheduled_combo(&conn, 951, &weekday_window_schedule());

    let insert = |request_id: &str, created_at: &str, cost: f64| -> i64 {
        conn.execute(
            "INSERT INTO usage (request_id, trace_id, provider_id, upstream_model_id, attempt, \
             race_total, race_lost, total_ms, status_code, client_response, cost_usd, combo_id, \
             prompt_tokens, completion_tokens, created_at) \
             VALUES (?1, 'trace', 'test', 'test-model', 1, 1, 0, 1, 200, 1, ?2, 951, 1000000, \
             1000000, ?3)",
            rusqlite::params![request_id, cost, created_at],
        )
        .expect("seed usage row");
        conn.last_insert_rowid()
    };
    // 2026-11-16 is a Monday: 07:00 bills peak, 12:00 bills off-peak.
    let peak_row = insert("bf-peak", "2026-11-16 07:00:00", 0.0);
    let off_row = insert("bf-off", "2026-11-16 12:00:00", 0.0);
    let broken_row = insert("bf-broken", "not-a-timestamp", 0.0);

    let updated = backfill_usage_pricing(&conn).expect("backfill");

    // 6.0 + 24.0 for the peak row, 2.0 + 8.0 for the off-peak row — each at the
    // band its own timestamp falls in, never "today's" band for both.
    assert_eq!(cost_of(&conn, peak_row), Some(30.0));
    assert_eq!(cost_of(&conn, off_row), Some(10.0));
    // An unreadable timestamp is skipped, not guessed.
    assert_eq!(cost_of(&conn, broken_row), Some(0.0));
    assert_eq!(updated, 2);
}

/// A Monday-to-Friday `04:00`–`10:00` peak window: narrow enough that a test can
/// sit one second on either side of a boundary.
fn weekday_window_schedule() -> PricingSchedule {
    PricingSchedule {
        timezone: "UTC".to_string(),
        peak: PricingRate {
            input_per_1m: 6.0,
            output_per_1m: 24.0,
            cached_per_1m: None,
        },
        off_peak: PricingRate {
            input_per_1m: 2.0,
            output_per_1m: 8.0,
            cached_per_1m: None,
        },
        peak_windows: vec![PeakWindow {
            days: vec![1, 2, 3, 4, 5],
            start: PeakClock::parse("04:00").expect("start"),
            end: PeakClock::parse("10:00").expect("end"),
        }],
        off_peak_dates: Vec::new(),
    }
}

fn utc_at(
    y: i32,
    mo: u32,
    d: u32,
    h: u32,
    mi: u32,
    s: u32,
    nano: u32,
) -> chrono::DateTime<chrono::Utc> {
    chrono::NaiveDate::from_ymd_opt(y, mo, d)
        .expect("valid date")
        .and_hms_nano_opt(h, mi, s, nano)
        .expect("valid time")
        .and_utc()
}

/// The band and the stored timestamp must come from one instant: both edges of a
/// window, plus a sub-second instant that has to truncate to the second it is
/// billed at. `record_row_at` is the seam that makes this deterministic without
/// waiting on a clock.
#[test]
fn record_row_at_bills_the_band_of_the_instant_it_stores() {
    let pool = DbPool::test_pool_with_prefix("openproxy-cost-sched-edge").expect("open pool");
    let conn = pool.writer();
    seed_scheduled_combo(&conn, 955, &weekday_window_schedule());

    let mut input = test_input(1_000_000, 1_000_000, None, 1000, USAGE_FLAG_CLIENT_RESPONSE);
    input.combo_id = Some(openproxy_types::ids::ComboId(955));

    // 2026-11-16 is a Monday. (instant, stored created_at, billed cost)
    let cases = [
        // 999ms before the window opens: still off-peak, and the sub-second part
        // must not leak into either the band or the stored timestamp.
        (
            utc_at(2026, 11, 16, 3, 59, 59, 999_000_000),
            "2026-11-16 03:59:59",
            10.0,
        ),
        // First peak second.
        (
            utc_at(2026, 11, 16, 4, 0, 0, 0),
            "2026-11-16 04:00:00",
            30.0,
        ),
        // Last peak second.
        (
            utc_at(2026, 11, 16, 9, 59, 59, 0),
            "2026-11-16 09:59:59",
            30.0,
        ),
        // Window close is exclusive.
        (
            utc_at(2026, 11, 16, 10, 0, 0, 0),
            "2026-11-16 10:00:00",
            10.0,
        ),
    ];

    for (billed_at, stored, expected) in &cases {
        use chrono::Timelike;
        let truncated = billed_at
            .with_nanosecond(0)
            .expect("truncate to the second");
        let mut row_input = input.clone();
        row_input.request_id = RequestId::new();
        let (usage_id, row) =
            record_row_at(&conn, &row_input, *billed_at).expect("record row at instant");

        let cost = cost_of(&conn, usage_id.0).expect("priced row");
        assert!(
            (cost - expected).abs() < 1e-9,
            "cost at {stored} was {cost}, expected {expected}"
        );
        let persisted: String = conn
            .query_row(
                "SELECT created_at FROM usage WHERE id = ?1",
                rusqlite::params![usage_id.0],
                |r| r.get(0),
            )
            .expect("read created_at");
        // The stored instant is exactly the one the band was resolved at, so
        // re-pricing this row from its own timestamp reproduces this cost.
        assert_eq!(&persisted, stored, "stored created_at");
        assert_eq!(
            row.created_at,
            truncated.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
            "broadcast row carries the same instant"
        );
        assert_eq!(
            crate::pricing::parse_usage_timestamp(&persisted),
            Some(truncated),
            "the stored timestamp parses back to the billing instant"
        );
    }
}
