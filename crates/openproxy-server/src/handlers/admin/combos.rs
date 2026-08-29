use super::{
    ApiError, AppState, Arc, ComboId, ComboTargetId, CoreError, Deserialize, ModelRowId,
    TestOptions, core_combos, run_test_for_model, types_combos,
};
use axum::{
    Json,
    extract::{Path, State},
};

use openproxy_core::admin as core_admin;

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/", axum::routing::get(list_combos).post(create_combo))
        .route(
            "/{id}",
            axum::routing::get(get_combo)
                .delete(delete_combo)
                .patch(update_combo),
        )
        .route(
            "/{id}/test-all",
            axum::routing::post(test_combo_targets).route_layer(axum::middleware::from_fn(
                crate::disconnect::client_disconnect_middleware,
            )),
        )
        .route(
            "/{id}/targets",
            axum::routing::get(list_combo_targets).post(add_target),
        )
        .route(
            "/{id}/targets/valid-sub-combos",
            axum::routing::get(list_valid_sub_combos),
        )
        .route(
            "/{id}/targets/reorder",
            axum::routing::post(reorder_combo_targets),
        )
        .route(
            "/{id}/targets/{target_id}/clear-cooldown",
            axum::routing::post(clear_combo_target_cooldown),
        )
        .route(
            "/{id}/targets/{target_id}",
            axum::routing::patch(update_combo_target).delete(delete_combo_target),
        )
}

pub async fn list_combos(
    State(s): State<AppState>,
) -> Result<Json<Vec<types_combos::Combo>>, ApiError> {
    // Read-only SELECT — use the READER.
    let r = s.db_pool().reader();
    let list = core_admin::list_combos(&r)?;
    Ok(Json(list))
}

pub async fn create_combo(
    State(s): State<AppState>,
    Json(input): Json<core_admin::CreateComboInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let w = s.db_pool().writer();
    let id = core_admin::create_combo(&w, &input)?;
    Ok(Json(serde_json::json!({ "id": id.0 })))
}

