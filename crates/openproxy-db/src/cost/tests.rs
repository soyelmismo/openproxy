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

/// Política antseed-cancel: una fila 499 (cancel del cliente) factura los
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
