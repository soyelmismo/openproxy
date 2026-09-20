use super::{ApiError, AppState, ComboId, CoreError, core_combos, types_combos};
use axum::{
    Json,
    extract::{Path, State},
};
use openproxy_types::UpdateField;

use openproxy_core::admin as core_admin;

pub use super::combo_targets::{
    ComboTargetUpdates, ReorderComboTargetsInput, add_target, clear_combo_target_cooldown,
    delete_combo_target, list_combo_targets, list_valid_sub_combos, reorder_combo_targets,
    test_combo_targets, update_combo_target,
};

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

pub(crate) fn parse_nullable_str<'a>(
    body: &'a serde_json::Value,
    field: &str,
) -> Result<UpdateField<&'a str>, ApiError> {
    match body.get(field) {
        None => Ok(UpdateField::Ignore),
        Some(v) if v.is_null() => Ok(UpdateField::Reset),
        Some(v) => match v.as_str() {
            Some(s) => Ok(UpdateField::Set(s)),
            None => Err(ApiError(CoreError::Validation(format!(
                "{field} must be a string or null, got {v}"
            )))),
        },
    }
}

pub(crate) fn parse_nullable_u64(
    body: &serde_json::Value,
    field: &str,
) -> Result<UpdateField<u64>, ApiError> {
    match body.get(field) {
        None => Ok(UpdateField::Ignore),
        Some(v) if v.is_null() => Ok(UpdateField::Reset),
        Some(v) => match v.as_u64() {
            Some(n) => Ok(UpdateField::Set(n)),
            None => Err(ApiError(CoreError::Validation(format!(
                "{field} must be a non-negative integer or null"
            )))),
        },
    }
}

pub(crate) fn parse_nullable_i64(
    body: &serde_json::Value,
    field: &str,
) -> Result<UpdateField<i64>, ApiError> {
    match body.get(field) {
        None => Ok(UpdateField::Ignore),
        Some(v) if v.is_null() => Ok(UpdateField::Reset),
        Some(v) => match v.as_i64() {
            Some(n) => Ok(UpdateField::Set(n)),
            None => Err(ApiError(CoreError::Validation(format!(
                "{field} must be null or an integer"
            )))),
        },
    }
}