pub async fn get_combo(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<types_combos::Combo>, ApiError> {
    // Read-only SELECT — use the READER.
    let r = s.db_pool().reader();
    let id = ComboId(id);
    let combo = core_combos::get_combo(&r, id)?.ok_or_else(|| CoreError::ComboNotFound(id.0))?;
    Ok(Json(combo))
}

fn build_skipped_target_entry(t: &types_combos::ComboTargetWithModel) -> Option<serde_json::Value> {
    use serde_json::json;
    if t.sub_combo_id.is_some() {
        return Some(json!({
            "target_id": t.id.0,
            "sub_combo_id": t.sub_combo_id.map(|c| c.0),
            "sub_combo_name": t.sub_combo_name,
            "provider_id": t.provider_id.to_string(),
            "status": 0_i32,
            "elapsed_ms": serde_json::Value::Null,
            "error_msg": "sub-combo; test children individually",
            "skipped": true,
        }));
    }
    if t.in_cooldown {
        return Some(json!({
            "target_id": t.id.0,
            "provider_id": t.provider_id.to_string(),
            "account_id": t.account_id.map(|a| a.0),
            "model_row_id": t.model_row_id.map(|m| m.0),
            "model_id": t.model_id,
            "model_display_name": t.model_display_name,
            "status": 0_i32,
            "elapsed_ms": serde_json::Value::Null,
            "error_msg": format!(
                "in_cooldown: {}",
                t.cooldown_reason.as_deref().unwrap_or("no reason recorded")
            ),
            "skipped": true,
        }));
    }
    None
}

async fn run_and_format_single_combo_target(
    s: &AppState,
    t: &types_combos::ComboTargetWithModel,
    cancel_rx: Option<tokio::sync::watch::Receiver<Option<openproxy_types::CancelReason>>>,
) -> serde_json::Value {
    let (r, _) = run_test_for_model(
        s,
        t.model_row_id.unwrap_or(ModelRowId(0)).0,
        t.account_id,
        None,
        TestOptions {
            in_combo_fanout: true,
        },
        cancel_rx,
    )
    .await;

    let mut obj = serde_json::json!({
        "target_id": t.id.0,
        "provider_id": t.provider_id.to_string(),
        "account_id": t.account_id.map(|a| a.0),
        "model_row_id": t.model_row_id.map(|m| m.0),
        "model_id": t.model_id,
        "model_display_name": t.model_display_name,
        "status": r.status,
        "elapsed_ms": r.elapsed_ms,
        "error_msg": r.error_msg,
        "skipped": r.skipped,
        "row_id": r.row_id,
    });
    if r.skipped {
        obj["error_msg"] =
            serde_json::json!(r.skip_reason.unwrap_or_else(|| "skipped".to_string()));
    }
    obj
}

async fn fetch_combo_targets_for_test(
    s: &AppState,
    id: i64,
) -> Result<Vec<types_combos::ComboTargetWithModel>, ApiError> {
    tokio::task::spawn_blocking({
        let pool = Arc::clone(s.db_pool());
        move || {
            let w = pool.writer();
            core_combos::list_targets_with_model(&w, ComboId(id))
        }
    })
    .await
    .unwrap_or_else(|e| Err(CoreError::Internal(format!("spawn_blocking failed: {e}"))))
    .map_err(ApiError)
}

async fn run_fan_out_combo_tests(
    s: &AppState,
    id: i64,
    targets: Vec<types_combos::ComboTargetWithModel>,
    cancel_rx: Option<tokio::sync::watch::Receiver<Option<openproxy_types::CancelReason>>>,
) -> Result<Vec<serde_json::Value>, ApiError> {
    let fan_out = async {
        let mut results = Vec::with_capacity(targets.len());
        for t in targets {
            if let Some(skipped) = build_skipped_target_entry(&t) {
                results.push(skipped);
                continue;
            }
            if let Some(ref rx) = cancel_rx
                && rx.borrow().is_some()
            {
                tracing::info!("test_combo_targets: client disconnected, aborting fan-out");
                break;
            }
            results.push(run_and_format_single_combo_target(s, &t, cancel_rx.clone()).await);
        }
        results
    };

    tokio::time::timeout(std::time::Duration::from_mins(3), fan_out)
        .await
        .map_err(|_| {
            tracing::warn!(combo_id = id, "test-all fan-out exceeded 180s budget");
            crate::error::ApiError(openproxy_types::CoreError::Internal(
                "test-all exceeded 180s budget; partial results dropped".into(),
            ))
        })
}

pub async fn test_combo_targets(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    cancel_watch: Option<axum::Extension<crate::disconnect::CancelWatch>>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    let cancel_rx = cancel_watch.map(|axum::Extension(cw)| cw.rx);
    let targets = fetch_combo_targets_for_test(&s, id).await?;
    let results = run_fan_out_combo_tests(&s, id, targets, cancel_rx).await?;
    Ok(Json(results))
}

crate::admin_entity_action_handler! {
    pub async fn delete_combo(
        State(s) with writer(w),
        Path(id): Path<i64>,
    ) -> Result<Json<serde_json::Value>, ApiError> {
        let id = ComboId(id);
        core_admin::delete_combo(&w, id)?;
        Ok(Json(serde_json::json!({ "deleted": id.0 })))
    }
}

pub async fn list_combo_targets(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<types_combos::ComboTargetWithModel>>, ApiError> {
    // Read-only SELECT — use the READER.
    let r = s.db_pool().reader();
    let id = ComboId(id);
    let targets = core_admin::list_combo_targets_with_model(&r, id)?;
    Ok(Json(targets))
}

pub async fn add_target(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(input): Json<core_admin::AddTargetInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let w = s.db_pool().writer();
    let combo_id = ComboId(id);
    let new_id = core_admin::add_target_to_combo(&w, combo_id, input)?;
    Ok(Json(serde_json::json!({ "id": new_id.0 })))
}

pub async fn list_valid_sub_combos(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Vec<core_admin::ComboSummary>>, ApiError> {
    // Read-only SELECT — use the READER.
    let r = s.db_pool().reader();
    let id = ComboId(id);
    let list = core_admin::list_valid_sub_combos(&r, id)?;
    Ok(Json(list))
}

#[allow(clippy::option_option)]
fn parse_nullable_str<'a>(
    body: &'a serde_json::Value,
    field: &str,
) -> Result<Option<Option<&'a str>>, ApiError> {
    match body.get(field) {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(Some(None)),
        Some(v) => match v.as_str() {
            Some(s) => Ok(Some(Some(s))),
            None => Err(ApiError(CoreError::Validation(format!(
                "{field} must be a string or null, got {v}"
            )))),
        },
    }
}

#[allow(clippy::option_option)]
fn parse_nullable_u64(
    body: &serde_json::Value,
    field: &str,
) -> Result<Option<Option<u64>>, ApiError> {
    match body.get(field) {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(Some(None)),
        Some(v) => match v.as_u64() {
            Some(n) => Ok(Some(Some(n))),
            None => Err(ApiError(CoreError::Validation(format!(
                "{field} must be a non-negative integer or null"
            )))),
        },
    }
}

#[allow(clippy::option_option)]
fn parse_nullable_i64(
    body: &serde_json::Value,
    field: &str,
) -> Result<Option<Option<i64>>, ApiError> {
    match body.get(field) {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(Some(None)),
        Some(v) => match v.as_i64() {
            Some(n) => Ok(Some(Some(n))),
            None => Err(ApiError(CoreError::Validation(format!(
                "{field} must be null or an integer"
            )))),
        },
    }
}

#[allow(clippy::option_option)]
fn parse_nullable_f64(
    body: &serde_json::Value,
    field: &str,
) -> Result<Option<Option<f64>>, ApiError> {
    match body.get(field) {
        None => Ok(None),
        Some(v) if v.is_null() => Ok(Some(None)),
        Some(v) => match v.as_f64() {
            Some(n) => Ok(Some(Some(n))),
            None => Err(ApiError(CoreError::Validation(format!(
                "{field} must be a number in [0.0, 1.0] or null"
            )))),
        },
    }
}

fn apply_combo_cooldown_updates(
    w: &rusqlite::Connection,
    id: ComboId,
    body: &serde_json::Value,
) -> Result<(), ApiError> {
    if let Some(mode) = parse_nullable_str(body, "cooldown_mode")? {
        core_combos::update_cooldown_mode(w, id, mode)?;
    }
    if let Some(base) = parse_nullable_u64(body, "cooldown_base_secs")? {
        core_combos::update_cooldown_base(w, id, base)?;
    }
    if let Some(max) = parse_nullable_u64(body, "cooldown_max_secs")? {
        core_combos::update_cooldown_max(w, id, max)?;
    }
    if let Some(factor) = parse_nullable_u64(body, "cooldown_factor")? {
        core_combos::update_cooldown_factor(w, id, factor.map(|f| f as u32))?;
    }
    Ok(())
}

fn apply_combo_general_updates(
    w: &rusqlite::Connection,
    id: ComboId,
    body: &serde_json::Value,
) -> Result<(), ApiError> {
    if let Some(n) = body.get("race_size").and_then(serde_json::Value::as_u64) {
        let rs = u8::try_from(n).unwrap_or(0);
        core_combos::update_combo(w, id, Some(rs))?;
    }
    if let Some(cw) = parse_nullable_i64(body, "context_window")? {
        core_combos::update_context_window(w, id, cw)?;
    }
    if let Some(mode) = parse_nullable_str(body, "priority_mode")? {
        core_combos::update_priority_mode(w, id, mode)?;
    }
    if let Some(rate) = parse_nullable_f64(body, "lkgp_exploration_rate")? {
        core_combos::update_lkgp_settings(w, id, rate)?;
    }
    if let Some(window) = parse_nullable_u64(body, "selection_window_secs")? {
        core_combos::update_selection_window(w, id, window)?;
    }
    if let Some(v) = body.get("preventive_rate_limit")
        && let Some(enabled) = v.as_bool()
    {
        core_combos::update_preventive_rate_limit(w, id, enabled)?;
    }
    Ok(())
}

pub async fn update_combo(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let w = s.db_pool().writer();
    let combo_id = ComboId(id);
    apply_combo_general_updates(&w, combo_id, &body)?;
    apply_combo_cooldown_updates(&w, combo_id, &body)?;
    Ok(Json(serde_json::json!({ "id": id })))
}

#[allow(clippy::option_option)]
struct ComboTargetUpdates<'a> {
    priority_order: Option<i32>,
    weight: Option<i32>,
    active: Option<bool>,
    cooldown_mode: Option<Option<&'a str>>,
    cooldown_base_secs: Option<Option<u64>>,
    cooldown_max_secs: Option<Option<u64>>,
    cooldown_factor: Option<Option<u32>>,
}

