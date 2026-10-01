use super::super::*;
use super::{TestUsageParams, fresh_conn, insert};
use crate::usage::UsageFilter;
use rusqlite::params;

#[test]
fn race_stats_counts_races() {
    let (conn, _p) = fresh_conn();

    // 3 races × (1 winner + 1 loser) = 6 rows total, 3 distinct
    // request_ids, race_total = 2.
    for i in 0..3i64 {
        // winner
        insert(
            &conn,
            TestUsageParams {
                request_id: &format!("r-{i}"),
                trace_id: &format!("wt-{i}"),
                provider: "openrouter",
                model: "openai/gpt-4o",
                connect_ms: Some(50),
                ttft_ms: Some(200),
                total_ms: 1000,
                race_total: 2,
                ..Default::default()
            },
        );
        // loser
        insert(
            &conn,
            TestUsageParams {
                request_id: &format!("r-{i}"),
                trace_id: &format!("lt-{i}"),
                provider: "openrouter",
                model: "openai/gpt-4o",
                connect_ms: Some(60),
                total_ms: 1500,
                race_total: 2,
                race_lost: true,
                ..Default::default()
            },
        );
    }

    let s = race_stats(&conn, &UsageFilter::default()).expect("race_stats");
    assert_eq!(s.total_races, 3, "3 distinct request_ids in races");
    assert_eq!(s.winners, 3, "3 winners");
    assert_eq!(s.losers, 3, "3 losers");
}

#[test]
fn race_stats_ignores_non_races() {
    let (conn, _p) = fresh_conn();

    // 5 sequential (race_total=1) rows — must not contribute.
    for i in 0..5i64 {
        insert(
            &conn,
            TestUsageParams {
                request_id: &format!("seq-{i}"),
                trace_id: &format!("seqt-{i}"),
                provider: "openrouter",
                model: "openai/gpt-4o",
                connect_ms: Some(50),
                ttft_ms: Some(200),
                total_ms: 1000,
                ..Default::default()
            },
        );
    }
    // 1 race to ensure the function returns non-zero when races exist.
    insert(
        &conn,
        TestUsageParams {
            request_id: "race-only",
            trace_id: "race-only-w",
            provider: "openrouter",
            model: "openai/gpt-4o",
            connect_ms: Some(50),
            ttft_ms: Some(200),
            total_ms: 1000,
            race_total: 2,
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: "race-only",
            trace_id: "race-only-l",
            provider: "openrouter",
            model: "openai/gpt-4o",
            connect_ms: Some(60),
            total_ms: 1500,
            race_total: 2,
            race_lost: true,
            ..Default::default()
        },
    );

    let s = race_stats(&conn, &UsageFilter::default()).expect("race_stats");
    assert_eq!(s.total_races, 1, "only the race counted");
    assert_eq!(s.winners, 1);
    assert_eq!(s.losers, 1);
}

