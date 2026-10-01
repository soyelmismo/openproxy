use super::*;
use openproxy_types::TargetFormat;
use openproxy_types::combos::{AddTargetInput, Strategy};
use openproxy_types::ids::{ComboId, ModelRowId, ProviderId};
use openproxy_types::providers::AuthType;
use std::sync::Arc;

fn setup_test_pipeline() -> (
    crate::Pipeline,
    Arc<parking_lot::Mutex<rusqlite::Connection>>,
    Arc<openproxy_db::secrets::MasterKey>,
    tokio::runtime::Runtime,
) {
    let (_pool, conn_arc, _path) = crate::test_utils::fresh_pool();
    let master_key = Arc::new(openproxy_db::secrets::MasterKey::generate().unwrap());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let _enter = rt.enter();
    let pipeline = crate::Pipeline::new(
        Arc::clone(&conn_arc),
        crate::test_utils::test_config(Arc::clone(&master_key)),
    );
    (pipeline, conn_arc, master_key, rt)
}

#[test]
fn test_missing_account_records_cooldown_and_resolves_empty() {
    let (pipeline, conn_arc, _master_key, rt) = setup_test_pipeline();
    let _enter = rt.enter();

    let (combo_id, target_id) = {
        let conn = conn_arc.lock();
        let m_id = crate::test_utils::seed_provider_and_model(
            &conn,
            "openai",
            "gpt-4o",
            TargetFormat::Openai,
        );
        let c_id = crate::test_utils::combos::create_combo(
            &conn,
            "smart-combo",
            Strategy::Priority,
            1,
        )
        .unwrap();
        let t_id = crate::test_utils::combos::add_target(
            &conn,
            AddTargetInput {
                combo_id: c_id,
                provider_id: ProviderId::new("openai"),
                account_id: None,
                model_row_id: Some(m_id),
                sub_combo_id: None,
                priority_order: 1,
                weight: 1,
            },
        )
        .unwrap();
        (c_id, t_id)
    };

    let targets = pipeline.repo().list_targets(combo_id).unwrap();
    assert_eq!(targets.len(), 1);

    let resolved = rt.block_on(pipeline.resolve_combo_targets_full(targets));
    assert!(resolved.is_empty(), "target without account must not be resolved");

    let conn = conn_arc.lock();
    let cd = openproxy_db::cooldowns::get_for_target(&conn, target_id).unwrap();
    assert!(cd.is_some(), "cooldown must be recorded for target without account");
    let cd = cd.unwrap();
    assert_eq!(cd.combo_target_id, target_id);
    assert_eq!(cd.failure_count, 1);
    let reason = cd.reason.unwrap();
    assert!(reason.contains("smart-combo"), "reason must contain combo name: {reason}");
    assert!(reason.contains("openai"), "reason must contain provider: {reason}");
    assert!(reason.contains("gpt-4o"), "reason must contain model: {reason}");
    assert!(!reason.contains("sk-"), "reason must not leak tokens");
}