pub(crate) fn parse_nullable_f64(
    body: &serde_json::Value,
    field: &str,
) -> Result<UpdateField<f64>, ApiError> {
    match body.get(field) {
        None => Ok(UpdateField::Ignore),
        Some(v) if v.is_null() => Ok(UpdateField::Reset),
        Some(v) => match v.as_f64() {
            Some(n) => Ok(UpdateField::Set(n)),
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
    match parse_nullable_str(body, "cooldown_mode")? {
        UpdateField::Set(v) => core_combos::update_cooldown_mode(w, id, Some(v))?,
        UpdateField::Reset => core_combos::update_cooldown_mode(w, id, None)?,
        UpdateField::Ignore => {}
    }
    match parse_nullable_u64(body, "cooldown_base_secs")? {
        UpdateField::Set(v) => core_combos::update_cooldown_base(w, id, Some(v))?,
        UpdateField::Reset => core_combos::update_cooldown_base(w, id, None)?,
        UpdateField::Ignore => {}
    }
    match parse_nullable_u64(body, "cooldown_max_secs")? {
        UpdateField::Set(v) => core_combos::update_cooldown_max(w, id, Some(v))?,
        UpdateField::Reset => core_combos::update_cooldown_max(w, id, None)?,
        UpdateField::Ignore => {}
    }
    match parse_nullable_u64(body, "cooldown_factor")? {
        UpdateField::Set(v) => core_combos::update_cooldown_factor(w, id, Some(v as u32))?,
        UpdateField::Reset => core_combos::update_cooldown_factor(w, id, None)?,
        UpdateField::Ignore => {}
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
    if let Some(v) = body.get("strategy") {
        let strategy = v.as_str().ok_or_else(|| {
            ApiError(CoreError::Validation(
                "strategy must be a string when present".into(),
            ))
        })?;
        core_combos::update_strategy(w, id, strategy)?;
    }
    match parse_nullable_i64(body, "context_window")? {
        UpdateField::Set(v) => core_combos::update_context_window(w, id, Some(v))?,
        UpdateField::Reset => core_combos::update_context_window(w, id, None)?,
        UpdateField::Ignore => {}
    }
    match parse_nullable_str(body, "priority_mode")? {
        UpdateField::Set(v) => core_combos::update_priority_mode(w, id, Some(v))?,
        UpdateField::Reset => core_combos::update_priority_mode(w, id, None)?,
        UpdateField::Ignore => {}
    }
    match parse_nullable_f64(body, "lkgp_exploration_rate")? {
        UpdateField::Set(v) => core_combos::update_lkgp_settings(w, id, Some(v))?,
        UpdateField::Reset => core_combos::update_lkgp_settings(w, id, None)?,
        UpdateField::Ignore => {}
    }
    match parse_nullable_u64(body, "selection_window_secs")? {
        UpdateField::Set(v) => core_combos::update_selection_window(w, id, Some(v))?,
        UpdateField::Reset => core_combos::update_selection_window(w, id, None)?,
        UpdateField::Ignore => {}
    }
    match parse_nullable_str(body, "decision_model")? {
        UpdateField::Set(v) => core_combos::update_decision_model(w, id, Some(v))?,
        UpdateField::Reset => core_combos::update_decision_model(w, id, None)?,
        UpdateField::Ignore => {}
    }
    match parse_nullable_u64(body, "decision_timeout_ms")? {
        UpdateField::Set(v) => core_combos::update_decision_timeout(w, id, Some(v))?,
        UpdateField::Reset => core_combos::update_decision_timeout(w, id, None)?,
        UpdateField::Ignore => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_nullable_str() {
        let obj = json!({ "valid": "text", "empty": null, "invalid": 123 });
        assert_eq!(
            parse_nullable_str(&obj, "valid").unwrap(),
            UpdateField::Set("text")
        );
        assert_eq!(
            parse_nullable_str(&obj, "empty").unwrap(),
            UpdateField::Reset
        );
        assert_eq!(
            parse_nullable_str(&obj, "missing").unwrap(),
            UpdateField::Ignore
        );
        assert!(parse_nullable_str(&obj, "invalid").is_err());
    }

    #[test]
    fn test_parse_nullable_u64() {
        let obj = json!({ "valid": 42, "empty": null, "invalid": "text", "negative": -1 });
        assert_eq!(
            parse_nullable_u64(&obj, "valid").unwrap(),
            UpdateField::Set(42)
        );
        assert_eq!(
            parse_nullable_u64(&obj, "empty").unwrap(),
            UpdateField::Reset
        );
        assert_eq!(
            parse_nullable_u64(&obj, "missing").unwrap(),
            UpdateField::Ignore
        );
        assert!(parse_nullable_u64(&obj, "invalid").is_err());
        assert!(parse_nullable_u64(&obj, "negative").is_err());
    }

    #[test]
    fn test_parse_nullable_i64() {
        let obj = json!({ "valid": -42, "empty": null, "invalid": "text" });
        assert_eq!(
            parse_nullable_i64(&obj, "valid").unwrap(),
            UpdateField::Set(-42)
        );
        assert_eq!(
            parse_nullable_i64(&obj, "empty").unwrap(),
            UpdateField::Reset
        );
        assert_eq!(
            parse_nullable_i64(&obj, "missing").unwrap(),
            UpdateField::Ignore
        );
        assert!(parse_nullable_i64(&obj, "invalid").is_err());
    }

    #[test]
    fn test_parse_nullable_f64() {
        let obj = json!({ "valid": 0.5, "empty": null, "invalid": "text" });
        assert_eq!(
            parse_nullable_f64(&obj, "valid").unwrap(),
            UpdateField::Set(0.5)
        );
        assert_eq!(
            parse_nullable_f64(&obj, "empty").unwrap(),
            UpdateField::Reset
        );
        assert_eq!(
            parse_nullable_f64(&obj, "missing").unwrap(),
            UpdateField::Ignore
        );
        assert!(parse_nullable_f64(&obj, "invalid").is_err());
    }
}
