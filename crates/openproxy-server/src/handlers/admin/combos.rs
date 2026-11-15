use super::{ApiError, AppState, ComboId, CoreError, core_combos, types_combos};
use axum::{
    Json,
    extract::{Path, State},
};
use openproxy_types::UpdateField;
use serde::Deserialize;

#[derive(Debug, Deserialize, Default)]
pub struct CloneComboInput {
    pub name: Option<String>,
}

use openproxy_core::admin as core_admin;

pub use super::combo_targets::{
    ComboTargetUpdates, ReorderComboTargetsInput, add_target, clear_combo_target_cooldown,
    create_target_system_prefix, delete_combo_target, delete_target_system_prefix,
    list_combo_targets, list_target_system_prefixes, list_valid_sub_combos, reorder_combo_targets,
    test_combo_targets, update_combo_target, update_target_system_prefix,
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
        .route("/{id}/clone", axum::routing::post(clone_combo))
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
        .route(
            "/{id}/targets/{target_id}/prefixes",
            axum::routing::get(list_target_system_prefixes).post(create_target_system_prefix),
        )
        .route(
            "/{id}/targets/{target_id}/prefixes/{prefix_id}",
            axum::routing::patch(update_target_system_prefix).delete(delete_target_system_prefix),
        )
}

pub async fn list_combos(
    State(s): State<AppState>,
) -> Result<Json<Vec<types_combos::Combo>>, ApiError> {
    // Read-only SELECT — use the READER.
    let list = s.db_pool().spawn_read(core_admin::list_combos).await?;
    Ok(Json(list))
}

pub async fn create_combo(
    State(s): State<AppState>,
    Json(input): Json<core_admin::CreateComboInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = s
        .db_pool()
        .spawn_write(move |w| core_admin::create_combo(w, &input))
        .await?;
    Ok(Json(serde_json::json!({ "id": id.0 })))
}

