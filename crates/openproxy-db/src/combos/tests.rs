//! Unit tests for combos repository.

use super::*;
use crate::conn::DbPool;
use openproxy_types::MAX_SUB_COMBO_DEPTH;
use openproxy_types::combos::Strategy;
use openproxy_types::error::CoreError;
use openproxy_types::ids::{ComboId, ComboTargetId};
use std::path::PathBuf;

fn fresh_pool() -> (DbPool, PathBuf) {
    let pool = DbPool::test_pool_with_prefix("openproxy-crud-test").expect("open pool");
    let path = pool.path().to_path_buf();
    (pool, path)
}

fn seed_chain(conn: &rusqlite::Connection, count: i64, links: &[(i64, i64, i64)]) {
    conn.execute("INSERT INTO providers (id, name, base_url, auth_type, format) VALUES ('p1', 'P1', 'url', 'bearer', 'openai')", []).unwrap();
    for i in 1..=count {
        conn.execute(
            "INSERT INTO combos (id, name, strategy) VALUES (?1, ?2, 'priority')",
            rusqlite::params![i, format!("c{i}")],
        )
        .unwrap();
    }
    for &(from, sub, prio) in links {
        conn.execute("INSERT INTO combo_targets (combo_id, provider_id, sub_combo_id, priority_order) VALUES (?1, 'p1', ?2, ?3)", rusqlite::params![from, sub, prio]).unwrap();
    }
}

#[test]
fn test_create_combo_validation() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();

    let id = create_combo(&conn, "c1", Strategy::Priority, 2).expect("create");
    let combo = get_combo(&conn, id).unwrap().unwrap();
    assert_eq!(combo.name, "c1");
    assert_eq!(combo.strategy, Strategy::Priority);
    assert_eq!(combo.race_size, 2);

    for bad_size in [0, 9] {
        let err = create_combo(&conn, "bad", Strategy::RoundRobin, bad_size).unwrap_err();
        assert!(
            matches!(err, CoreError::Validation(ref msg) if msg.contains("race_size must be in 1..=8"))
        );
    }

    let dup_err = create_combo(&conn, "c1", Strategy::Priority, 1).unwrap_err();
    assert!(
        matches!(dup_err, CoreError::Validation(ref msg) if msg.contains("combo name already exists"))
    );

    let shuffle_err = create_combo(&conn, "shuf", Strategy::Shuffle, 1).unwrap_err();
    assert!(
        matches!(shuffle_err, CoreError::Database { ref message, .. } if message.contains("insert combo"))
    );
}

#[test]
fn test_update_strategy() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let id = create_combo(&conn, "test_update_strategy", Strategy::Priority, 1).unwrap();

    update_strategy(&conn, id, "round_robin").unwrap();
    assert_eq!(
        get_combo(&conn, id).unwrap().unwrap().strategy,
        Strategy::RoundRobin
    );

    let err = update_strategy(&conn, id, "fifo").unwrap_err();
    assert!(matches!(err, CoreError::Validation(ref msg) if msg.contains("invalid strategy")));
    assert_eq!(
        get_combo(&conn, id).unwrap().unwrap().strategy,
        Strategy::RoundRobin
    );

    assert!(matches!(
        update_strategy(&conn, ComboId(9999), "priority").unwrap_err(),
        CoreError::ComboNotFound(9999)
    ));
}

#[test]
fn test_combo_in_chain_scenarios() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_chain(&conn, 3, &[(1, 2, 1), (2, 3, 1)]);

    assert!(!combo_in_chain(&conn, ComboId(1), ComboId(3), MAX_SUB_COMBO_DEPTH).unwrap());
    assert!(combo_in_chain(&conn, ComboId(1), ComboId(1), MAX_SUB_COMBO_DEPTH).unwrap());
}

#[test]
fn test_combo_in_chain_has_cycle() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_chain(&conn, 3, &[(1, 2, 1), (2, 3, 1), (3, 1, 1)]);

    assert!(combo_in_chain(&conn, ComboId(1), ComboId(1), MAX_SUB_COMBO_DEPTH).unwrap());
    assert!(combo_in_chain(&conn, ComboId(1), ComboId(2), MAX_SUB_COMBO_DEPTH).unwrap());
}