impl ComboTargetUpdates<'_> {
    fn is_empty(&self) -> bool {
        self.priority_order.is_none()
            && self.weight.is_none()
            && self.active.is_none()
            && self.cooldown_mode.is_none()
            && self.cooldown_base_secs.is_none()
            && self.cooldown_max_secs.is_none()
            && self.cooldown_factor.is_none()
    }
}

fn parse_target_priority_order(body: &serde_json::Value) -> Result<Option<i32>, ApiError> {
    let Some(v) = body.get("priority_order") else {
        return Ok(None);
    };
    let p = v.as_i64().ok_or_else(|| {
        ApiError(CoreError::Validation(
            "priority_order must be an integer when present".into(),
        ))
    })?;
    if !(i64::from(i32::MIN)..=i64::from(i32::MAX)).contains(&p) {
        return Err(ApiError(CoreError::Validation(format!(
            "priority_order out of i32 range: {p}"
        ))));
    }
    Ok(Some(p as i32))
}

fn parse_target_weight(body: &serde_json::Value) -> Result<Option<i32>, ApiError> {
    let Some(v) = body.get("weight") else {
        return Ok(None);
    };
    let weight_i64 = v.as_i64().ok_or_else(|| {
        ApiError(CoreError::Validation(
            "weight must be an integer when present".into(),
        ))
    })?;
    if !(1..=i64::from(i32::MAX)).contains(&weight_i64) {
        return Err(ApiError(CoreError::Validation(format!(
            "weight must be a positive i32 (1..={}), got {}",
            i32::MAX,
            weight_i64
        ))));
    }
    Ok(Some(weight_i64 as i32))
}

