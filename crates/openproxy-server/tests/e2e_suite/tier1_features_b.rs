use crate::harness::TestHarness;
use axum::http::StatusCode;
use serde_json::json;

#[test]
fn test_tier1_admin_providers_list() {
    crate::run_with_big_stack(test_tier1_admin_providers_list_body);
}

async fn test_tier1_admin_providers_list_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/providers").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        resp.as_array()
            .expect("providers array")
            .iter()
            .any(|p| p["id"] == "mock-prov")
    );
}

#[test]
fn test_tier1_admin_providers_create() {
    crate::run_with_big_stack(test_tier1_admin_providers_create_body);
}

async fn test_tier1_admin_providers_create_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_post("/admin/api/providers", json!({"id": "tier1-new-prov", "name": "New Provider", "base_url": "http://127.0.0.1:9999/v1", "auth_type": "bearer", "format": "openai"})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["id"], "tier1-new-prov");
}

#[test]
fn test_tier1_admin_providers_get_by_id() {
    crate::run_with_big_stack(test_tier1_admin_providers_get_by_id_body);
}

async fn test_tier1_admin_providers_get_by_id_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/providers/mock-prov").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["id"], "mock-prov");
}

#[test]
fn test_tier1_admin_providers_update() {
    crate::run_with_big_stack(test_tier1_admin_providers_update_body);
}

async fn test_tier1_admin_providers_update_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .admin_patch(
            "/admin/api/providers/mock-prov",
            json!({"name": "Updated Mock Name"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(resp["id"], "mock-prov");
    let (_, fetched) = harness.admin_get("/admin/api/providers/mock-prov").await;
    assert_eq!(fetched["name"], "Updated Mock Name");
}

#[test]
fn test_tier1_admin_providers_delete() {
    crate::run_with_big_stack(test_tier1_admin_providers_delete_body);
}

async fn test_tier1_admin_providers_delete_body() {
    let harness = TestHarness::new().await;
    let _ = harness.admin_post("/admin/api/providers", json!({"id": "prov-to-delete", "name": "Deletable Provider", "base_url": "http://127.0.0.1:8888/v1", "auth_type": "bearer", "format": "openai"})).await;
    let (status, _) = harness
        .admin_delete("/admin/api/providers/prov-to-delete")
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status_after, _) = harness
        .admin_get("/admin/api/providers/prov-to-delete")
        .await;
    assert_eq!(status_after, StatusCode::NOT_FOUND);
}

#[test]
fn test_tier1_admin_models_list() {
    crate::run_with_big_stack(test_tier1_admin_models_list_body);
}

async fn test_tier1_admin_models_list_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/models").await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        resp.as_array()
            .unwrap()
            .iter()
            .any(|m| m["model_id"] == "mock-gpt-4")
    );
}

#[test]
fn test_tier1_admin_models_create_custom() {
    crate::run_with_big_stack(test_tier1_admin_models_create_custom_body);
}

async fn test_tier1_admin_models_create_custom_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_post("/admin/api/models/custom", json!({"provider_id": "mock-prov", "model_id": "mock-custom-tier1", "display_name": "Custom Model Tier 1", "target_format": "openai", "ttl_seconds": 3600})).await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("row_id").or_else(|| resp.get("id")).is_some());
}

#[test]
fn test_tier1_admin_models_toggle_active() {
    crate::run_with_big_stack(test_tier1_admin_models_toggle_active_body);
}

async fn test_tier1_admin_models_toggle_active_body() {
    let harness = TestHarness::new().await;
    let (status, models) = harness.admin_get("/admin/api/models").await;
    assert_eq!(status, StatusCode::OK);
    let row_id = models[0]
        .get("row_id")
        .or_else(|| models[0].get("id"))
        .unwrap()
        .as_i64()
        .unwrap();
    let (status_toggle, resp) = harness
        .admin_post(
            &format!("/admin/api/models/{row_id}/toggle"),
            json!({"active": false}),
        )
        .await;
    assert_eq!(status_toggle, StatusCode::OK);
    assert_eq!(resp["active"], false);
}

#[test]
fn test_tier1_admin_models_bulk_toggle() {
    crate::run_with_big_stack(test_tier1_admin_models_bulk_toggle_body);
}

