//! Regression tests for the audio branch of `recompute_costs`.
//!
//! Whisper-class rows have no token counts: their billable quantity is
//! `audio_ms`. Before this branch existed, `recompute_costs` only looked at
//! `prompt_tokens > 0`, so an audio row with a known duration and zero cost was
//! silently skipped and stayed at $0.00 forever.

use crate::models_dev_sync::backfill::recompute_costs;
use rusqlite::Connection;

fn seed_usage(conn: &Connection, id: i64, model: &str, audio_ms: Option<i64>, cost: f64) {
    conn.execute(
        "INSERT INTO usage (id, request_id, trace_id, provider_id, upstream_model_id,
                            status_code, total_ms, audio_ms, cost_usd, created_at, endpoint_kind)
         VALUES (?1, ?2, ?3, 'groq', ?4, 200, 500, ?5, ?6, '2026-10-29T00:00:00Z', 'audio')",
        rusqlite::params![
            id,
            format!("req-{id}"),
            format!("tr-{id}"),
            model,
            audio_ms,
            cost
        ],
    )
    .unwrap();
}

#[test]
fn recompute_prices_audio_row_from_duration() {
    let conn = openproxy_db::testing::open_in_memory();
    // 4 s of whisper-large-v3-turbo at $0.04/h.
    seed_usage(&conn, 1, "whisper-large-v3-turbo", Some(4_000), 0.0);

    let updated = recompute_costs(&conn).unwrap();
    assert_eq!(updated, 1, "the audio row must be re-priced");

    let cost: f64 = conn
        .query_row("SELECT cost_usd FROM usage WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    let expected = 4.0 / 3600.0 * 0.04;
    assert!(
        (cost - expected).abs() < 1e-15,
        "expected {expected}, got {cost}"
    );
}

#[test]
fn recompute_skips_audio_row_without_duration() {
    let conn = openproxy_db::testing::open_in_memory();
    // No duration and no tokens: nothing billable, so the row must not be
    // given a fabricated cost.
    seed_usage(&conn, 1, "whisper-large-v3", None, 0.0);

    let updated = recompute_costs(&conn).unwrap();
    assert_eq!(updated, 0);

    let cost: f64 = conn
        .query_row("SELECT cost_usd FROM usage WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(cost, 0.0);
}

#[test]
fn recompute_uses_model_rate_not_provider() {
    // The same model served by two different providers must cost the same:
    // the catalog is keyed by model, never by provider.
    let conn = openproxy_db::testing::open_in_memory();
    conn.execute(
        "INSERT INTO usage (id, request_id, trace_id, provider_id, upstream_model_id,
                            status_code, total_ms, audio_ms, cost_usd, created_at, endpoint_kind)
         VALUES (1, 'r1', 't1', 'groq', 'whisper-large-v3', 200, 500, 3600000, 0.0,
                 '2026-10-29T00:00:00Z', 'audio')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO usage (id, request_id, trace_id, provider_id, upstream_model_id,
                            status_code, total_ms, audio_ms, cost_usd, created_at, endpoint_kind)
         VALUES (2, 'r2', 't2', 'some-reseller', 'openai/whisper-large-v3', 200, 500, 3600000, 0.0,
                 '2026-10-29T00:00:00Z', 'audio')",
        [],
    )
    .unwrap();

    let updated = recompute_costs(&conn).unwrap();
    assert_eq!(updated, 2);

    let c1: f64 = conn
        .query_row("SELECT cost_usd FROM usage WHERE id = 1", [], |r| r.get(0))
        .unwrap();
    let c2: f64 = conn
        .query_row("SELECT cost_usd FROM usage WHERE id = 2", [], |r| r.get(0))
        .unwrap();
    assert!((c1 - 0.111).abs() < 1e-12, "groq row: {c1}");
    assert!((c2 - 0.111).abs() < 1e-12, "reseller row: {c2}");
}
