//! Billing-resolution tests for combo schedules: the band a request bills at,
//! the conservative bound admission reserves, and the historical snapshot.

use super::*;
use crate::conn::DbPool;
use chrono::{TimeZone as _, Timelike as _};
use openproxy_types::pricing_schedule::{OffPeakDate, PeakClock, PeakWindow, PricingRate};

const PEAK_INPUT: f64 = 6.0;
const PEAK_OUTPUT: f64 = 24.0;
const PEAK_CACHED: f64 = 3.0;
const OFF_INPUT: f64 = 2.0;
const OFF_OUTPUT: f64 = 8.0;

fn schedule() -> PricingSchedule {
    PricingSchedule {
        timezone: "UTC".to_string(),
        peak: PricingRate {
            input_per_1m: PEAK_INPUT,
            output_per_1m: PEAK_OUTPUT,
            cached_per_1m: Some(PEAK_CACHED),
        },
        off_peak: PricingRate {
            input_per_1m: OFF_INPUT,
            output_per_1m: OFF_OUTPUT,
            cached_per_1m: None,
        },
        peak_windows: vec![PeakWindow {
            days: vec![1, 2, 3, 4, 5],
            start: PeakClock::parse("06:00").expect("start"),
            end: PeakClock::parse("10:00").expect("end"),
        }],
        off_peak_dates: vec![OffPeakDate::parse("2026-11-18").expect("holiday")],
    }
}

/// Monday 2026-11-16 07:00 UTC — inside the 06:00–10:00 window.
fn peak_instant() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 16, 7, 0, 0)
        .single()
        .expect("peak instant")
}

/// Monday 2026-11-16 12:00 UTC — outside every window.
fn off_peak_instant() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 16, 12, 0, 0)
        .single()
        .expect("off-peak instant")
}

fn seed_combo(
    conn: &Connection,
    id: i64,
    flat: Option<(f64, f64)>,
    schedule: Option<&PricingSchedule>,
) {
    let (input, output) = match flat {
        Some((input, output)) => (Some(input), Some(output)),
        None => (None, None),
    };
    let json = schedule.map(|s| serde_json::to_string(s).expect("serialize schedule"));
    conn.execute(
        "INSERT INTO combos (id, name, strategy, pricing_input_per_1m, pricing_output_per_1m, \
         pricing_cached_per_1m, pricing_schedule) VALUES (?1, ?2, 'priority', ?3, ?4, NULL, ?5)",
        rusqlite::params![id, format!("combo-{id}"), input, output, json],
    )
    .expect("seed combo");
}

fn seed_model_price(conn: &Connection) {
    conn.execute(
        "INSERT INTO model_capabilities_sync \
         (provider_id, model_id, pricing_input_per_1m, pricing_output_per_1m) \
         VALUES ('test', 'test-model', 10.0, 30.0)",
        [],
    )
    .expect("seed model price");
}

#[test]
fn schedule_wins_over_the_flat_legacy_price() {
    let pool = DbPool::test_pool_with_prefix("openproxy-sched-flat").expect("open pool");
    let conn = pool.writer();
    // Flat 1.0/2.0 is the legacy price; the schedule must override it per band.
    seed_combo(&conn, 1, Some((1.0, 2.0)), Some(&schedule()));

    let peak = lookup_combo_price_at(&conn, 1, peak_instant()).expect("peak price");
    assert_eq!(peak.input_per_1m, PEAK_INPUT);
    assert_eq!(peak.output_per_1m, PEAK_OUTPUT);
    assert_eq!(peak.cached_per_1m, Some(PEAK_CACHED));

    let off = lookup_combo_price_at(&conn, 1, off_peak_instant()).expect("off-peak price");
    assert_eq!(off.input_per_1m, OFF_INPUT);
    assert_eq!(off.output_per_1m, OFF_OUTPUT);
    assert_eq!(off.cached_per_1m, None);

    // The legacy accessor keeps reporting the flat pair, so callers that only
    // want the flat price (public plan metadata) are unaffected.
    let flat = lookup_combo_price(&conn, 1).expect("flat price");
    assert_eq!(flat.input_per_1m, 1.0);
    assert_eq!(flat.output_per_1m, 2.0);
}

