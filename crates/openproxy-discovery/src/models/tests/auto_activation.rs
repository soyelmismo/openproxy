use super::*;
use crate::models::{
    apply_auto_activation, get_by_row_id, list_active, list_all, set_active, upsert_many,
};
use openproxy_types::TargetFormat;

fn insert_model_timed(conn: &Connection, model: &str, time_offset: &str, active: i32) {
    conn.execute(
        &format!("INSERT INTO models (provider_id, model_id, display_name, target_format, discovered_at, active) VALUES ('provA', '{model}', '{model}', 'openai', datetime('now', '{time_offset}'), {active})"),
        [],
    ).unwrap();
}

#[test]
fn apply_auto_activation_does_not_affect_old_re_upserted_model() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let n = upsert_many(
        &conn,
        &provider,
        &[minimal("anthropic/claude-sonnet-4")],
        Duration::from_hours(1),
    )
    .expect("first upsert");
    assert_eq!(n.touched, 1);
    let m = list_all(&conn).unwrap().pop().unwrap();
    set_active(&conn, m.row_id, false).expect("set_active false");
    conn.execute(
        "UPDATE models SET discovered_at = datetime('now', '-2 minutes') WHERE id = ?1",
        [m.row_id.0],
    )
    .expect("backdate");
    let pre_discovered = list_all(&conn).unwrap().pop().unwrap().discovered_at;

    let n2 = upsert_many(
        &conn,
        &provider,
        &[minimal("anthropic/claude-sonnet-4")],
        Duration::from_hours(1),
    )
    .expect("refresh");
    assert_eq!(n2.touched, 1);
    let m = list_all(&conn).unwrap().pop().unwrap();
    assert_eq!(m.discovered_at, pre_discovered);
    assert_eq!(
        apply_auto_activation(&conn, &provider, Some("claude")).unwrap(),
        0
    );
    assert_eq!(apply_auto_activation(&conn, &provider, None).unwrap(), 0);
    assert!(!list_all(&conn).unwrap().pop().unwrap().active);
}

#[test]
fn apply_auto_activation_with_keyword_matches_substring() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    upsert_many(
        &conn,
        &provider,
        &[
            discovered("claude-3", TargetFormat::Openai),
            discovered("claude-2", TargetFormat::Openai),
            discovered("gpt-4", TargetFormat::Openai),
            discovered("gemini-pro", TargetFormat::Openai),
        ],
        Duration::from_hours(1),
    )
    .expect("seed");

    let updated = apply_auto_activation(&conn, &provider, Some("claude")).expect("apply");
    assert!(updated >= 2);
    let active = list_active(&conn, &provider).expect("list_active");
    let active_ids: Vec<&str> = active.iter().map(|m| m.model_id.as_str()).collect();
    assert!(active_ids.contains(&"claude-3") && active_ids.contains(&"claude-2"));
    assert!(!active_ids.contains(&"gpt-4") && !active_ids.contains(&"gemini-pro"));
}

#[test]
fn apply_auto_activation_with_no_keyword_does_not_reactivate_manually_disabled() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    upsert_many(
        &conn,
        &provider,
        &[
            discovered("a", TargetFormat::Openai),
            discovered("b", TargetFormat::Openai),
            discovered("c", TargetFormat::Openai),
        ],
        Duration::from_hours(1),
    )
    .unwrap();
    for m in list_all(&conn).unwrap() {
        set_active(&conn, m.row_id, false).unwrap();
    }
    assert_eq!(apply_auto_activation(&conn, &provider, None).unwrap(), 0);
    assert_eq!(list_active(&conn, &provider).unwrap().len(), 0);
    for m in list_all(&conn).unwrap() {
        assert!(!m.active && m.manually_disabled_at.is_some());
    }
}

#[test]
fn apply_auto_activation_skips_custom_rows() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    upsert_many(
        &conn,
        &provider,
        &[
            discovered("claude-3", TargetFormat::Openai),
            discovered("gpt-4", TargetFormat::Openai),
        ],
        Duration::from_hours(1),
    )
    .unwrap();
    conn.execute("INSERT INTO models (provider_id, model_id, target_format, custom, active) VALUES ('provA', 'handpicked', 'openai', 1, 0)", []).unwrap();
    apply_auto_activation(&conn, &provider, Some("claude")).unwrap();
    let all = list_all(&conn).unwrap();
    let handpicked = all
        .iter()
        .find(|m| m.model_id.as_str() == "handpicked")
        .unwrap();
    assert!(!handpicked.active && handpicked.custom);
    let claude = all
        .iter()
        .find(|m| m.model_id.as_str() == "claude-3")
        .unwrap();
    assert!(claude.active);
    let gpt = all.iter().find(|m| m.model_id.as_str() == "gpt-4").unwrap();
    assert!(!gpt.active);
}

#[test]
fn apply_auto_activation_skips_old_models() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    insert_model_timed(&conn, "old-a", "-2 minutes", 0);
    insert_model_timed(&conn, "old-b", "-2 minutes", 0);
    insert_model_timed(&conn, "new-c", "+0 seconds", 0);
    assert_eq!(apply_auto_activation(&conn, &provider, None).unwrap(), 1);
    let all = list_all(&conn).unwrap();
    assert!(
        !all.iter()
            .find(|m| m.model_id.as_str() == "old-a")
            .unwrap()
            .active
    );
    assert!(
        !all.iter()
            .find(|m| m.model_id.as_str() == "old-b")
            .unwrap()
            .active
    );
    assert!(
        all.iter()
            .find(|m| m.model_id.as_str() == "new-c")
            .unwrap()
            .active
    );
}

