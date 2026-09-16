use super::common::*;
use crate::handlers::admin::usage::UsageQuery;

fn q(from: Option<&str>, to: Option<&str>, preset: Option<&str>) -> UsageQuery {
    UsageQuery {
        from: from.map(String::from),
        to: to.map(String::from),
        preset: preset.map(String::from),
        provider_id: None,
        model_id: None,
        account_id: None,
        combo_id: None,
        api_key_id: None,
    }
}

#[test]
fn usage_filter_rejects_garbage_timestamp_with_400() {
    let err = format!(
        "{:?}",
        q(Some("garbage"), None, None).into_filter().unwrap_err()
    );
    assert!(err.contains("from") && err.contains("garbage"));
}

#[test]
fn usage_filter_accepts_rfc3339_and_canonicalises() {
    let from = q(Some("2026-06-18T07:00:00+02:00"), None, None)
        .into_filter()
        .unwrap()
        .from
        .unwrap();
    assert!(from.ends_with('Z') && from.starts_with("2026-06-18T05:00:00"));
}

#[test]
fn usage_filter_accepts_sqlite_format() {
    assert_eq!(
        q(None, Some("2026-06-18 07:00:00"), None)
            .into_filter()
            .unwrap()
            .to
            .unwrap(),
        "2026-06-18T07:00:00Z"
    );
}

#[test]
fn usage_filter_rejects_from_after_to() {
    let err = format!(
        "{:?}",
        q(
            Some("2026-06-18T08:00:00Z"),
            Some("2026-06-18T07:00:00Z"),
            None
        )
        .into_filter()
        .unwrap_err()
    );
    assert!(err.contains("must be <="));
}

#[test]
fn usage_filter_absent_timestamps_still_pass() {
    let f = q(None, None, None).into_filter().unwrap();
    assert!(f.from.is_none() && f.to.is_none());
}

#[test]
fn usage_filter_preset_this_month_resolves_to_month_bounds() {
    let f = q(None, None, Some("this_month")).into_filter().unwrap();
    let (from, to) = (f.from.unwrap(), f.to.unwrap());
    assert!(from.ends_with("-01T00:00:00Z") && to.ends_with("-01T00:00:00Z") && from < to);
}

#[test]
fn usage_filter_preset_overrides_explicit_from_to() {
    let from = q(
        Some("2000-01-01T00:00:00Z"),
        Some("2000-01-02T00:00:00Z"),
        Some("7d"),
    )
    .into_filter()
    .unwrap()
    .from
    .unwrap();
    assert!(!from.starts_with("2000-") && from.starts_with("20"));
}

#[test]
fn usage_filter_preset_custom_falls_through_to_explicit_values() {
    let f = q(
        Some("2026-06-18T07:00:00Z"),
        Some("2026-06-19T07:00:00Z"),
        Some("custom"),
    )
    .into_filter()
    .unwrap();
    assert_eq!(f.from.as_deref(), Some("2026-06-18T07:00:00Z"));
    assert_eq!(f.to.as_deref(), Some("2026-06-19T07:00:00Z"));
}

#[test]
fn usage_filter_preset_unknown_string_returns_400() {
    let err = format!(
        "{:?}",
        q(None, None, Some("last_week")).into_filter().unwrap_err()
    );
    assert!(err.contains("preset") && err.contains("last_week"));
}

#[tokio::test]
async fn usage_recent_clamps_since_id_at_max() {
    let tmp = tempdir();
    let (state, key) = make_state_with_key(tmp.path()).await;
    let app = crate::router::build_router(state);
    let (status, _) = test_req(
        &app,
        "GET",
        "/admin/usage/recent?since_id=9223372036854775807&limit=1",
        Some(&key),
        None,
    )
    .await;
    assert!(status.is_success());
}

#[tokio::test]
async fn usage_recent_rejects_negative_since_id() {
    let tmp = tempdir();
    let (state, key) = make_state_with_key(tmp.path()).await;
    let app = crate::router::build_router(state);
    let (status, _) = test_req(
        &app,
        "GET",
        "/admin/usage/recent?since_id=-42&limit=1",
        Some(&key),
        None,
    )
    .await;
    assert!(status.is_success());
}