async fn test_tier1_admin_models_bulk_toggle_body() {
    let harness = TestHarness::new().await;
    let (status_bulk, resp) = harness
        .admin_post(
            "/admin/api/models/bulk-toggle",
            json!({"provider_id": "mock-prov", "active": true}),
        )
        .await;
    assert_eq!(status_bulk, StatusCode::OK);
    assert!(resp.get("updated").is_some());
}

#[test]
fn test_tier1_admin_models_refresh_trigger() {
    crate::run_with_big_stack(test_tier1_admin_models_refresh_trigger_body);
}

async fn test_tier1_admin_models_refresh_trigger_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .admin_post("/admin/api/providers/mock-prov/refresh", json!({}))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("models_refreshed").is_some());
}

#[test]
fn test_tier1_admin_combos_list() {
    crate::run_with_big_stack(test_tier1_admin_combos_list_body);
}

async fn test_tier1_admin_combos_list_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness.admin_get("/admin/api/combos").await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.is_array());
}

#[test]
fn test_tier1_admin_combos_create() {
    crate::run_with_big_stack(test_tier1_admin_combos_create_body);
}

async fn test_tier1_admin_combos_create_body() {
    let harness = TestHarness::new().await;
    let (status, resp) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "combo-tier1-roundrobin", "strategy": "round_robin"}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("id").is_some());
}

#[test]
fn test_tier1_admin_combos_add_targets() {
    crate::run_with_big_stack(test_tier1_admin_combos_add_targets_body);
}

async fn test_tier1_admin_combos_add_targets_body() {
    let harness = TestHarness::new().await;
    let (_, combo) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "combo-with-target", "strategy": "priority"}),
        )
        .await;
    let combo_id = combo["id"].as_i64().unwrap();
    let (_, models) = harness.admin_get("/admin/api/models").await;
    let model_row_id = models[0]
        .get("row_id")
        .or_else(|| models[0].get("id"))
        .unwrap()
        .as_i64()
        .unwrap();
    let (status, resp) = harness
        .admin_post(
            &format!("/admin/api/combos/{combo_id}/targets"),
            json!({"provider_id": "mock-prov", "model_row_id": model_row_id, "priority_order": 1}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("id").is_some());
}

#[test]
fn test_tier1_admin_combos_reorder_targets() {
    crate::run_with_big_stack(test_tier1_admin_combos_reorder_targets_body);
}

async fn test_tier1_admin_combos_reorder_targets_body() {
    let harness = TestHarness::new().await;
    let (_, combo) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "combo-reorder", "strategy": "priority"}),
        )
        .await;
    let combo_id = combo["id"].as_i64().unwrap();
    let (_, models) = harness.admin_get("/admin/api/models").await;
    let m1 = models[0]
        .get("row_id")
        .or_else(|| models[0].get("id"))
        .unwrap()
        .as_i64()
        .unwrap();
    let m2 = models.get(1).map_or(m1, |m| {
        m.get("row_id")
            .or_else(|| m.get("id"))
            .unwrap()
            .as_i64()
            .unwrap()
    });
    let (_, t1) = harness
        .admin_post(
            &format!("/admin/api/combos/{combo_id}/targets"),
            json!({"provider_id": "mock-prov", "model_row_id": m1, "priority_order": 1}),
        )
        .await;
    let (_, t2) = harness
        .admin_post(
            &format!("/admin/api/combos/{combo_id}/targets"),
            json!({"provider_id": "mock-prov", "model_row_id": m2, "priority_order": 2}),
        )
        .await;
    let (status, resp) = harness
        .admin_post(
            &format!("/admin/api/combos/{combo_id}/targets/reorder"),
            json!({"target_ids": vec![t2["id"].as_i64().unwrap(), t1["id"].as_i64().unwrap()]}),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(resp.get("reordered").is_some());
}

#[test]
fn test_tier1_admin_combos_delete() {
    crate::run_with_big_stack(test_tier1_admin_combos_delete_body);
}

async fn test_tier1_admin_combos_delete_body() {
    let harness = TestHarness::new().await;
    let (_, combo) = harness
        .admin_post(
            "/admin/api/combos",
            json!({"name": "combo-to-delete", "strategy": "priority"}),
        )
        .await;
    let combo_id = combo["id"].as_i64().unwrap();
    let (status, _) = harness
        .admin_delete(&format!("/admin/api/combos/{combo_id}"))
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status_after, _) = harness
        .admin_get(&format!("/admin/api/combos/{combo_id}"))
        .await;
    assert_eq!(status_after, StatusCode::NOT_FOUND);
}