pub async fn get_combo(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<types_combos::Combo>, ApiError> {
    // Read-only SELECT — use the READER.
    let id = ComboId(id);
    let combo = s
        .db_pool()
        .spawn_read(move |r| core_combos::get_combo(r, id))
        .await?
        .ok_or_else(|| CoreError::ComboNotFound(id.0))?;
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

pub async fn clone_combo(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    body: Option<Json<CloneComboInput>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let id = ComboId(id);
    let override_name = body.and_then(|Json(b)| b.name);
    let new_id = s
        .db_pool()
        .spawn_write(move |w| core_admin::clone_combo(w, id, override_name.as_deref()))
        .await?;
    Ok(Json(serde_json::json!({ "id": new_id.0 })))
}

pub(crate) use super::helpers::{
    parse_nullable_f64, parse_nullable_i64, parse_nullable_str, parse_nullable_u64,
};

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
    if let Some(v) = body.get("name") {
        let name = v.as_str().ok_or_else(|| {
            ApiError(CoreError::Validation(
                "name must be a string when present".into(),
            ))
        })?;
        core_combos::update_name(w, id, name)?;
    }
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
    match parse_nullable_i64(body, "max_output_tokens")? {
        UpdateField::Set(v) => core_combos::update_max_output_tokens(w, id, Some(v))?,
        UpdateField::Reset => core_combos::update_max_output_tokens(w, id, None)?,
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
    match body.get("modality_gate") {
        None => {}
        // An explicit JSON null clears the override so the combo inherits the
        // global `[modality_routing] enabled` setting again.
        Some(serde_json::Value::Null) => core_combos::update_modality_gate(w, id, None)?,
        Some(serde_json::Value::Bool(enabled)) => {
            core_combos::update_modality_gate(w, id, Some(*enabled))?;
        }
        Some(other) => {
            return Err(ApiError(CoreError::Validation(format!(
                "modality_gate must be a boolean or null, got {other}"
            ))));
        }
    }
    match body.get("sanitize_response") {
        None => {}
        Some(serde_json::Value::Null) => core_combos::update_sanitize_response(w, id, None)?,
        Some(serde_json::Value::Bool(enabled)) => {
            core_combos::update_sanitize_response(w, id, Some(*enabled))?;
        }
        Some(other) => {
            return Err(ApiError(CoreError::Validation(format!(
                "sanitize_response must be a boolean or null, got {other}"
            ))));
        }
    }
    match body.get("real_model_id") {
        None => {}
        Some(serde_json::Value::Null) => core_combos::update_real_model_id(w, id, None)?,
        Some(serde_json::Value::String(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                core_combos::update_real_model_id(w, id, None)?;
            } else {
                core_combos::update_real_model_id(w, id, Some(trimmed))?;
            }
        }
        Some(other) => {
            return Err(ApiError(CoreError::Validation(format!(
                "real_model_id must be a string or null, got {other}"
            ))));
        }
    }
    match body.get("purge_reasoning") {
        None => {}
        Some(serde_json::Value::Null) => core_combos::update_purge_reasoning(w, id, None)?,
        Some(serde_json::Value::Bool(enabled)) => {
            core_combos::update_purge_reasoning(w, id, Some(*enabled))?;
        }
        Some(other) => {
            return Err(ApiError(CoreError::Validation(format!(
                "purge_reasoning must be a boolean or null, got {other}"
            ))));
        }
    }
    // Per-combo pricing: input/output is a pair (both rates or neither) and the
    // optional cached-input rate only exists alongside it. The combo's price
    // wins over the target model's on every request routed through it.
    // A key absent from the body keeps its stored value, so fixing just the
    // cached rate never wipes the base pair (and vice versa).
    let touches_pair =
        body.get("pricing_input_per_1m").is_some() || body.get("pricing_output_per_1m").is_some();
    let touches_cached = body.get("pricing_cached_per_1m").is_some();
    if touches_pair || touches_cached {
        let read_rate = |key: &str| -> Result<Option<f64>, ApiError> {
            match body.get(key) {
                None | Some(serde_json::Value::Null) => Ok(None),
                Some(v) => v.as_f64().map(Some).ok_or_else(|| {
                    ApiError(CoreError::Validation(format!(
                        "{key} must be a number or null"
                    )))
                }),
            }
        };
        let current = openproxy_db::combos::get_combo(w, id)?
            .ok_or_else(|| ApiError(CoreError::Validation(format!("combo {id} not found"))))?;
        let input_per_1m = if body.get("pricing_input_per_1m").is_some() {
            read_rate("pricing_input_per_1m")?
        } else {
            current.pricing_input_per_1m
        };
        let output_per_1m = if body.get("pricing_output_per_1m").is_some() {
            read_rate("pricing_output_per_1m")?
        } else {
            current.pricing_output_per_1m
        };
        let cached_per_1m = if input_per_1m.is_none() && output_per_1m.is_none() {
            // Clearing the pair clears the discount with it: a cached rate with
            // nothing to discount is an orphan and must not survive.
            None
        } else if touches_cached {
            read_rate("pricing_cached_per_1m")?
        } else {
            current.pricing_cached_per_1m
        };
        core_combos::update_combo_pricing(w, id, input_per_1m, output_per_1m, cached_per_1m)?;
    }
    Ok(())
}

/// Read the PATCH body's `pricing_schedule` key.
///
/// `Ignore` (key absent) preserves the stored calendar, `Reset` (`null`) clears
/// it and reveals the flat `pricing_*` price, and `Set` replaces it. The
/// calendar is validated here — before the transaction opens — so an unbillable
/// one can never be half-applied.
fn parse_pricing_schedule(
    body: &serde_json::Value,
) -> Result<UpdateField<openproxy_types::pricing_schedule::PricingSchedule>, ApiError> {
    match body.get("pricing_schedule") {
        None => Ok(UpdateField::Ignore),
        Some(serde_json::Value::Null) => Ok(UpdateField::Reset),
        Some(value) => {
            let schedule = serde_json::from_value::<
                openproxy_types::pricing_schedule::PricingSchedule,
            >(value.clone())
            .map_err(|e| {
                ApiError(CoreError::Validation(format!(
                    "pricing_schedule is not a valid schedule: {e}"
                )))
            })?;
            schedule.validate()?;
            Ok(UpdateField::Set(schedule))
        }
    }
}

fn apply_pricing_schedule(
    w: &rusqlite::Connection,
    id: ComboId,
    schedule: UpdateField<openproxy_types::pricing_schedule::PricingSchedule>,
) -> Result<(), ApiError> {
    match schedule {
        UpdateField::Ignore => Ok(()),
        UpdateField::Reset => Ok(core_combos::update_combo_pricing_schedule(w, id, None)?),
        UpdateField::Set(schedule) => Ok(core_combos::update_combo_pricing_schedule(
            w,
            id,
            Some(&schedule),
        )?),
    }
}

pub async fn update_combo(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    crate::error::run_blocking(move || {
        let mut w = s.db_pool().writer();
        let combo_id = ComboId(id);
        // Parse and validate the schedule first: a bad tariff aborts the whole
        // PATCH before the first write.
        let schedule = parse_pricing_schedule(&body)?;
        // One transaction for the whole PATCH: a later field failing (unique
        // name, invalid rate) must not leave the earlier ones applied.
        let tx = w
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| ApiError(openproxy_db::error::map_db_error(e)))?;
        apply_combo_general_updates(&tx, combo_id, &body)?;
        apply_combo_cooldown_updates(&tx, combo_id, &body)?;
        apply_pricing_schedule(&tx, combo_id, schedule)?;
        tx.commit()
            .map_err(|e| ApiError(openproxy_db::error::map_db_error(e)))?;
        Ok(Json(serde_json::json!({ "id": id })))
    })
    .await
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

    fn schedule_body() -> serde_json::Value {
        use openproxy_types::pricing_schedule::{
            PeakClock, PeakWindow, PricingRate, PricingSchedule,
        };
        serde_json::to_value(PricingSchedule {
            timezone: "UTC".to_string(),
            peak: PricingRate {
                input_per_1m: 6.0,
                output_per_1m: 24.0,
                cached_per_1m: Some(3.0),
            },
            off_peak: PricingRate {
                input_per_1m: 2.0,
                output_per_1m: 8.0,
                cached_per_1m: None,
            },
            peak_windows: vec![
                PeakWindow {
                    days: vec![1, 2, 3, 4, 5],
                    start: PeakClock::parse("01:00").expect("start"),
                    end: PeakClock::parse("04:00").expect("end"),
                },
                PeakWindow {
                    days: vec![1, 2, 3, 4, 5],
                    start: PeakClock::parse("06:00").expect("start"),
                    end: PeakClock::parse("10:00").expect("end"),
                },
            ],
            off_peak_dates: vec![
                openproxy_types::pricing_schedule::OffPeakDate::parse("2026-12-25").expect("date"),
            ],
        })
        .expect("serialize schedule")
    }

    /// PATCH is partial: the key must distinguish "leave the calendar alone"
    /// from "remove it", or a client editing a cooldown would silently wipe the
    /// combo's tariff (and a client clearing it would silently keep it).
    #[test]
    fn test_parse_pricing_schedule_partial_patch() {
        assert!(matches!(
            parse_pricing_schedule(&json!({ "name": "renamed" })).unwrap(),
            UpdateField::Ignore
        ));
        assert!(matches!(
            parse_pricing_schedule(&json!({ "pricing_schedule": null })).unwrap(),
            UpdateField::Reset
        ));
        let set = parse_pricing_schedule(&json!({ "pricing_schedule": schedule_body() }))
            .expect("valid schedule");
        let UpdateField::Set(schedule) = set else {
            panic!("an object must replace the calendar");
        };
        schedule
            .validate()
            .expect("round-tripped schedule is valid");
        assert_eq!(schedule.peak_windows.len(), 2);
        assert_eq!(schedule.peak.cached_per_1m, Some(3.0));
        assert_eq!(schedule.off_peak.cached_per_1m, None);
    }

    /// A calendar that cannot bill is rejected before the transaction opens:
    /// non-UTC timezone, overlapping windows, a missing rate pair, unknown
    /// fields, and a malformed date all have to fail loudly.
    #[test]
    fn test_parse_pricing_schedule_rejects_unbillable_calendars() {
        let mut bad_tz = schedule_body();
        bad_tz["timezone"] = json!("Europe/Madrid");
        assert!(parse_pricing_schedule(&json!({ "pricing_schedule": bad_tz })).is_err());

        let mut overlap = schedule_body();
        overlap["peak_windows"] = json!([
            { "days": [1], "start": "01:00", "end": "04:00" },
            { "days": [1], "start": "03:00", "end": "05:00" }
        ]);
        assert!(parse_pricing_schedule(&json!({ "pricing_schedule": overlap })).is_err());

        let mut no_windows = schedule_body();
        no_windows["peak_windows"] = json!([]);
        assert!(parse_pricing_schedule(&json!({ "pricing_schedule": no_windows })).is_err());

        let mut negative = schedule_body();
        negative["off_peak"]["input_per_1m"] = json!(-1.0);
        assert!(parse_pricing_schedule(&json!({ "pricing_schedule": negative })).is_err());

        let mut missing_pair = schedule_body();
        missing_pair["peak"]
            .as_object_mut()
            .expect("object")
            .remove("output_per_1m");
        assert!(parse_pricing_schedule(&json!({ "pricing_schedule": missing_pair })).is_err());

        let mut bad_date = schedule_body();
        bad_date["off_peak_dates"] = json!(["2026-02-30"]);
        assert!(parse_pricing_schedule(&json!({ "pricing_schedule": bad_date })).is_err());

        let mut unknown = schedule_body();
        unknown["surprise"] = json!(true);
        assert!(parse_pricing_schedule(&json!({ "pricing_schedule": unknown })).is_err());
    }
}