#[test]
fn race_stats_wins_by_target() {
    let (conn, _p) = fresh_conn();

    // Need combo_targets rows to resolve priority_order via the LEFT JOIN.
    // The schema requires a combo + combo_targets with valid FKs.
    conn.execute(
        "INSERT INTO providers (id, name, base_url, auth_type, format) \
         VALUES ('openrouter', 'OpenRouter', 'https://x', 'bearer', 'openai')",
        [],
    )
    .expect("provider");
    conn.execute(
        "INSERT INTO combos (name, strategy, race_size) VALUES ('c1', 'priority', 2)",
        [],
    )
    .expect("combo");
    let combo_id: i64 = conn
        .query_row("SELECT id FROM combos WHERE name='c1'", [], |r| r.get(0))
        .expect("combo id");
    conn.execute(
        "INSERT INTO models (provider_id, model_id, target_format) \
         VALUES ('openrouter', 'openai/gpt-4o', 'openai')",
        [],
    )
    .expect("model");
    let model_row_id: i64 = conn
        .query_row(
            "SELECT id FROM models WHERE provider_id='openrouter'",
            [],
            |r| r.get(0),
        )
        .expect("model id");
    // Two targets with priority 10 and 20.
    conn.execute(
        "INSERT INTO combo_targets (combo_id, provider_id, account_id, model_row_id, priority_order) \
         VALUES (?1, 'openrouter', NULL, ?2, 10)",
        params![combo_id, model_row_id],
    )
    .expect("target 1");
    let target5: i64 = conn
        .query_row(
            "SELECT id FROM combo_targets WHERE priority_order=10",
            [],
            |r| r.get(0),
        )
        .expect("target5");
    conn.execute(
        "INSERT INTO combo_targets (combo_id, provider_id, account_id, model_row_id, priority_order) \
         VALUES (?1, 'openrouter', NULL, ?2, 20)",
        params![combo_id, model_row_id],
    )
    .expect("target 2");
    let target7: i64 = conn
        .query_row(
            "SELECT id FROM combo_targets WHERE priority_order=20",
            [],
            |r| r.get(0),
        )
        .expect("target7");

    // Race 1: target5 wins.
    insert(
        &conn,
        TestUsageParams {
            request_id: "race-A",
            trace_id: "race-A-w",
            provider: "openrouter",
            model: "openai/gpt-4o",
            connect_ms: Some(50),
            ttft_ms: Some(200),
            total_ms: 1000,
            race_total: 2,
            combo_target_id: Some(target5),
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: "race-A",
            trace_id: "race-A-l",
            provider: "openrouter",
            model: "openai/gpt-4o",
            connect_ms: Some(60),
            total_ms: 1500,
            race_total: 2,
            race_lost: true,
            combo_target_id: Some(target7),
            ..Default::default()
        },
    );
    // Race 2: target5 wins again.
    insert(
        &conn,
        TestUsageParams {
            request_id: "race-B",
            trace_id: "race-B-w",
            provider: "openrouter",
            model: "openai/gpt-4o",
            connect_ms: Some(50),
            ttft_ms: Some(200),
            total_ms: 1000,
            race_total: 2,
            combo_target_id: Some(target5),
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: "race-B",
            trace_id: "race-B-l",
            provider: "openrouter",
            model: "openai/gpt-4o",
            connect_ms: Some(60),
            total_ms: 1500,
            race_total: 2,
            race_lost: true,
            combo_target_id: Some(target7),
            ..Default::default()
        },
    );
    // Race 3: target7 wins.
    insert(
        &conn,
        TestUsageParams {
            request_id: "race-C",
            trace_id: "race-C-w",
            provider: "openrouter",
            model: "openai/gpt-4o",
            connect_ms: Some(50),
            ttft_ms: Some(200),
            total_ms: 1000,
            race_total: 2,
            combo_target_id: Some(target7),
            ..Default::default()
        },
    );
    insert(
        &conn,
        TestUsageParams {
            request_id: "race-C",
            trace_id: "race-C-l",
            provider: "openrouter",
            model: "openai/gpt-4o",
            connect_ms: Some(60),
            total_ms: 1500,
            race_total: 2,
            race_lost: true,
            combo_target_id: Some(target5),
            ..Default::default()
        },
    );

    let s = race_stats(&conn, &UsageFilter::default()).expect("race_stats");
    assert_eq!(s.total_races, 3);
    assert_eq!(s.winners, 3);
    assert_eq!(s.losers, 3);

    // target5 has 2 wins, target7 has 1 → ordered by count DESC.
    assert_eq!(s.wins_by_target.len(), 2);
    assert_eq!(s.wins_by_target[0], (target5, 2));
    assert_eq!(s.wins_by_target[1], (target7, 1));

    // avg_winner_position: target5 has priority 10 (x2), target7 has
    // priority 20 (x1). Mean = (10 + 10 + 20) / 3 = 13.333...
    let avg = s.avg_winner_position.expect("avg_winner_position present");
    assert!(
        (avg - (40.0 / 3.0)).abs() < 1e-9,
        "avg_winner_position = 40/3, got {avg}"
    );

    // MVP reserves this metric; we always return None.
    assert_eq!(s.avg_ttft_savings_ms, None);
}
