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

pub async fn test_combo_targets(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    cancel_watch: Option<axum::Extension<crate::disconnect::CancelWatch>>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    use serde_json::json;

    let cancel_rx = cancel_watch.map(|axum::Extension(cw)| cw.rx);

    // Cancellation note: the previous implementation spawned a
    // disconnect-watcher task that drained `request.into_parts().1`
    // (the request body) and flipped a `tokio::sync::watch` flag
    // when the body stream ended. For a POST with no body — which is
    // what the dashboard actually sends — `Body::frame()` resolves
    // to `None` immediately, so the watcher fired `disconnect_tx`
    // before the fan-out loop started its second iteration. The
    // fan-out then aborted after the first target, which silently
    // broke "Test all".
    //
    // We rely on Axum's natural cancellation instead: when the
    // client drops the response future (closes the tab, navigates
    // away, etc.), the handler future is dropped, which in turn
    // drops the in-flight `UpstreamClient::call()` future
    // (UpstreamClient is cancel-safe) and aborts the loop. No watcher
    // task is needed — and a watcher task is in fact *counter-
    // productive* because it would outlive the handler and never
    // observe the drop. The 180s `tokio::time::timeout` below
    // remains the upper bound for the happy path.
    let res: Result<Json<Vec<serde_json::Value>>, crate::error::ApiError> = async {
        // Snapshot the targets up-front and drop the writer guard.
        // The per-target test below does its own short DB
        // transactions (writer lock + drop), so the long-running
        // HTTP calls don't block other handlers from writing.
        let targets = tokio::task::spawn_blocking({
            let pool = Arc::clone(s.db_pool());
            move || {
                let w = pool.writer();
                core_combos::list_targets_with_model(&w, ComboId(id))
            }
        })
        .await
        .unwrap_or_else(|e| Err(CoreError::Internal(format!("spawn_blocking failed: {e}"))))?;

        // The fan-out is intentionally serial. The prompt explicitly
        // asked for no parallelization in the MVP ("NO paralelizar.
        // Secuencial está bien para MVP. Documentar como
        // follow-up"); the comment on the inner loop is the
        // follow-up. We do, however, bound the whole fan-out with a
        // hard timeout so the dashboard never waits longer than 3
        // minutes — the worst case is 8 targets × 15 s each.
        let fan_out = async {
            let mut results = Vec::with_capacity(targets.len());
            for t in targets {
                if t.sub_combo_id.is_some() {
                    // Sub-combo row: do not recurse. The "test
                    // children individually" message mirrors the
                    // pre-refactor handler so existing dashboard
                    // tooltip behavior is preserved.
                    results.push(json!({
                        "target_id": t.id.0,
                        "sub_combo_id": t.sub_combo_id.map(|c| c.0),
                        "sub_combo_name": t.sub_combo_name,
                        "provider_id": t.provider_id.to_string(),
                        "status": 0_i32,
                        "elapsed_ms": serde_json::Value::Null,
                        "error_msg": "sub-combo; test children individually",
                        "skipped": true,
                    }));
                    continue;
                }
                if t.in_cooldown {
                    // The target is parked. Surface that as a
                    // skipped row with the same shape the dashboard
                    // already knows, and copy the reason into the
                    // error message so the operator can see *why*
                    // the row is parked without opening a second
                    // endpoint.
                    results.push(json!({
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
                    continue;
                }
                if let Some(ref rx) = cancel_rx
                    && rx.borrow().is_some()
                {
                    tracing::info!("test_combo_targets: client disconnected, aborting fan-out");
                    break;
                }
                // Flat, active, not in cooldown: actually fire
                // upstream. The helper handles the model-not-active
                // short-circuit itself (skipped row with
                // "model is inactive" in the error_msg).
                let (r, _) = run_test_for_model(
                    &s,
                    t.model_row_id.unwrap_or(ModelRowId(0)).0,
                    t.account_id,
                    None,
                    TestOptions {
                        in_combo_fanout: true,
                    },
                    cancel_rx.clone(),
                )
                .await;
                // Use the per-target metadata from the snapshot
                // for the response, not whatever the helper
                // returned (the helper doesn't have the row
                // metadata handy). `r.row_id` is informational
                // and matches `t.model_row_id`.
                let mut obj = json!({
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
                        json!(r.skip_reason.unwrap_or_else(|| "skipped".to_string()));
                }
                results.push(obj);
            }
            results
        };

        let Ok(results) = tokio::time::timeout(std::time::Duration::from_mins(3), fan_out).await
        else {
            // Timed out before we finished. Return whatever we
            // have so the dashboard can render the partial
            // picture. The frontend treats the response shape
            // uniformly; a 504 here would just wipe the
            // button state with no data.
            tracing::warn!(combo_id = id, "test-all fan-out exceeded 180s budget");
            return Err(crate::error::ApiError(
                openproxy_types::CoreError::Internal(
                    "test-all exceeded 180s budget; partial results dropped".into(),
                ),
            ));
        };

        Ok(Json(results))
    }
    .await;
    res
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

pub async fn update_combo(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let w = s.db_pool().writer();
    // Optional race_size update.
    if let Some(n) = body.get("race_size").and_then(serde_json::Value::as_u64) {
        let rs = u8::try_from(n).unwrap_or(0);
        core_combos::update_combo(&w, ComboId(id), Some(rs))?;
    }
    // Optional context_window update. `null` or missing means
    // "auto-compute from targets". A positive integer pins the
    // reported context window.
    if let Some(cw_val) = body.get("context_window") {
        let cw = if cw_val.is_null() {
            None
        } else {
            Some(cw_val.as_i64().ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "context_window must be null or an integer".into(),
                ))
            })?)
        };
        core_combos::update_context_window(&w, ComboId(id), cw)?;
    }
    // Optional `priority_mode` update. `null` clears the column
    // back to `strict` (the legacy default).
    if let Some(v) = body.get("priority_mode") {
        let mode = if v.is_null() {
            None
        } else if let Some(s) = v.as_str() {
            Some(s)
        } else {
            return Err(ApiError(CoreError::Validation(format!(
                "priority_mode must be a string or null, got {v}"
            ))));
        };
        core_combos::update_priority_mode(&w, ComboId(id), mode)?;
    }
    // Optional cooldown settings update. Each field is updated
    // INDEPENDENTLY — if only `cooldown_base_secs` is in the body,
    // only that column is written, leaving `cooldown_mode` etc.
    // untouched. This prevents the "changing base resets mode to
    // flat" bug.
    //
    // The frontend sends one field at a time (e.g. `{cooldown_base_secs: 30}`)
    // so we must NOT batch them into a single UPDATE that would
    // NULL out the absent fields.
    if let Some(v) = body.get("cooldown_mode") {
        let mode = if v.is_null() {
            None
        } else if let Some(s) = v.as_str() {
            Some(s)
        } else {
            return Err(ApiError(CoreError::Validation(format!(
                "cooldown_mode must be a string or null, got {v}"
            ))));
        };
        core_combos::update_cooldown_mode(&w, ComboId(id), mode)?;
    }
    if let Some(v) = body.get("cooldown_base_secs") {
        let base = if v.is_null() {
            None
        } else {
            Some(v.as_u64().ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "cooldown_base_secs must be a non-negative integer or null".into(),
                ))
            })?)
        };
        core_combos::update_cooldown_base(&w, ComboId(id), base)?;
    }
    if let Some(v) = body.get("cooldown_max_secs") {
        let max = if v.is_null() {
            None
        } else {
            Some(v.as_u64().ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "cooldown_max_secs must be a non-negative integer or null".into(),
                ))
            })?)
        };
        core_combos::update_cooldown_max(&w, ComboId(id), max)?;
    }
    if let Some(v) = body.get("cooldown_factor") {
        let factor = if v.is_null() {
            None
        } else {
            Some(v.as_u64().ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "cooldown_factor must be a non-negative integer or null".into(),
                ))
            })? as u32)
        };
        core_combos::update_cooldown_factor(&w, ComboId(id), factor)?;
    }
    // Optional LKGP exploration rate update.
    if let Some(v) = body.get("lkgp_exploration_rate") {
        let rate = if v.is_null() {
            None
        } else {
            Some(v.as_f64().ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "lkgp_exploration_rate must be a number in [0.0, 1.0] or null".into(),
                ))
            })?)
        };
        core_combos::update_lkgp_settings(&w, ComboId(id), rate)?;
    }
    // Optional selection window update.
    if let Some(v) = body.get("selection_window_secs") {
        let window = if v.is_null() {
            None
        } else {
            Some(v.as_u64().ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "selection_window_secs must be a non-negative integer or null".into(),
                ))
            })?)
        };
        core_combos::update_selection_window(&w, ComboId(id), window)?;
    }
    Ok(Json(serde_json::json!({ "id": id })))
}