#[test]
fn test_combo_in_chain_max_depth_exceeded() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_chain(&conn, 5, &[(1, 2, 1), (2, 3, 1), (3, 4, 1), (4, 5, 1)]);

    assert!(!combo_in_chain(&conn, ComboId(5), ComboId(1), 3).unwrap());
    assert!(combo_in_chain(&conn, ComboId(5), ComboId(1), 4).unwrap());
}

#[test]
fn test_combo_in_chain_mutual_cycle() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_chain(&conn, 3, &[(1, 2, 1), (2, 1, 1)]);

    assert!(combo_in_chain(&conn, ComboId(2), ComboId(1), 10).unwrap());
    assert!(combo_in_chain(&conn, ComboId(1), ComboId(2), 10).unwrap());
    assert!(!combo_in_chain(&conn, ComboId(3), ComboId(1), 10).unwrap());
}

#[test]
fn test_combo_in_chain_multi_branch() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_chain(&conn, 4, &[(1, 2, 1), (1, 3, 2), (3, 4, 1)]);
    assert!(combo_in_chain(&conn, ComboId(4), ComboId(1), 10).unwrap());
}

#[test]
fn test_combo_target_thinking_effort_crud() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_chain(&conn, 2, &[(1, 2, 1)]);
    let tid = ComboTargetId(1);

    assert_eq!(
        get_target(&conn, tid).unwrap().unwrap().thinking_effort,
        None
    );

    update_target_thinking_effort(&conn, tid, Some("high")).unwrap();
    assert_eq!(
        get_target(&conn, tid)
            .unwrap()
            .unwrap()
            .thinking_effort
            .as_deref(),
        Some("high")
    );
    assert_eq!(
        list_targets_with_model(&conn, ComboId(1)).unwrap()[0]
            .thinking_effort
            .as_deref(),
        Some("high")
    );

    update_target_thinking_effort(&conn, tid, None).unwrap();
    assert_eq!(
        get_target(&conn, tid).unwrap().unwrap().thinking_effort,
        None
    );

    update_target_thinking_effort(&conn, tid, Some("medium")).unwrap();
    update_target_thinking_effort(&conn, tid, Some("passthrough")).unwrap();
    assert_eq!(
        get_target(&conn, tid).unwrap().unwrap().thinking_effort,
        None
    );
}

#[test]
fn test_list_targets_excludes_paused_models_and_model_cooldowns() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();

    conn.execute_batch(
        "INSERT INTO providers (id, name, base_url, auth_type, format, active) VALUES ('p1', 'P1', 'https://example.com', 'none', 'openai', 1);
         INSERT INTO models (id, provider_id, model_id, target_format, active, custom) VALUES (101, 'p1', 'model-active', 'openai', 1, 0), (102, 'p1', 'model-paused', 'openai', 0, 0), (103, 'p1', 'model-cooldown', 'openai', 1, 0);
         INSERT INTO combos (id, name, strategy) VALUES (1, 'c1', 'priority'), (2, 'c2', 'priority');
         INSERT INTO combo_targets (id, combo_id, provider_id, model_row_id, priority_order, active) VALUES (1, 1, 'p1', 101, 1, 1), (2, 1, 'p1', 102, 2, 1), (3, 1, 'p1', 103, 3, 1), (4, 2, 'p1', 103, 1, 1);"
    ).unwrap();

    let targets = list_targets(&conn, ComboId(1)).unwrap();
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0].id.0, 1);
    assert_eq!(targets[1].id.0, 3);

    crate::cooldowns::record_cooldown(
        &conn,
        ComboTargetId(3),
        "timeout",
        openproxy_types::CooldownMode::Flat,
        300,
        300,
        2,
    )
    .unwrap();

    let targets_c1 = list_targets(&conn, ComboId(1)).unwrap();
    assert_eq!(targets_c1.len(), 1);
    assert_eq!(targets_c1[0].id.0, 1);

    let targets_c2 = list_targets(&conn, ComboId(2)).unwrap();
    assert!(targets_c2.is_empty());
}
