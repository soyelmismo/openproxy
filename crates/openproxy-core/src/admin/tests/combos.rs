use super::fresh_pool;
use crate::admin::combos::{
    AddTargetInput, CreateComboInput, add_target_to_combo, create_combo, delete_combo_target,
    list_combo_targets, list_combo_targets_with_model, list_combos, reorder_combo_targets,
};
use crate::admin::providers::{CreateProviderInput, create_provider};
use crate::error::CoreError;
use crate::ids::{ComboId, ComboTargetId, ModelRowId};
use openproxy_db::combos;
use rusqlite::Connection;

fn ci(name: &str, strategy: &str) -> CreateComboInput {
    CreateComboInput {
        name: name.into(),
        strategy: strategy.into(),
        race_size: None,
        priority_mode: None,
        cooldown_mode: None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
        decision_model: None,
        decision_timeout_ms: None,
    }
}

fn ati(pid: &str, mid: ModelRowId, prio: i32) -> AddTargetInput {
    AddTargetInput {
        provider_id: pid.into(),
        account_id: None,
        model_row_id: Some(mid),
        sub_combo_id: None,
        priority_order: prio,
        description: None,
    }
}

fn seed_prov(conn: &Connection, pid: &str) {
    create_provider(
        conn,
        CreateProviderInput {
            rate_limit_scope: None,
            id: pid.into(),
            name: pid.into(),
            base_url: "https://example.com".into(),
            auth_type: "bearer".into(),
            format: "openai".into(),
            extra_headers_json: None,
        },
    )
    .expect("seed provider");
}

fn seed_model(conn: &Connection, pid: &str, mid: &str, disp: Option<&str>) -> ModelRowId {
    conn.execute(
        "INSERT INTO models(provider_id, model_id, target_format, display_name) VALUES (?1, ?2, 'openai', ?3)",
        rusqlite::params![pid, mid, disp],
    ).expect("seed model");
    ModelRowId(
        conn.query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("rowid"),
    )
}

#[test]
fn create_combo_with_targets_then_list() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    seed_prov(&conn, "p1");
    seed_prov(&conn, "p2");
    let m1 = seed_model(&conn, "p1", "m1", None);
    let m2 = seed_model(&conn, "p2", "m2", None);

    let combo_id = create_combo(&conn, &ci("primary", "priority")).expect("create combo");
    let listed = list_combos(&conn).expect("list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, combo_id);
    assert_eq!(
        listed[0].strategy,
        openproxy_types::combos::Strategy::Priority
    );
    assert_eq!(listed[0].race_size, 1);

    let mut rr = ci("rr", "round_robin");
    rr.race_size = Some(3);
    let combo_rr = create_combo(&conn, &rr).expect("create rr");
    let c = list_combos(&conn)
        .expect("list")
        .into_iter()
        .find(|c| c.id == combo_rr)
        .unwrap();
    assert_eq!(c.race_size, 3);
    assert_eq!(c.strategy, openproxy_types::combos::Strategy::RoundRobin);

    let t1 = add_target_to_combo(&conn, combo_id, ati("p1", m1, 10)).expect("add t1");
    let t2 = add_target_to_combo(&conn, combo_id, ati("p2", m2, 20)).expect("add t2");

    let targets = list_combo_targets(&conn, combo_id).expect("list targets");
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0].id, t1);
    assert_eq!(targets[1].id, t2);
    assert_eq!(targets[0].priority_order, 10);
    assert_eq!(targets[1].priority_order, 20);

    assert!(matches!(
        create_combo(&conn, &ci("bad", "fifo")).expect_err("bad"),
        CoreError::Validation(_)
    ));
    assert!(matches!(
        add_target_to_combo(&conn, ComboId(99999), ati("p1", m1, 0)).expect_err("bad"),
        CoreError::ComboNotFound(_)
    ));
    assert!(matches!(
        add_target_to_combo(&conn, combo_id, ati("p1", ModelRowId(88888), 0)).expect_err("bad"),
        CoreError::Validation(_)
    ));
}