pub async fn update_combo_target(
    State(s): State<AppState>,
    Path((combo_id, target_id)): Path<(i64, i64)>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Optional `priority_order` — the historical primary field.
    // Kept optional so a future dashboard that only wants to
    // PATCH `weight` can do so without round-tripping the order.
    let priority_order: Option<i32> = match body.get("priority_order") {
        None => None,
        Some(v) => {
            let p = v.as_i64().ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "priority_order must be an integer when present".into(),
                ))
            })?;
            // Cast: i32 is well under i64::MAX in practice; the SQL
            // column is INTEGER (i64 in rusqlite) so a non-negative
            // i32 is safe.
            if p < i64::from(i32::MIN) || p > i64::from(i32::MAX) {
                return Err(ApiError(CoreError::Validation(format!(
                    "priority_order out of i32 range: {p}"
                ))));
            }
            Some(p as i32)
        }
    };
    // Optional `weight` (migration 000035).
    let weight: Option<i32> = match body.get("weight") {
        None => None,
        Some(v) => {
            let weight_i64 = v.as_i64().ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "weight must be an integer when present".into(),
                ))
            })?;
            // Range-check before the i32 cast so an out-of-range
            // value surfaces as a 400 instead of a silent wrap.
            if weight_i64 < 1 || weight_i64 > i64::from(i32::MAX) {
                return Err(ApiError(CoreError::Validation(format!(
                    "weight must be a positive i32 (1..={}), got {}",
                    i32::MAX,
                    weight_i64
                ))));
            }
            Some(weight_i64 as i32)
        }
    };
    // Optional `active` flag.
    let active: Option<bool> = match body.get("active") {
        None => None,
        Some(v) => Some(v.as_bool().ok_or_else(|| {
            ApiError(CoreError::Validation(
                "active must be a boolean when present".into(),
            ))
        })?),
    };
    // Optional per-target `cooldown_mode`
    let cooldown_mode: Option<Option<&str>> = match body.get("cooldown_mode") {
        None => None,
        Some(v) if v.is_null() => Some(None),
        Some(v) => match v.as_str() {
            Some(s) => Some(Some(s)),
            None => {
                return Err(ApiError(CoreError::Validation(format!(
                    "cooldown_mode must be a string or null, got {v}"
                ))));
            }
        },
    };
    // Optional per-target `cooldown_base_secs`
    let cooldown_base_secs: Option<Option<u64>> = match body.get("cooldown_base_secs") {
        None => None,
        Some(v) if v.is_null() => Some(None),
        Some(v) => {
            let base = v.as_u64().ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "cooldown_base_secs must be a non-negative integer or null".into(),
                ))
            })?;
            Some(Some(base))
        }
    };
    // Optional per-target `cooldown_max_secs`
    let cooldown_max_secs: Option<Option<u64>> = match body.get("cooldown_max_secs") {
        None => None,
        Some(v) if v.is_null() => Some(None),
        Some(v) => {
            let max = v.as_u64().ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "cooldown_max_secs must be a non-negative integer or null".into(),
                ))
            })?;
            Some(Some(max))
        }
    };
    // Optional per-target `cooldown_factor`
    let cooldown_factor: Option<Option<u32>> = match body.get("cooldown_factor") {
        None => None,
        Some(v) if v.is_null() => Some(None),
        Some(v) => {
            let factor = v.as_u64().ok_or_else(|| {
                ApiError(CoreError::Validation(
                    "cooldown_factor must be a non-negative integer or null".into(),
                ))
            })? as u32;
            Some(Some(factor))
        }
    };
    // Backwards-compat: if no known field was present, surface validation error
    if priority_order.is_none()
        && weight.is_none()
        && active.is_none()
        && cooldown_mode.is_none()
        && cooldown_base_secs.is_none()
        && cooldown_max_secs.is_none()
        && cooldown_factor.is_none()
    {
        return Err(ApiError(CoreError::Validation(
            "missing update fields in request body".into(),
        )));
    }

    let w = s.db_pool().writer();
    if let Some(p) = priority_order {
        core_combos::update_target_priority(&w, ComboTargetId(target_id), p)?;
    }
    if let Some(w_val) = weight {
        core_combos::update_target_weight(&w, ComboTargetId(target_id), w_val)?;
    }
    if let Some(active_val) = active {
        core_combos::update_target_active(&w, ComboTargetId(target_id), active_val)?;
    }
    if let Some(mode) = cooldown_mode {
        core_combos::update_target_cooldown_mode(&w, ComboTargetId(target_id), mode)?;
    }
    if let Some(base) = cooldown_base_secs {
        core_combos::update_target_cooldown_base(&w, ComboTargetId(target_id), base)?;
    }
    if let Some(max) = cooldown_max_secs {
        core_combos::update_target_cooldown_max(&w, ComboTargetId(target_id), max)?;
    }
    if let Some(factor) = cooldown_factor {
        core_combos::update_target_cooldown_factor(&w, ComboTargetId(target_id), factor)?;
    }

    Ok(Json(serde_json::json!({
        "combo_id": combo_id,
        "id": target_id,
        "priority_order": priority_order,
        "weight": body.get("weight").and_then(serde_json::Value::as_i64),
        "active": active,
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
