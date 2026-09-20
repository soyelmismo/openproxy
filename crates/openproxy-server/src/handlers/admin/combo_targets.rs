use super::{
    ApiError, AppState, Arc, ComboId, ComboTargetId, CoreError, Deserialize, ModelRowId,
    TestOptions, core_combos, run_test_for_model, types_combos,
};
use axum::{
    Json,
    extract::{Path, State},
};
use openproxy_core::admin as core_admin;
use openproxy_types::UpdateField;

use super::combos::{parse_nullable_str, parse_nullable_u64};

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

pub fn build_skipped_target_entry(
    t: &types_combos::ComboTargetWithModel,
) -> Option<serde_json::Value> {
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

pub struct ComboTargetUpdates<'a> {
    pub priority_order: Option<i32>,
    pub weight: Option<i32>,
    pub active: Option<bool>,
    pub cooldown_mode: UpdateField<&'a str>,
    pub cooldown_base_secs: UpdateField<u64>,
    pub cooldown_max_secs: UpdateField<u64>,
    pub cooldown_factor: UpdateField<u32>,
    pub thinking_effort: UpdateField<&'a str>,
    pub description: UpdateField<&'a str>,
}

impl ComboTargetUpdates<'_> {
    pub fn is_empty(&self) -> bool {
        self.priority_order.is_none()
            && self.weight.is_none()
            && self.active.is_none()
            && self.cooldown_mode.is_ignore()
            && self.cooldown_base_secs.is_ignore()
            && self.cooldown_max_secs.is_ignore()
            && self.cooldown_factor.is_ignore()
            && self.thinking_effort.is_ignore()
            && self.description.is_ignore()
    }
}

