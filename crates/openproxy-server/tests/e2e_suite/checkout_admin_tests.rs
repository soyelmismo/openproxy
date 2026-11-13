//! HTTP regressions for partial plan commerce updates.
use crate::{credits_tests::seed_plan, harness::TestHarness};
use axum::http::{Method, StatusCode};
use serde_json::json;

#[tokio::test]
async fn partial_plan_patch_preserves_omitted_fields_and_rejects_null_lifetime() {
    let h = TestHarness::new().await;
    let id = seed_plan(&h, 5.0, 30.0, 7).await;
    let path = format!("/admin/api/credits/tiers/{id}");
    let (status, body) = h
        .oneshot_json(h.admin_request(Method::PATCH, &path, json!({"price_usd": 6.0}).to_string()))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let tier = h
        .db_pool
        .spawn_read(move |r| openproxy_core::credits::tiers::get(r, id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tier.price_usd, Some(6.0));
    assert_eq!(tier.credit_usd, Some(30.0));
    assert_eq!(tier.ttl_days, 7);
    let (status, body) = h
        .oneshot_json(h.admin_request(
            Method::PATCH,
            &path,
            json!({"credit_usd": null}).to_string(),
        ))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = h
        .oneshot_json(h.admin_request(
            Method::PATCH,
            &path,
            json!({"price_usd": 9.0, "ttl_days": null}).to_string(),
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let tier = h
        .db_pool
        .spawn_read(move |r| openproxy_core::credits::tiers::get(r, id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tier.price_usd, Some(6.0), "invalid patch must roll back");
    assert_eq!(tier.credit_usd, None);
    assert_eq!(tier.ttl_days, 7);
}
