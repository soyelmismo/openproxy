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

#[test]
fn test_compute_effective_context_window_recursive_and_inactive_filters() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();

    conn.execute_batch(
        "INSERT INTO providers (id, name, base_url, auth_type, format, active) VALUES
            ('p_active', 'Active Provider', 'https://example.com', 'none', 'openai', 1),
            ('p_inactive', 'Inactive Provider', 'https://example.com', 'none', 'openai', 0);
         INSERT INTO models (id, provider_id, model_id, target_format, context_length, active, custom) VALUES
            (201, 'p_active', 'model-256k', 'openai', 256000, 1, 0),
            (202, 'p_active', 'model-512k', 'openai', 512000, 1, 0),
            (203, 'p_active', 'model-1m', 'openai', 1048576, 1, 0),
            (204, 'p_active', 'model-inactive-128k', 'openai', 128000, 1, 0),
            (205, 'p_active', 'model-paused-128k', 'openai', 128000, 0, 0),
            (206, 'p_inactive', 'model-prov-inactive-128k', 'openai', 128000, 1, 0),
            (207, 'p_active', 'openai/gpt-4o', 'openai', NULL, 1, 0);
         INSERT INTO combos (id, name, strategy) VALUES
            (10, 'topics', 'priority'),
            (11, 'sub_news', 'priority'),
            (12, 'sub_chat', 'priority');
         -- Sub-combo 11 (sub_news): active 256k & 512k, plus inactive target 128k
         INSERT INTO combo_targets (id, combo_id, provider_id, model_row_id, priority_order, active) VALUES
            (101, 11, 'p_active', 201, 1, 1),
            (102, 11, 'p_active', 202, 2, 1),
            (103, 11, 'p_active', 204, 3, 0);
         -- Sub-combo 12 (sub_chat): active 1M, plus paused model 128k, plus inactive provider 128k
         INSERT INTO combo_targets (id, combo_id, provider_id, model_row_id, priority_order, active) VALUES
            (104, 12, 'p_active', 203, 1, 1),
            (105, 12, 'p_active', 205, 2, 1),
            (106, 12, 'p_inactive', 206, 3, 1);
         -- Parent combo 10 (topics): references sub_news (11) and sub_chat (12)
         INSERT INTO combo_targets (id, combo_id, provider_id, sub_combo_id, priority_order, active) VALUES
            (107, 10, 'p_active', 11, 1, 1),
            (108, 10, 'p_active', 12, 2, 1);"
    ).unwrap();

    // 1. sub_news effective context window: min(256k, 512k) = 256k (inactive 128k target 103 is ignored)
    let cw_sub1 = compute_effective_context_window(&conn, ComboId(11)).unwrap();
    assert_eq!(cw_sub1, Some(256_000));

    // 2. sub_chat effective context window: 1M (paused model 205 and inactive provider 206 are ignored)
    let cw_sub2 = compute_effective_context_window(&conn, ComboId(12)).unwrap();
    assert_eq!(cw_sub2, Some(1_048_576));

    // 3. topics effective context window: min(256k, 1M) = 256k
    let cw_topics = compute_effective_context_window(&conn, ComboId(10)).unwrap();
    assert_eq!(cw_topics, Some(256_000));

    // 4. list_targets_with_model populates context_length for sub-combos
    let targets_with_model = list_targets_with_model(&conn, ComboId(10)).unwrap();
    assert_eq!(targets_with_model.len(), 2);
    assert_eq!(targets_with_model[0].sub_combo_id, Some(ComboId(11)));
    assert_eq!(targets_with_model[0].context_length, Some(256_000));
    assert_eq!(targets_with_model[1].sub_combo_id, Some(ComboId(12)));
    assert_eq!(targets_with_model[1].context_length, Some(1_048_576));

    // 5. Inferred context length: add gpt-4o (context_length = NULL in DB) to sub_chat
    conn.execute(
        "INSERT INTO combo_targets (id, combo_id, provider_id, model_row_id, priority_order, active) VALUES (109, 12, 'p_active', 207, 4, 1)",
        [],
    ).unwrap();
    let cw_sub2_inferred = compute_effective_context_window(&conn, ComboId(12)).unwrap();
    assert_eq!(cw_sub2_inferred, Some(128_000)); // gpt-4o inferred as 128_000
    let cw_topics_inferred = compute_effective_context_window(&conn, ComboId(10)).unwrap();
    assert_eq!(cw_topics_inferred, Some(128_000));

    // 6. Override capping: set context_window override on topics
    // If override is 512k, but natural is 128k, capped to 128k
    conn.execute(
        "UPDATE combos SET context_window = 512000 WHERE id = 10",
        [],
    )
    .unwrap();
    assert_eq!(
        compute_effective_context_window(&conn, ComboId(10)).unwrap(),
        Some(128_000)
    );

    // If override is 64k (lower than 128k natural), capped lower to 64k
    conn.execute("UPDATE combos SET context_window = 64000 WHERE id = 10", [])
        .unwrap();
    assert_eq!(
        compute_effective_context_window(&conn, ComboId(10)).unwrap(),
        Some(64_000)
    );

    // Clearing override (NULL) returns natural 128k
    conn.execute("UPDATE combos SET context_window = NULL WHERE id = 10", [])
        .unwrap();
    assert_eq!(
        compute_effective_context_window(&conn, ComboId(10)).unwrap(),
        Some(128_000)
    );
}