struct ComboTargetFixture {
    combo_id: ComboId,
    m1: ModelRowId,
    t1: ComboTargetId,
    t2: ComboTargetId,
}

fn seed_combo_with_two_targets(conn: &Connection) -> ComboTargetFixture {
    seed_prov(conn, "p1");
    seed_prov(conn, "p2");
    let m1 = seed_model(conn, "p1", "m1", Some("Model One"));
    let m2 = seed_model(conn, "p2", "m2", Some("Model Two"));
    let combo_id = create_combo(conn, &ci("two-target", "priority")).expect("create combo");
    combos::clear_targets(conn, combo_id).expect("clear auto-populate");
    let t1 = add_target_to_combo(conn, combo_id, ati("p1", m1, 10)).expect("add t1");
    let t2 = add_target_to_combo(conn, combo_id, ati("p2", m2, 20)).expect("add t2");
    ComboTargetFixture {
        combo_id,
        m1,
        t1,
        t2,
    }
}

#[test]
fn delete_combo_target_removes_row() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let fx = seed_combo_with_two_targets(&conn);
    delete_combo_target(&conn, fx.combo_id, fx.t1).expect("delete t1");
    let targets = list_combo_targets(&conn, fx.combo_id).expect("list");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].id, fx.t2);
}

#[test]
fn delete_combo_target_rejects_wrong_combo() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let fx = seed_combo_with_two_targets(&conn);
    let err = delete_combo_target(&conn, ComboId(99_999), fx.t1).expect_err("cross-combo");
    let CoreError::Validation(msg) = err else {
        panic!("expected Validation, got {err:?}");
    };
    assert!(msg.contains("not in combo"));
    assert_eq!(
        list_combo_targets(&conn, fx.combo_id).expect("list").len(),
        2
    );
}

#[test]
fn reorder_combo_targets_assigns_priority_1_2_3() {
    let (pool, _path) = fresh_pool();
    let mut conn = pool.writer();
    let fx = seed_combo_with_two_targets(&conn);
    reorder_combo_targets(&mut conn, fx.combo_id, &[fx.t2, fx.t1]).expect("reorder");
    let targets = list_combo_targets(&conn, fx.combo_id).expect("list");
    assert_eq!(targets.len(), 2);
    assert_eq!(targets[0].id, fx.t2);
    assert_eq!(targets[0].priority_order, 1);
    assert_eq!(targets[1].id, fx.t1);
    assert_eq!(targets[1].priority_order, 2);
}

#[test]
fn reorder_combo_targets_rejects_non_permutation() {
    let (pool, _path) = fresh_pool();
    let mut conn = pool.writer();
    let fx = seed_combo_with_two_targets(&conn);
    let before: Vec<i32> = list_combo_targets(&conn, fx.combo_id)
        .unwrap()
        .into_iter()
        .map(|t| t.priority_order)
        .collect();
    let err = reorder_combo_targets(
        &mut conn,
        fx.combo_id,
        &[fx.t1, fx.t2, ComboTargetId(88888)],
    )
    .expect_err("extra");
    assert!(matches!(err, CoreError::Validation(_)));
    let after: Vec<i32> = list_combo_targets(&conn, fx.combo_id)
        .unwrap()
        .into_iter()
        .map(|t| t.priority_order)
        .collect();
    assert_eq!(before, after);
}

#[test]
fn list_combo_targets_with_model_returns_display_name() {
    let (pool, _path) = fresh_pool();
    let conn = pool.writer();
    let fx = seed_combo_with_two_targets(&conn);
    let enriched = list_combo_targets_with_model(&conn, fx.combo_id).expect("list");
    assert_eq!(enriched.len(), 2);
    assert_eq!(&*enriched[0].model_id, "m1");
    assert_eq!(enriched[0].model_display_name.as_deref(), Some("Model One"));
    assert_eq!(enriched[0].model_row_id, Some(fx.m1));
    assert_eq!(&*enriched[1].model_id, "m2");
    assert_eq!(enriched[1].model_display_name.as_deref(), Some("Model Two"));
}
