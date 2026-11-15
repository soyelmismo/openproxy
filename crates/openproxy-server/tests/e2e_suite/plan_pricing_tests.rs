//! Public plan pricing is metadata only: it must not allocate an order or credit.
use crate::{credits_tests::seed_plan, harness::TestHarness};
use axum::http::{Method, StatusCode};
use serde_json::{Value, json};

async fn get_plans(h: &TestHarness) -> Value {
    let (status, body) = h
        .oneshot_json(h.public_request(Method::GET, "/v1/credits/plans", ""))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

#[tokio::test]
async fn public_plan_pricing_tracks_combo_edits_without_changing_commerce() {
    let h = TestHarness::new().await;
    let tier = seed_plan(&h, 5.0, 30.0, 7).await;
    let schedule = json!({
        "timezone": "UTC",
        "peak": {"input_per_1m": 0.30, "output_per_1m": 1.20, "cached_per_1m": 0.006},
        "off_peak": {"input_per_1m": 0.15, "output_per_1m": 0.60, "cached_per_1m": 0.003},
        "peak_windows": [
            {"days": [1,2,3,4,5], "start": "01:00", "end": "04:00"},
            {"days": [1,2,3,4,5], "start": "06:00", "end": "10:00"}
        ],
        "off_peak_dates": ["2026-11-16"]
    });
    let (status, combo) = h
        .oneshot_json(
            h.admin_request(
                Method::POST,
                "/admin/api/combos",
                json!({"name": "scheduled-plan", "strategy": "priority",
            "pricing_input_per_1m": 0.3, "pricing_output_per_1m": 1.2,
            "pricing_cached_per_1m": 0.006, "pricing_schedule": schedule})
                .to_string(),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{combo}");
    let combo_id = combo["id"].as_i64().unwrap();
    let (status, body) = h
        .oneshot_json(
            h.admin_request(
                Method::PATCH,
                &format!("/admin/api/credits/tiers/{tier}"),
                json!({"config": {"allowed_models": ["unknown-model"],
            "allowed_combos": ["combo:scheduled-plan"]}})
                .to_string(),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let before = get_plans(&h).await;
    let plan = before["plans"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["tier_id"] == tier)
        .unwrap();
    assert_eq!(plan["usd"], 5.0);
    assert_eq!(
        plan["credit_usd"], 30.0,
        "estimates use credit, not the checkout price"
    );
    assert_eq!(plan["ttl_days"], 7);
    assert_eq!(plan["combos"], json!(["scheduled-plan"]));
    let entries = plan["model_pricing"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    let scheduled = entries.iter().find(|p| p["combo"] == true).unwrap();
    assert_eq!(scheduled["name"], "scheduled-plan");
    assert_eq!(scheduled["pricing_schedule"], schedule);
    assert!(scheduled["pricing_input_per_1m"].as_f64().unwrap() >= 0.0);
    let unknown = entries
        .iter()
        .find(|p| p["name"] == "unknown-model")
        .unwrap();
    assert!(unknown["pricing_input_per_1m"].is_null());
    assert!(unknown["pricing_output_per_1m"].is_null());
    assert!(unknown["pricing_schedule"].is_null());

    let (status, body) = h
        .oneshot_json(
            h.admin_request(
                Method::PATCH,
                &format!("/admin/api/combos/{combo_id}"),
                json!({"pricing_schedule": null, "pricing_input_per_1m": 0.42,
            "pricing_output_per_1m": 1.68, "pricing_cached_per_1m": null})
                .to_string(),
            ),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let after = get_plans(&h).await;
    let plan = after["plans"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["tier_id"] == tier)
        .unwrap();
    let updated = plan["model_pricing"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["combo"] == true)
        .unwrap();
    assert!(updated["pricing_schedule"].is_null());
    assert_eq!(updated["pricing_input_per_1m"], 0.42);
    assert_eq!(updated["pricing_output_per_1m"], 1.68);
    assert_eq!(plan["credit_usd"], 30.0);
    assert_eq!(plan["usd"], 5.0);
    let order_count = h
        .db_pool
        .spawn_read(|r| {
            r.query_row("SELECT COUNT(*) FROM credit_orders", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(openproxy_db::error::map_db_error)
        })
        .await
        .unwrap();
    assert_eq!(
        order_count, 0,
        "viewing estimates never creates a payable order"
    );
}