#[test]
fn test_second_healthy_target_same_provider_resolves_without_cooldown() {
    let (pipeline, conn_arc, master_key, rt) = setup_test_pipeline();
    let _enter = rt.enter();

    let (combo_id, bad_tid, good_tid) = {
        let conn = conn_arc.lock();
        let m_id = crate::test_utils::seed_provider_and_model(
            &conn,
            "openai",
            "gpt-4o",
            TargetFormat::Openai,
        );
        let acc_id = openproxy_db::accounts::create(
            &conn,
            &ProviderId::new("openai"),
            Some("sk-healthy-key"),
            master_key.as_ref(),
            Some("acc-healthy"),
            10,
            None,
        )
        .unwrap();
        let c_id = crate::test_utils::combos::create_combo(
            &conn,
            "pair-combo",
            Strategy::Priority,
            1,
        )
        .unwrap();
        let bad_id = crate::test_utils::combos::add_target(
            &conn,
            AddTargetInput {
                combo_id: c_id,
                provider_id: ProviderId::new("openai"),
                account_id: None,
                model_row_id: Some(m_id),
                sub_combo_id: None,
                priority_order: 1,
                weight: 1,
            },
        )
        .unwrap();
        let good_id = crate::test_utils::combos::add_target(
            &conn,
            AddTargetInput {
                combo_id: c_id,
                provider_id: ProviderId::new("openai"),
                account_id: Some(acc_id),
                model_row_id: Some(m_id),
                sub_combo_id: None,
                priority_order: 2,
                weight: 1,
            },
        )
        .unwrap();
        (c_id, bad_id, good_id)
    };

    let targets = pipeline.repo().list_targets(combo_id).unwrap();
    assert_eq!(targets.len(), 2);

    let resolved = rt.block_on(pipeline.resolve_combo_targets_full(targets));
    assert_eq!(resolved.len(), 1, "healthy target must be resolved");
    assert_eq!(resolved[0].target.id, good_tid);
    assert_eq!(resolved[0].api_key, "sk-healthy-key");

    let conn = conn_arc.lock();
    let cd_bad = openproxy_db::cooldowns::get_for_target(&conn, bad_tid).unwrap();
    assert!(cd_bad.is_some(), "bad target must be in cooldown");

    let cd_good = openproxy_db::cooldowns::get_for_target(&conn, good_tid).unwrap();
    assert!(cd_good.is_none(), "healthy target must NOT be in cooldown");
}

#[test]
fn test_anonymous_none_and_fallback_resolve_without_cooldown() {
    let (pipeline, conn_arc, _master_key, rt) = setup_test_pipeline();
    let _enter = rt.enter();

    // Case A: Provider with AuthType::None (e.g. local-ollama)
    let (c_none_id, anon_none_tid) = {
        let conn = conn_arc.lock();
        crate::test_utils::seed_provider(&conn, "local-ollama", AuthType::None);
        conn.execute(
            "INSERT INTO models(provider_id, model_id, target_format) VALUES ('local-ollama', 'llama3', 'openai')",
            [],
        )
        .unwrap();
        let m_id: i64 = conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0)).unwrap();
        let c_id = crate::test_utils::combos::create_combo(
            &conn,
            "anon-none-combo",
            Strategy::Priority,
            1,
        )
        .unwrap();
        let tid = crate::test_utils::combos::add_target(
            &conn,
            AddTargetInput {
                combo_id: c_id,
                provider_id: ProviderId::new("local-ollama"),
                account_id: None,
                model_row_id: Some(ModelRowId(m_id)),
                sub_combo_id: None,
                priority_order: 1,
                weight: 1,
            },
        )
        .unwrap();
        (c_id, tid)
    };

    let targets_none = pipeline.repo().list_targets(c_none_id).unwrap();
    let resolved_none = rt.block_on(pipeline.resolve_combo_targets_full(targets_none));
    assert_eq!(resolved_none.len(), 1, "anonymous AuthType::None target must resolve");
    assert_eq!(resolved_none[0].target.id, anon_none_tid);
    assert_eq!(resolved_none[0].api_key, "");

    {
        let conn = conn_arc.lock();
        let cd_none = openproxy_db::cooldowns::get_for_target(&conn, anon_none_tid).unwrap();
        assert!(cd_none.is_none(), "anonymous AuthType::None target must not be in cooldown");
    }

    // Case B: Real is_anonymous_fallback provider from adapters (e.g. horde with API auth type)
    let (c_fallback_id, anon_fallback_tid) = {
        let conn = conn_arc.lock();
        crate::test_utils::seed_provider(&conn, "horde", AuthType::Bearer);
        conn.execute(
            "INSERT INTO models(provider_id, model_id, target_format) VALUES ('horde', 'stable-diffusion', 'openai')",
            [],
        )
        .unwrap();
        let m_id: i64 = conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0)).unwrap();
        let c_id = crate::test_utils::combos::create_combo(
            &conn,
            "anon-fallback-combo",
            Strategy::Priority,
            1,
        )
        .unwrap();
        let tid = crate::test_utils::combos::add_target(
            &conn,
            AddTargetInput {
                combo_id: c_id,
                provider_id: ProviderId::new("horde"),
                account_id: None,
                model_row_id: Some(ModelRowId(m_id)),
                sub_combo_id: None,
                priority_order: 1,
                weight: 1,
            },
        )
        .unwrap();
        (c_id, tid)
    };

    let targets_fb = pipeline.repo().list_targets(c_fallback_id).unwrap();
    let resolved_fb = rt.block_on(pipeline.resolve_combo_targets_full(targets_fb));
    assert_eq!(resolved_fb.len(), 1, "is_anonymous_fallback provider target must resolve");
    assert_eq!(resolved_fb[0].target.id, anon_fallback_tid);
    assert_eq!(resolved_fb[0].api_key, "");

    {
        let conn = conn_arc.lock();
        let cd_fb = openproxy_db::cooldowns::get_for_target(&conn, anon_fallback_tid).unwrap();
        assert!(cd_fb.is_none(), "is_anonymous_fallback target must not be in cooldown");
    }
}