#[test]
fn test_deep_nested_combos_alternating_overrides_and_depth_limit() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();

    conn.execute_batch(
        "INSERT INTO providers (id, name, base_url, auth_type, format, active) VALUES
            ('p_deep', 'Deep Provider', 'https://example.com', 'none', 'openai', 1);
         INSERT INTO models (id, provider_id, model_id, target_format, context_length, active, custom) VALUES
            (301, 'p_deep', 'model-leaf-512k', 'openai', 512000, 1, 0),
            (302, 'p_deep', 'model-leaf-256k', 'openai', 256000, 1, 0);
         -- Combos hierarchy: 1 -> 2 -> 3 -> 4 -> 5 -> 6 (leaf)
         INSERT INTO combos (id, name, strategy, context_window) VALUES
            (1, 'level_1', 'priority', NULL),
            (2, 'level_2', 'priority', 1000000), -- override 1M (higher than natural 256k -> capped to 256k)
            (3, 'level_3', 'priority', NULL),
            (4, 'level_4', 'priority', 300000),  -- override 300k (higher than natural 256k -> capped to 256k)
            (5, 'level_5', 'priority', NULL),
            (6, 'level_6', 'priority', NULL);
         -- Targets connecting the chain
         INSERT INTO combo_targets (id, combo_id, provider_id, sub_combo_id, priority_order, active) VALUES
            (401, 1, 'p_deep', 2, 1, 1),
            (402, 2, 'p_deep', 3, 1, 1),
            (403, 3, 'p_deep', 4, 1, 1),
            (404, 4, 'p_deep', 5, 1, 1);
         -- Leaf targets on 5: model 256k and model 512k
         INSERT INTO combo_targets (id, combo_id, provider_id, model_row_id, priority_order, active) VALUES
            (405, 5, 'p_deep', 301, 1, 1),
            (406, 5, 'p_deep', 302, 2, 1);"
    ).unwrap();

    // Level 5 natural = min(512k, 256k) = 256k
    assert_eq!(
        compute_effective_context_window(&conn, ComboId(5)).unwrap(),
        Some(256_000)
    );

    // Level 4 override = 300k, capped to natural 256k -> 256k
    assert_eq!(
        compute_effective_context_window(&conn, ComboId(4)).unwrap(),
        Some(256_000)
    );

    // Level 3 natural = inherited from level 4 (256k) -> 256k
    assert_eq!(
        compute_effective_context_window(&conn, ComboId(3)).unwrap(),
        Some(256_000)
    );

    // Level 2 override = 1M, capped to inherited 256k -> 256k
    assert_eq!(
        compute_effective_context_window(&conn, ComboId(2)).unwrap(),
        Some(256_000)
    );

    // Level 1 top-level = inherited 256k (depth 0 to 4 within MAX_SUB_COMBO_DEPTH = 5)
    assert_eq!(
        compute_effective_context_window(&conn, ComboId(1)).unwrap(),
        Some(256_000)
    );

    // If level 4 has lower override (64k < 256k natural), it constrains the whole chain upwards
    conn.execute("UPDATE combos SET context_window = 64000 WHERE id = 4", [])
        .unwrap();
    assert_eq!(
        compute_effective_context_window(&conn, ComboId(1)).unwrap(),
        Some(64_000)
    );

    // Reset level 4 override
    conn.execute("UPDATE combos SET context_window = NULL WHERE id = 4", [])
        .unwrap();
    assert_eq!(
        compute_effective_context_window(&conn, ComboId(1)).unwrap(),
        Some(256_000)
    );

    // Now test depth limit: connect 5 -> 6 -> 7 and put models in 7, making chain 1->2->3->4->5->6->7 (depth 6 > MAX_SUB_COMBO_DEPTH = 5)
    conn.execute("DELETE FROM combo_targets WHERE id IN (405, 406)", [])
        .unwrap();
    conn.execute(
        "INSERT INTO combos (id, name, strategy) VALUES (7, 'level_7', 'priority')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO combo_targets (id, combo_id, provider_id, sub_combo_id, priority_order, active) VALUES (407, 5, 'p_deep', 6, 1, 1)",
        [],
    ).unwrap();
    conn.execute(
        "INSERT INTO combo_targets (id, combo_id, provider_id, sub_combo_id, priority_order, active) VALUES (408, 6, 'p_deep', 7, 1, 1)",
        [],
    ).unwrap();
    conn.execute(
        "INSERT INTO combo_targets (id, combo_id, provider_id, model_row_id, priority_order, active) VALUES (409, 7, 'p_deep', 302, 1, 1)",
        [],
    ).unwrap();

    let err = compute_effective_context_window(&conn, ComboId(1)).unwrap_err();
    assert!(err.to_string().contains("max sub-combo depth"));
}