#[test]
fn apply_auto_activation_with_keyword_only_affects_new() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    insert_model_timed(&conn, "old-free", "-2 minutes", 0);
    insert_model_timed(&conn, "new-free", "+0 seconds", 0);
    assert_eq!(
        apply_auto_activation(&conn, &provider, Some("free")).unwrap(),
        1
    );
    let all = list_all(&conn).unwrap();
    assert!(
        !all.iter()
            .find(|m| m.model_id.as_str() == "old-free")
            .unwrap()
            .active
    );
    assert!(
        all.iter()
            .find(|m| m.model_id.as_str() == "new-free")
            .unwrap()
            .active
    );
}

#[test]
fn apply_auto_activation_no_new_models_is_noop() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    for id in ["x", "y", "z"] {
        insert_model_timed(&conn, id, "-5 minutes", 0);
    }
    assert_eq!(apply_auto_activation(&conn, &provider, None).unwrap(), 0);
    assert_eq!(
        apply_auto_activation(&conn, &provider, Some("x")).unwrap(),
        0
    );
    assert!(list_all(&conn).unwrap().iter().all(|m| !m.active));
}

#[test]
fn apply_auto_activation_skips_manually_disabled_within_60s() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    upsert_many(
        &conn,
        &provider,
        &[discovered("m1", TargetFormat::Openai)],
        Duration::from_hours(1),
    )
    .unwrap();
    let row_id = list_all(&conn).unwrap()[0].row_id;
    set_active(&conn, row_id, false).unwrap();
    assert_eq!(apply_auto_activation(&conn, &provider, None).unwrap(), 0);
    let m = get_by_row_id(&conn, row_id).unwrap().unwrap();
    assert!(!m.active && m.manually_disabled_at.is_some());
}

#[test]
fn apply_auto_activation_acts_on_non_manually_disabled_within_60s() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    upsert_many(
        &conn,
        &provider,
        &[discovered("m1", TargetFormat::Openai)],
        Duration::from_hours(1),
    )
    .unwrap();
    let row_id = list_all(&conn).unwrap()[0].row_id;
    conn.execute(
        "UPDATE models SET active = 0, manually_disabled_at = NULL WHERE id = ?1",
        [row_id.0],
    )
    .unwrap();
    assert_eq!(apply_auto_activation(&conn, &provider, None).unwrap(), 1);
    let m = get_by_row_id(&conn, row_id).unwrap().unwrap();
    assert!(m.active && m.manually_disabled_at.is_none());
}

#[test]
fn apply_auto_activation_keyword_skips_manually_disabled_within_60s() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    upsert_many(
        &conn,
        &provider,
        &[discovered("matchable", TargetFormat::Openai)],
        Duration::from_hours(1),
    )
    .unwrap();
    let row_id = list_all(&conn).unwrap()[0].row_id;
    set_active(&conn, row_id, false).unwrap();
    assert_eq!(
        apply_auto_activation(&conn, &provider, Some("matchable")).unwrap(),
        0
    );
    let m = get_by_row_id(&conn, row_id).unwrap().unwrap();
    assert!(!m.active && m.manually_disabled_at.is_some());
}

#[test]
fn manually_disabled_model_survives_arbitrary_refresh_cycles() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let m1 = discovered("m1", TargetFormat::Openai);
    upsert_many(
        &conn,
        &provider,
        std::slice::from_ref(&m1),
        Duration::from_hours(1),
    )
    .unwrap();
    let row_id = list_all(&conn).unwrap()[0].row_id;

    set_active(&conn, row_id, false).unwrap();
    for _ in 1..=5 {
        upsert_many(
            &conn,
            &provider,
            std::slice::from_ref(&m1),
            Duration::from_hours(1),
        )
        .unwrap();
        assert_eq!(apply_auto_activation(&conn, &provider, None).unwrap(), 0);
        let m = get_by_row_id(&conn, row_id).unwrap().unwrap();
        assert!(!m.active && m.manually_disabled_at.is_some());
    }
    set_active(&conn, row_id, true).unwrap();
    let after_enable = get_by_row_id(&conn, row_id).unwrap().unwrap();
    assert!(after_enable.active && after_enable.manually_disabled_at.is_none());
}

#[test]
fn apply_auto_activation_keyword_persists_non_matching_disable_across_refreshes() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let m_match = discovered("vendor/model-free-001", TargetFormat::Openai);
    let m_nomatch = discovered("vendor/model-paid-001", TargetFormat::Openai);
    let models = &[m_match, m_nomatch];

    for _ in 1..=5 {
        upsert_many(&conn, &provider, models, Duration::from_hours(1)).unwrap();
        let _ = apply_auto_activation(&conn, &provider, Some("free-")).unwrap();
        let all = list_all(&conn).unwrap();
        let r_match = all
            .iter()
            .find(|m| m.model_id.as_str() == "vendor/model-free-001")
            .unwrap();
        let r_nomatch = all
            .iter()
            .find(|m| m.model_id.as_str() == "vendor/model-paid-001")
            .unwrap();
        assert!(r_match.active && r_match.manually_disabled_at.is_none());
        assert!(!r_nomatch.active && r_nomatch.manually_disabled_at.is_none());
    }
}