#[test]
fn schedule_prices_a_combo_with_no_flat_rates() {
    let pool = DbPool::test_pool_with_prefix("openproxy-sched-only").expect("open pool");
    let conn = pool.writer();
    seed_model_price(&conn);
    seed_combo(&conn, 2, None, Some(&schedule()));

    let peak = lookup_for_request_at(&conn, "test", "test-model", Some(2), peak_instant())
        .expect("scheduled combo is priced at peak");
    assert_eq!(peak.input_per_1m, PEAK_INPUT);
    assert_eq!(peak.output_per_1m, PEAK_OUTPUT);

    let off = lookup_for_request_at(&conn, "test", "test-model", Some(2), off_peak_instant())
        .expect("scheduled combo is priced off-peak");
    assert_eq!(off.input_per_1m, OFF_INPUT);

    // The schedule never leaks into the legacy accessor: no flat pair, no price.
    assert!(lookup_combo_price(&conn, 2).is_none());
}

#[test]
fn flat_price_is_untouched_without_a_schedule() {
    let pool = DbPool::test_pool_with_prefix("openproxy-sched-flat-only").expect("open pool");
    let conn = pool.writer();
    seed_model_price(&conn);
    seed_combo(&conn, 3, Some((1.0, 2.0)), None);

    for at in [peak_instant(), off_peak_instant()] {
        let price = lookup_for_request_at(&conn, "test", "test-model", Some(3), at)
            .expect("flat combo price");
        assert_eq!(price.input_per_1m, 1.0);
        assert_eq!(price.output_per_1m, 2.0);
    }
}

#[test]
fn a_combo_without_any_price_falls_back_to_the_model() {
    let pool = DbPool::test_pool_with_prefix("openproxy-sched-model-fallback").expect("open pool");
    let conn = pool.writer();
    seed_model_price(&conn);
    seed_combo(&conn, 4, None, None);

    let price = lookup_for_request_at(&conn, "test", "test-model", Some(4), peak_instant())
        .expect("model fallback");
    assert_eq!(price.input_per_1m, 10.0);
    assert_eq!(price.output_per_1m, 30.0);
}

#[test]
fn conservative_lookup_bounds_both_bands() {
    let pool = DbPool::test_pool_with_prefix("openproxy-sched-conservative").expect("open pool");
    let conn = pool.writer();
    seed_combo(&conn, 5, None, Some(&schedule()));

    let bound = lookup_for_request_conservative(&conn, "test", "test-model", Some(5))
        .expect("conservative price");
    assert_eq!(bound.input_per_1m, PEAK_INPUT);
    assert_eq!(bound.output_per_1m, PEAK_OUTPUT);
    assert_eq!(bound.cached_per_1m, Some(PEAK_CACHED));

    // Admission must never reserve less than the peak band, even though the
    // request may end up billed off-peak.
    let billed = lookup_for_request_at(&conn, "test", "test-model", Some(5), off_peak_instant())
        .expect("off-peak price");
    assert!(bound.input_per_1m >= billed.input_per_1m);
    assert!(bound.output_per_1m >= billed.output_per_1m);
}

#[test]
fn usage_lookup_reproduces_the_stored_instant() {
    let pool = DbPool::test_pool_with_prefix("openproxy-sched-history").expect("open pool");
    let conn = pool.writer();
    seed_model_price(&conn);
    seed_combo(&conn, 6, Some((1.0, 2.0)), Some(&schedule()));

    let peak = lookup_for_usage_at(&conn, "test", "test-model", Some(6), Some(peak_instant()))
        .expect("historical peak row");
    assert_eq!(peak.input_per_1m, PEAK_INPUT);

    let off = lookup_for_usage_at(
        &conn,
        "test",
        "test-model",
        Some(6),
        Some(off_peak_instant()),
    )
    .expect("historical off-peak row");
    assert_eq!(off.input_per_1m, OFF_INPUT);

    // No timestamp: a scheduled row cannot be re-priced without inventing a
    // band, so the lookup refuses instead of applying today's rate.
    assert!(lookup_for_usage_at(&conn, "test", "test-model", Some(6), None).is_none());

    // A flat combo has no time dimension: its history re-prices without a stamp.
    seed_combo(&conn, 7, Some((1.0, 2.0)), None);
    let flat =
        lookup_for_usage_at(&conn, "test", "test-model", Some(7), None).expect("flat history");
    assert_eq!(flat.input_per_1m, 1.0);
}