pub fn parse_target_priority_order(body: &serde_json::Value) -> Result<Option<i32>, ApiError> {
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

pub fn parse_target_weight(body: &serde_json::Value) -> Result<Option<i32>, ApiError> {
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

pub fn parse_target_active(body: &serde_json::Value) -> Result<Option<bool>, ApiError> {
    let Some(v) = body.get("active") else {
        return Ok(None);
    };
    v.as_bool().map(Some).ok_or_else(|| {
        ApiError(CoreError::Validation(
            "active must be a boolean when present".into(),
        ))
    })
}

pub fn parse_combo_target_updates(
    body: &serde_json::Value,
) -> Result<ComboTargetUpdates<'_>, ApiError> {
    let priority_order = parse_target_priority_order(body)?;
    let weight = parse_target_weight(body)?;
    let active = parse_target_active(body)?;
    let cooldown_mode = parse_nullable_str(body, "cooldown_mode")?;
    let cooldown_base_secs = parse_nullable_u64(body, "cooldown_base_secs")?;
    let cooldown_max_secs = parse_nullable_u64(body, "cooldown_max_secs")?;
    let cooldown_factor = parse_nullable_u64(body, "cooldown_factor")?.map(|f| f as u32);
    let thinking_effort = parse_nullable_str(body, "thinking_effort")?;
    let description = parse_nullable_str(body, "description")?;

    let updates = ComboTargetUpdates {
        priority_order,
        weight,
        active,
        cooldown_mode,
        cooldown_base_secs,
        cooldown_max_secs,
        cooldown_factor,
        thinking_effort,
        description,
    };

    if updates.is_empty() {
        return Err(ApiError(CoreError::Validation(
            "missing update fields in request body".into(),
        )));
    }
    Ok(updates)
}

pub fn apply_target_db_updates(
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
    match updates.cooldown_mode {
        UpdateField::Set(v) => core_combos::update_target_cooldown_mode(w, target_id, Some(v))?,
        UpdateField::Reset => core_combos::update_target_cooldown_mode(w, target_id, None)?,
        UpdateField::Ignore => {}
    }
    match updates.cooldown_base_secs {
        UpdateField::Set(v) => core_combos::update_target_cooldown_base(w, target_id, Some(v))?,
        UpdateField::Reset => core_combos::update_target_cooldown_base(w, target_id, None)?,
        UpdateField::Ignore => {}
    }
    match updates.cooldown_max_secs {
        UpdateField::Set(v) => core_combos::update_target_cooldown_max(w, target_id, Some(v))?,
        UpdateField::Reset => core_combos::update_target_cooldown_max(w, target_id, None)?,
        UpdateField::Ignore => {}
    }
    match updates.cooldown_factor {
        UpdateField::Set(v) => core_combos::update_target_cooldown_factor(w, target_id, Some(v))?,
        UpdateField::Reset => core_combos::update_target_cooldown_factor(w, target_id, None)?,
        UpdateField::Ignore => {}
    }
    match updates.thinking_effort {
        UpdateField::Set(v) => core_combos::update_target_thinking_effort(w, target_id, Some(v))?,
        UpdateField::Reset => core_combos::update_target_thinking_effort(w, target_id, None)?,
        UpdateField::Ignore => {}
    }
    match updates.description {
        UpdateField::Set(v) => core_combos::update_target_description(w, target_id, Some(v))?,
        UpdateField::Reset => core_combos::update_target_description(w, target_id, None)?,
        UpdateField::Ignore => {}
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
        "description": body.get("description"),
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
    fn test_parse_target_weight() {
        assert_eq!(parse_target_weight(&json!({})).unwrap(), None);
        assert_eq!(parse_target_weight(&json!({"weight": 1})).unwrap(), Some(1));
        assert_eq!(
            parse_target_weight(&json!({"weight": i32::MAX})).unwrap(),
            Some(i32::MAX)
        );
        assert!(parse_target_weight(&json!({"weight": 0})).is_err());
        assert!(parse_target_weight(&json!({"weight": i64::from(i32::MAX) + 1})).is_err());
        assert!(parse_target_weight(&json!({"weight": "text"})).is_err());
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
        );
        assert!(parse_target_priority_order(&json!({"priority_order": "text"})).is_err());
    }

    #[test]
    fn test_parse_combo_target_updates_thinking_effort() {
        let val_set = json!({"thinking_effort": "high"});
        let updates = parse_combo_target_updates(&val_set).unwrap();
        assert_eq!(updates.thinking_effort, UpdateField::Set("high"));

        let val_clear = json!({"thinking_effort": null});
        let updates_clear = parse_combo_target_updates(&val_clear).unwrap();
        assert_eq!(updates_clear.thinking_effort, UpdateField::Reset);

        let val_ignore = json!({"weight": 2});
        let updates_ignore = parse_combo_target_updates(&val_ignore).unwrap();
        assert_eq!(updates_ignore.thinking_effort, UpdateField::Ignore);

        assert!(parse_combo_target_updates(&json!({})).is_err());
        let val_err = json!({"thinking_effort": 123});
        assert!(parse_combo_target_updates(&val_err).is_err());
    }

    fn make_test_target(id: i64, combo_id: i64) -> types_combos::ComboTargetWithModel {
        types_combos::ComboTargetWithModel {
            id: ComboTargetId(id),
            combo_id: ComboId(combo_id),
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
            sub_combo_id: None,
            sub_combo_name: None,
            thinking_effort: None,
            description: None,
        }
    }

    #[test]
    fn test_build_skipped_target_entry() {
        // Test sub_combo skip
        let mut t_sub = make_test_target(1, 2);
        t_sub.sub_combo_id = Some(ComboId(3));
        t_sub.sub_combo_name = Some("Sub".into());
        let res_sub = build_skipped_target_entry(&t_sub).expect("Should be skipped");
        assert_eq!(res_sub["target_id"], 1);
        assert_eq!(res_sub["sub_combo_id"], 3);
        assert_eq!(res_sub["skipped"], true);

        // Test cooldown skip
        let mut t_cd = make_test_target(2, 2);
        t_cd.model_row_id = Some(ModelRowId(4));
        t_cd.account_id = Some(openproxy_types::AccountId(5));
        t_cd.in_cooldown = true;
        t_cd.cooldown_reason = Some("Rate limited".into());
        t_cd.provider_id = openproxy_types::ProviderId("anthropic".to_string());
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