fn parse_target_active(body: &serde_json::Value) -> Result<Option<bool>, ApiError> {
    let Some(v) = body.get("active") else {
        return Ok(None);
    };
    v.as_bool().map(Some).ok_or_else(|| {
        ApiError(CoreError::Validation(
            "active must be a boolean when present".into(),
        ))
    })
}

fn parse_combo_target_updates(
    body: &serde_json::Value,
) -> Result<ComboTargetUpdates<'_>, ApiError> {
    let priority_order = parse_target_priority_order(body)?;
    let weight = parse_target_weight(body)?;
    let active = parse_target_active(body)?;
    let cooldown_mode = parse_nullable_str(body, "cooldown_mode")?;
    let cooldown_base_secs = parse_nullable_u64(body, "cooldown_base_secs")?;
    let cooldown_max_secs = parse_nullable_u64(body, "cooldown_max_secs")?;
    let cooldown_factor =
        parse_nullable_u64(body, "cooldown_factor")?.map(|opt| opt.map(|f| f as u32));

    let updates = ComboTargetUpdates {
        priority_order,
        weight,
        active,
        cooldown_mode,
        cooldown_base_secs,
        cooldown_max_secs,
        cooldown_factor,
    };

    if updates.is_empty() {
        return Err(ApiError(CoreError::Validation(
            "missing update fields in request body".into(),
        )));
    }
    Ok(updates)
}

fn apply_target_db_updates(
    w: &rusqlite::Connection,
    target_id: ComboTargetId,
    updates: &ComboTargetUpdates<'_>,
) -> Result<(), ApiError> {
    if let Some(p) = updates.priority_order {
        core_combos::update_target_priority(w, target_id, p)?;
    }
    if let Some(w_val) = updates.weight {
        core_combos::update_target_weight(w, target_id, w_val)?;
    }
    if let Some(active_val) = updates.active {
        core_combos::update_target_active(w, target_id, active_val)?;
    }
    if let Some(mode) = updates.cooldown_mode {
        core_combos::update_target_cooldown_mode(w, target_id, mode)?;
    }
    if let Some(base) = updates.cooldown_base_secs {
        core_combos::update_target_cooldown_base(w, target_id, base)?;
    }
    if let Some(max) = updates.cooldown_max_secs {
        core_combos::update_target_cooldown_max(w, target_id, max)?;
    }
    if let Some(factor) = updates.cooldown_factor {
        core_combos::update_target_cooldown_factor(w, target_id, factor)?;
    }
    Ok(())
}