#[test]
fn unreadable_schedule_fails_closed_instead_of_billing_a_foreign_rate() {
    let pool = DbPool::test_pool_with_prefix("openproxy-sched-corrupt").expect("open pool");
    let conn = pool.writer();
    seed_model_price(&conn);
    // Truncated JSON plus a flat pair: neither may be billed, and the model's
    // 10.0/30.0 must not leak in either.
    conn.execute(
        "INSERT INTO combos (id, name, strategy, pricing_input_per_1m, pricing_output_per_1m, \
         pricing_schedule) VALUES (8, 'corrupt', 'priority', 1.0, 2.0, '{\"timezone\":')",
        [],
    )
    .expect("seed corrupt combo");

    assert!(lookup_combo_price_at(&conn, 8, peak_instant()).is_none());
    assert!(lookup_combo_price_conservative(&conn, 8).is_none());
    assert!(lookup_for_request_at(&conn, "test", "test-model", Some(8), peak_instant()).is_none());
    assert!(lookup_for_request_conservative(&conn, "test", "test-model", Some(8)).is_none());
    assert!(
        lookup_for_usage_at(&conn, "test", "test-model", Some(8), Some(peak_instant())).is_none()
    );
    assert_eq!(combo_is_scheduled(&conn, 8), Some(true));

    // The legacy metadata accessor still reports the flat pair: it describes the
    // stored columns, and only billing paths decide what is charged.
    let flat = lookup_combo_price(&conn, 8).expect("flat metadata");
    assert_eq!(flat.input_per_1m, 1.0);
}

#[test]
fn usage_timestamps_parse_both_stored_shapes() {
    let sqlite = parse_usage_timestamp("2026-11-16 07:00:00").expect("sqlite shape");
    assert_eq!(sqlite, peak_instant());
    let rfc = parse_usage_timestamp("2026-11-16T12:00:00Z").expect("rfc3339 shape");
    assert_eq!(rfc, off_peak_instant());
    let fractional =
        parse_usage_timestamp("2026-11-16T07:00:00.123Z").expect("fractional rfc3339 shape");
    assert_eq!(
        fractional.with_nanosecond(0).expect("truncate subsec"),
        peak_instant()
    );
    assert!(parse_usage_timestamp("").is_none());
    assert!(parse_usage_timestamp("yesterday").is_none());
    // A non-UTC offset is not guessed at: the row is left alone instead.
    assert!(parse_usage_timestamp("2026-11-16T07:00:00+02:00").is_none());
}

#[test]
fn a_parsable_but_invalid_calendar_is_unreadable() {
    let pool = DbPool::test_pool_with_prefix("openproxy-sched-invalid").expect("open pool");
    let conn = pool.writer();
    seed_model_price(&conn);
    // Parses as JSON, fails `validate()`: 25:00 is not a clock time, so serde
    // rejects it outright; the case that matters is a schedule that *parses* and
    // still cannot bill. Use a non-UTC timezone for that.
    let json = serde_json::to_string(&schedule())
        .expect("serialize")
        .replace(r#""timezone":"UTC""#, r#""timezone":"Europe/Madrid""#);
    conn.execute(
        "INSERT INTO combos (id, name, strategy, pricing_input_per_1m, pricing_output_per_1m, \
         pricing_schedule) VALUES (9, 'invalid', 'priority', 1.0, 2.0, ?1)",
        rusqlite::params![json],
    )
    .expect("seed invalid combo");

    assert!(lookup_combo_price_at(&conn, 9, peak_instant()).is_none());
    assert!(lookup_for_request_at(&conn, "test", "test-model", Some(9), peak_instant()).is_none());
    assert_eq!(combo_is_scheduled(&conn, 9), Some(true));
}

#[test]
fn missing_combo_is_unpriced_rather_than_free() {
    let pool = DbPool::test_pool_with_prefix("openproxy-sched-missing").expect("open pool");
    let conn = pool.writer();
    assert!(lookup_combo_price_at(&conn, 999, peak_instant()).is_none());
    assert!(lookup_combo_price_conservative(&conn, 999).is_none());
}