#[test]
fn test_cooldown_overrides_none_and_zero_preserved() {
    let (pipeline, conn_arc, _master_key, rt) = setup_test_pipeline();
    let _enter = rt.enter();

    let (c_id, t_none_id, t_zero_id) = {
        let conn = conn_arc.lock();
        let m_id = crate::test_utils::seed_provider_and_model(
            &conn,
            "openai",
            "gpt-4o",
            TargetFormat::Openai,
        );
        let c_id = crate::test_utils::combos::create_combo(
            &conn,
            "override-combo",
            Strategy::Priority,
            1,
        )
        .unwrap();

        let t_none = crate::test_utils::combos::add_target(
            &conn,
            AddTargetInput {
                combo_id: c_id,
                provider_id: ProviderId::new("openai"),
                account_id: None,
                model_row_id: Some(m_id),
                sub_combo_id: None,
                priority_order: 1,
                weight: 1,
            },
        )
        .unwrap();
        conn.execute(
            "UPDATE combo_targets SET cooldown_mode = 'none' WHERE id = ?1",
            rusqlite::params![t_none.0],
        )
        .unwrap();

        let t_zero = crate::test_utils::combos::add_target(
            &conn,
            AddTargetInput {
                combo_id: c_id,
                provider_id: ProviderId::new("openai"),
                account_id: None,
                model_row_id: Some(m_id),
                sub_combo_id: None,
                priority_order: 2,
                weight: 1,
            },
        )
        .unwrap();
        conn.execute(
            "UPDATE combo_targets SET cooldown_base_secs = 0 WHERE id = ?1",
            rusqlite::params![t_zero.0],
        )
        .unwrap();

        (c_id, t_none, t_zero)
    };

    let targets = pipeline.repo().list_targets(c_id).unwrap();
    assert_eq!(targets.len(), 2);

    let resolved = rt.block_on(pipeline.resolve_combo_targets_full(targets));
    assert!(resolved.is_empty());

    let conn = conn_arc.lock();
    let cd_none = openproxy_db::cooldowns::get_for_target(&conn, t_none_id).unwrap();
    assert!(cd_none.is_none(), "target with cooldown_mode = none must not get cooldown");

    let cd_zero = openproxy_db::cooldowns::get_for_target(&conn, t_zero_id).unwrap();
    assert!(cd_zero.is_none(), "target with cooldown_base_secs = 0 must not get cooldown");
}

