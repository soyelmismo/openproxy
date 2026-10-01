use super::*;
use crate::models::{create_custom, get_by_row_id, list_all};
use openproxy_types::TargetFormat;

#[test]
fn create_custom_inserts_active_row() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let row_id = create_custom(
        &conn,
        &provider,
        &ModelId::new("manual-model"),
        Some("My Hand-Crafted Model"),
        TargetFormat::Openai,
        3600,
        None,
    )
    .expect("create_custom");
    assert!(row_id.0 > 0);

    let m = get_by_row_id(&conn, row_id).unwrap().expect("present");
    assert!(m.custom);
    assert!(m.active);
    assert_eq!(m.model_id.as_str(), "manual-model");
    assert_eq!(m.display_name.as_deref(), Some("My Hand-Crafted Model"));
    assert!(m.expires_at.is_some());
    assert_eq!(m.last_test_status, None);
}

#[test]
fn create_custom_with_ttl_zero_means_no_expiry() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");
    let row_id = create_custom(
        &conn,
        &provider,
        &ModelId::new("forever-model"),
        None,
        TargetFormat::Anthropic,
        0,
        None,
    )
    .expect("create_custom ttl=0");
    let m = get_by_row_id(&conn, row_id).unwrap().expect("present");
    assert!(m.expires_at.is_none());
}

#[test]
fn create_custom_on_existing_row_upserts() {
    let conn = fresh_db();
    let provider = ProviderId::new("provA");

    conn.execute(
        "INSERT INTO models (provider_id, model_id, display_name, target_format, active, custom) \
         VALUES ('provA', 'shared-id', 'old', 'openai', 0, 0)",
        [],
    )
    .expect("seed non-custom row");
    let original_row_id = list_all(&conn).unwrap()[0].row_id;

    let returned = create_custom(
        &conn,
        &provider,
        &ModelId::new("shared-id"),
        Some("new"),
        TargetFormat::Openai,
        60,
        None,
    )
    .expect("create_custom on existing");

    assert_eq!(returned, original_row_id);
    let m = get_by_row_id(&conn, returned).unwrap().unwrap();
    assert!(m.custom);
    assert!(m.active);
    assert_eq!(m.display_name.as_deref(), Some("new"));
}

#[test]
fn create_custom_with_unknown_provider_fails_validation() {
    let conn = fresh_db();
    let err = create_custom(
        &conn,
        &ProviderId::new("does-not-exist"),
        &ModelId::new("m"),
        None,
        TargetFormat::Openai,
        60,
        None,
    )
    .expect_err("FK violation");
    assert!(matches!(err, CoreError::Validation(_)));
}