pub async fn update_combo_target(
    State(s): State<AppState>,
    Path((combo_id, target_id)): Path<(i64, i64)>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let updates = parse_combo_target_updates(&body)?;
    let w = s.db_pool().writer();
    apply_target_db_updates(&w, ComboTargetId(target_id), &updates)?;

    Ok(Json(serde_json::json!({
        "combo_id": combo_id,
        "id": target_id,
        "priority_order": updates.priority_order,
        "weight": body.get("weight").and_then(serde_json::Value::as_i64),
        "active": updates.active,
        "cooldown_mode": body.get("cooldown_mode"),
        "cooldown_base_secs": body.get("cooldown_base_secs"),
    })))
}

crate::admin_entity_action_handler! {
    pub async fn delete_combo_target(
        State(s) with writer(w),
        Path((combo_id, target_id)): Path<(i64, i64)>,
    ) -> Result<Json<serde_json::Value>, ApiError> {
        core_admin::delete_combo_target(&w, ComboId(combo_id), ComboTargetId(target_id))?;
        Ok(Json(serde_json::json!({ "deleted": target_id })))
    }
}

crate::admin_entity_action_handler! {
    pub async fn clear_combo_target_cooldown(
        State(s) with writer(w),
        Path((combo_id, target_id)): Path<(i64, i64)>,
    ) -> Result<Json<serde_json::Value>, ApiError> {
        core_admin::clear_combo_target_cooldown(&w, ComboId(combo_id), ComboTargetId(target_id))?;
        Ok(Json(serde_json::json!({ "ok": true, "cleared": target_id })))
    }
}

/// Body for `POST /admin/combos/:id/targets/reorder`.
#[derive(Debug, Deserialize)]
pub struct ReorderComboTargetsInput {
    pub target_ids: Vec<i64>,
}