#[test]
fn test_two_calls_with_active_cooldown_do_not_re_increment_nor_prolong() {
    let (pipeline, conn_arc, _master_key, rt) = setup_test_pipeline();
    let _enter = rt.enter();

    let (combo_id, target_id) = {
        let conn = conn_arc.lock();
        let m_id = crate::test_utils::seed_provider_and_model(
            &conn,
            "openai",
            "gpt-4o",
            TargetFormat::Openai,
        );
        let c_id = crate::test_utils::combos::create_combo(
            &conn,
            "idempotent-combo",
            Strategy::Priority,
            1,
        )
        .unwrap();
        let t_id = crate::test_utils::combos::add_target(
            &conn,
            AddTargetInput {
                combo_id: c_id,
                provider_id: ProviderId::new("openai"),
                account_id: None,
                model_row_id: Some(m_id),
                sub_combo_id: None,
                priority_order: 1,
                weight: 1,
            },
        )
        .unwrap();
        (c_id, t_id)
    };

    let targets = pipeline.repo().list_targets(combo_id).unwrap();
    assert_eq!(targets.len(), 1);

    // First call: triggers cooldown
    let resolved1 = rt.block_on(pipeline.resolve_combo_targets_full(targets.clone()));
    assert!(resolved1.is_empty());

    let (first_count, first_until) = {
        let conn = conn_arc.lock();
        let cd = openproxy_db::cooldowns::get_for_target(&conn, target_id).unwrap().unwrap();
        (cd.failure_count, cd.cooldown_until)
    };
    assert_eq!(first_count, 1);

    // Second call: active cooldown exists, must not re-increment or prolong
    let resolved2 = rt.block_on(pipeline.resolve_combo_targets_full(targets));
    assert!(resolved2.is_empty());

    let (second_count, second_until) = {
        let conn = conn_arc.lock();
        let cd = openproxy_db::cooldowns::get_for_target(&conn, target_id).unwrap().unwrap();
        (cd.failure_count, cd.cooldown_until)
    };
    assert_eq!(second_count, 1, "failure count must remain 1");
    assert_eq!(second_until, first_until, "cooldown_until must remain unchanged");
}

#[test]
fn test_nested_target_uses_owner_sub_combo_info() {
    let (pipeline, conn_arc, _master_key, rt) = setup_test_pipeline();
    let _enter = rt.enter();

    let (root_c_id, sub_c_id, target_id) = {
        let conn = conn_arc.lock();
        let m_id = crate::test_utils::seed_provider_and_model(
            &conn,
            "anthropic",
            "claude-3-5-sonnet",
            TargetFormat::Anthropic,
        );
        let root_c_id = crate::test_utils::combos::create_combo(
            &conn,
            "root-combo",
            Strategy::Priority,
            1,
        )
        .unwrap();
        let sub_c_id = crate::test_utils::combos::create_combo(
            &conn,
            "sub-combo",
            Strategy::Priority,
            1,
        )
        .unwrap();

        let t_id = crate::test_utils::combos::add_target(
            &conn,
            AddTargetInput {
                combo_id: sub_c_id,
                provider_id: ProviderId::new("anthropic"),
                account_id: None,
                model_row_id: Some(m_id),
                sub_combo_id: None,
                priority_order: 1,
                weight: 1,
            },
        )
        .unwrap();

        // Link root -> sub
        crate::test_utils::combos::add_target(
            &conn,
            AddTargetInput {
                combo_id: root_c_id,
                provider_id: ProviderId::new("anthropic"),
                account_id: None,
                model_row_id: None,
                sub_combo_id: Some(sub_c_id),
                priority_order: 1,
                weight: 1,
            },
        )
        .unwrap();

        (root_c_id, sub_c_id, t_id)
    };

    // Flatten from root combo with an empty visited set
    let mut visited: Vec<ComboId> = Vec::new();
    let targets = pipeline
        .repo()
        .resolve_combo_to_targets(root_c_id, &mut visited, 0)
        .unwrap();
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].combo_id, sub_c_id, "target must maintain sub_combo owner");

    let resolved = rt.block_on(pipeline.resolve_combo_targets_full(targets));
    assert!(resolved.is_empty());

    let conn = conn_arc.lock();
    let cd = openproxy_db::cooldowns::get_for_target(&conn, target_id).unwrap().unwrap();
    let reason = cd.reason.unwrap();
    assert!(reason.contains("sub-combo"), "reason must reflect sub-combo owner: {reason}");
    assert!(!reason.contains("root-combo"), "reason must NOT use root combo: {reason}");
    assert!(reason.contains("anthropic"));
    assert!(reason.contains("claude-3-5-sonnet"));
}