#[test]
fn test_zero_or_empty_subcombos_and_nonpositive_context_lengths() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();

    conn.execute_batch(
        "INSERT INTO providers (id, name, base_url, auth_type, format, active) VALUES
            ('p_val', 'Validation Provider', 'https://example.com', 'none', 'openai', 1);
         INSERT INTO models (id, provider_id, model_id, target_format, context_length, active, custom) VALUES
            (501, 'p_val', 'model-valid-256k', 'openai', 256000, 1, 0),
            (502, 'p_val', 'model-zero-cw', 'openai', 0, 1, 0),
            (503, 'p_val', 'model-neg-cw', 'openai', -100, 1, 0);
         INSERT INTO combos (id, name, strategy) VALUES
            (50, 'parent', 'priority'),
            (51, 'empty_sub', 'priority'),
            (52, 'valid_sub', 'priority'),
            (53, 'zero_cw_sub', 'priority');
         -- Sub-combo 52 (valid): active 256k
         INSERT INTO combo_targets (id, combo_id, provider_id, model_row_id, priority_order, active) VALUES
            (601, 52, 'p_val', 501, 1, 1);
         -- Sub-combo 53: zero and negative context lengths
         INSERT INTO combo_targets (id, combo_id, provider_id, model_row_id, priority_order, active) VALUES
            (602, 53, 'p_val', 502, 1, 1),
            (603, 53, 'p_val', 503, 2, 1);
         -- Parent combo 50: references empty_sub (51) and valid_sub (52)
         INSERT INTO combo_targets (id, combo_id, provider_id, sub_combo_id, priority_order, active) VALUES
            (604, 50, 'p_val', 51, 1, 1),
            (605, 50, 'p_val', 52, 2, 1);"
    ).unwrap();

    // 1. Empty sub-combo with 0 targets returns None
    assert_eq!(
        compute_effective_context_window(&conn, ComboId(51)).unwrap(),
        None
    );

    // 2. Sub-combo with 0 or negative context length is filtered out, returns None (not 0 or negative)
    assert_eq!(
        compute_effective_context_window(&conn, ComboId(53)).unwrap(),
        None
    );

    // 3. Parent combo ignores empty_sub (None) and correctly inherits from valid_sub (256k)
    assert_eq!(
        compute_effective_context_window(&conn, ComboId(50)).unwrap(),
        Some(256_000)
    );
}