pub async fn reorder_combo_targets(
    State(s): State<AppState>,
    Path(combo_id): Path<i64>,
    Json(body): Json<ReorderComboTargetsInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut w = s.db_pool().writer();
    let ordered: Vec<ComboTargetId> = body.target_ids.into_iter().map(ComboTargetId).collect();
    core_admin::reorder_combo_targets(&mut w, ComboId(combo_id), &ordered)?;
    Ok(Json(serde_json::json!({
        "reordered": combo_id,
        "count": ordered.len(),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_nullable_str() {
        let obj = json!({ "valid": "text", "empty": null, "invalid": 123 });
        assert_eq!(
            parse_nullable_str(&obj, "valid").unwrap(),
            Some(Some("text"))
        );
        assert_eq!(parse_nullable_str(&obj, "empty").unwrap(), Some(None));
        assert_eq!(parse_nullable_str(&obj, "missing").unwrap(), None);
        assert!(parse_nullable_str(&obj, "invalid").is_err());
    }

    #[test]
    fn test_parse_nullable_u64() {
        let obj = json!({ "valid": 42, "empty": null, "invalid": "text", "negative": -1 });
        assert_eq!(parse_nullable_u64(&obj, "valid").unwrap(), Some(Some(42)));
        assert_eq!(parse_nullable_u64(&obj, "empty").unwrap(), Some(None));
        assert_eq!(parse_nullable_u64(&obj, "missing").unwrap(), None);
        assert!(parse_nullable_u64(&obj, "invalid").is_err());
        assert!(parse_nullable_u64(&obj, "negative").is_err());
    }

    #[test]
    fn test_parse_nullable_i64() {
        let obj = json!({ "valid": -42, "empty": null, "invalid": "text" });
        assert_eq!(parse_nullable_i64(&obj, "valid").unwrap(), Some(Some(-42)));
        assert_eq!(parse_nullable_i64(&obj, "empty").unwrap(), Some(None));
        assert_eq!(parse_nullable_i64(&obj, "missing").unwrap(), None);
        assert!(parse_nullable_i64(&obj, "invalid").is_err());
    }

    #[test]
    fn test_parse_nullable_f64() {
        let obj = json!({ "valid": 0.5, "empty": null, "invalid": "text" });
        assert_eq!(parse_nullable_f64(&obj, "valid").unwrap(), Some(Some(0.5)));
        assert_eq!(parse_nullable_f64(&obj, "empty").unwrap(), Some(None));
        assert_eq!(parse_nullable_f64(&obj, "missing").unwrap(), None);
        assert!(parse_nullable_f64(&obj, "invalid").is_err());
    }

    #[test]
    fn test_parse_target_weight() {
        assert_eq!(parse_target_weight(&json!({})).unwrap(), None);
        assert_eq!(parse_target_weight(&json!({"weight": 1})).unwrap(), Some(1));
        assert_eq!(
            parse_target_weight(&json!({"weight": i32::MAX})).unwrap(),
            Some(i32::MAX)
        );
        assert!(parse_target_weight(&json!({"weight": 0})).is_err()); // less than 1
        assert!(parse_target_weight(&json!({"weight": i64::from(i32::MAX) + 1})).is_err()); // larger than i32::MAX
        assert!(parse_target_weight(&json!({"weight": "text"})).is_err()); // invalid type
    }

    #[test]
    fn test_parse_target_priority_order() {
        assert_eq!(parse_target_priority_order(&json!({})).unwrap(), None);
        assert_eq!(
            parse_target_priority_order(&json!({"priority_order": 0})).unwrap(),
            Some(0)
        );
        assert_eq!(
            parse_target_priority_order(&json!({"priority_order": i32::MIN})).unwrap(),
            Some(i32::MIN)
        );
        assert_eq!(
            parse_target_priority_order(&json!({"priority_order": i32::MAX})).unwrap(),
            Some(i32::MAX)
        );
        assert!(
            parse_target_priority_order(&json!({"priority_order": i64::from(i32::MAX) + 1}))
                .is_err()
        ); // out of range
        assert!(parse_target_priority_order(&json!({"priority_order": "text"})).is_err()); // invalid type
    }

    #[test]
    fn test_build_skipped_target_entry() {
        // Test sub_combo skip
        let t_sub = types_combos::ComboTargetWithModel {
            id: ComboTargetId(1),
            combo_id: ComboId(2),
            model_row_id: None,
            account_id: None,
            priority_order: 1,
            weight: 1,
            active: true,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            in_cooldown: false,
            cooldown_reason: None,
            cooldown_until: None,
            provider_id: openproxy_types::ProviderId("openai".to_string()),
            model_id: "test".into(),
            model_display_name: Some("Test".into()),
            context_length: Some(4096),
            max_output_tokens: Some(4096),
            provider_active: true,
            sub_combo_id: Some(ComboId(3)),
            sub_combo_name: Some("Sub".into()),
        };
        let res_sub = build_skipped_target_entry(&t_sub).expect("Should be skipped");
        assert_eq!(res_sub["target_id"], 1);
        assert_eq!(res_sub["sub_combo_id"], 3);
        assert_eq!(res_sub["skipped"], true);

        // Test cooldown skip
        let t_cd = types_combos::ComboTargetWithModel {
            id: ComboTargetId(2),
            combo_id: ComboId(2),
            model_row_id: Some(ModelRowId(4)),
            account_id: Some(openproxy_types::AccountId(5)),
            priority_order: 1,
            weight: 1,
            active: true,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            in_cooldown: true,
            cooldown_reason: Some("Rate limited".into()),
            cooldown_until: None,
            provider_id: openproxy_types::ProviderId("anthropic".to_string()),
            model_id: "test".into(),
            model_display_name: Some("Test".into()),
            context_length: Some(4096),
            max_output_tokens: Some(4096),
            provider_active: true,
            sub_combo_id: None,
            sub_combo_name: None,
        };
        let res_cd = build_skipped_target_entry(&t_cd).expect("Should be skipped");
        assert_eq!(res_cd["target_id"], 2);
        assert_eq!(res_cd["error_msg"], "in_cooldown: Rate limited");
        assert_eq!(res_cd["skipped"], true);

        // Test active not skipped
        let mut t_active = t_cd;
        t_active.in_cooldown = false;
        assert!(build_skipped_target_entry(&t_active).is_none());
    }
}
