use super::{
    ApiError, AppState, Arc, CoreError, Deserialize, ModelRowId, ProviderId, adapters, core_models,
    core_providers,
};
use axum::{
    Json,
    extract::{Path, Query, State},
};

use openproxy_core::admin as core_admin;

pub use super::model_tester::{
    TEST_ERROR_BODY_MAX_CHARS, TestModelInput, TestOptions, TestResult, run_test_for_model,
    test_model,
};

/// Client-facing model catalog for the portal's Models view: the key-filtered
/// view of `GET /v1/models` enriched with per-1M pricing. Pricing comes from
/// the same lookup the metering path uses, so displayed rates match billing.
/// Served to any admin-API identity (manage or analytics scope).
pub async fn catalog(
    State(s): State<AppState>,
    identity: Option<axum::Extension<super::auth::AdminIdentity>>,
) -> Result<axum::response::Response, ApiError> {
    let key = identity.as_ref().and_then(|id| id.key.clone());
    let flight_key = format!(
        "catalog:{}",
        crate::handlers::models::catalog_scope(key.as_deref())
    );
    s.read_flights()
        .json(flight_key, move || {
            let raw_models = s
                .services()
                .models
                .list_active_all(std::time::Duration::from_secs(5))?;
            let raw_combos = s.services().combos.list_combos()?;

            let rows = crate::handlers::models::filter_models_for_key(raw_models, key.as_deref());
            let combo_rows =
                crate::handlers::models::filter_combos_for_key(raw_combos, key.as_deref());

            // Pricing map: one pass over the models.dev sync table, exact
            // (provider, model) first, normalized model id as fallback.
            let pool = std::sync::Arc::clone(s.db_pool());
            let conn = pool
                .try_reader_for(std::time::Duration::from_secs(5))
                .ok_or_else(|| ApiError(CoreError::Internal("reader lock timeout".into())))?;
            let mut pricing_exact: std::collections::HashMap<
                (String, String),
                (f64, f64, Option<f64>),
            > = std::collections::HashMap::new();
            let mut pricing_normalized: std::collections::HashMap<String, (f64, f64, Option<f64>)> =
                std::collections::HashMap::new();
            {
                let mut stmt = conn
                    .prepare_cached(
                        "SELECT provider_id, model_id, model_id_normalized, \
                     pricing_input_per_1m, pricing_output_per_1m, pricing_cached_per_1m \
                     FROM model_capabilities_sync WHERE pricing_input_per_1m IS NOT NULL",
                    )
                    .map_err(|e| ApiError(openproxy_db::error::map_db_error(e)))?;
                let mapped = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, Option<String>>(2)?,
                            row.get::<_, f64>(3)?,
                            row.get::<_, f64>(4)?,
                            row.get::<_, Option<f64>>(5)?,
                        ))
                    })
                    .map_err(|e| ApiError(openproxy_db::error::map_db_error(e)))?;
                for r in mapped {
                    let (prov, model, normalized, inp, out, cached) =
                        r.map_err(|e| ApiError(openproxy_db::error::map_db_error(e)))?;
                    pricing_exact
                        .entry((prov, model))
                        .or_insert((inp, out, cached));
                    if let Some(norm) = normalized {
                        pricing_normalized.entry(norm).or_insert((inp, out, cached));
                    }
                }
            }

            // Combo resolution acquires its own readers. Release pricing's
            // connection before nested lookups or serializing a large catalog.
            drop(conn);
            let mut data: Vec<serde_json::Value> =
                Vec::with_capacity(rows.len() + combo_rows.len());
            for m in &rows {
                let mut entry = crate::handlers::models::build_model_entry(m);
                let price = pricing_exact
                    .get(&(m.provider_id.to_string(), m.model_id.to_string()))
                    .or_else(|| pricing_normalized.get(&m.model_id.to_string()));
                if let Some((inp, out, cached)) = price {
                    entry["pricing_input_per_1m"] = serde_json::json!(inp);
                    entry["pricing_output_per_1m"] = serde_json::json!(out);
                    if let Some(c) = cached {
                        entry["pricing_cached_per_1m"] = serde_json::json!(c);
                    }
                }
                data.push(entry);
            }
            for c in &combo_rows {
                let effective_cw = s
                    .services()
                    .combos
                    .compute_effective_context_window(c.id)
                    .ok()
                    .flatten()
                    .or(c.context_window);
                let effective_mot = s
                    .services()
                    .combos
                    .compute_effective_max_output_tokens(c.id)
                    .ok()
                    .flatten()
                    .or(c.max_output_tokens);
                let effective_caps = s
                    .services()
                    .combos
                    .compute_effective_capabilities(c.id)
                    .ok()
                    .flatten();
                // One entry per combo, under its bare routing name. The
                // `combo:<name>` virtual id stays accepted by routing but is no
                // longer advertised: listing both doubles every combo in the UI.
                let mut entry = crate::handlers::models::build_combo_entry(
                    c,
                    Some(&c.name),
                    effective_cw,
                    effective_mot,
                    effective_caps.as_ref(),
                );
                // A combo with its own price is billed at that rate, so the
                // catalog shows it instead of leaving the row unpriced.
                if let (Some(input), Some(output)) =
                    (c.pricing_input_per_1m, c.pricing_output_per_1m)
                {
                    entry["pricing_input_per_1m"] = serde_json::json!(input);
                    entry["pricing_output_per_1m"] = serde_json::json!(output);
                    if let Some(cached) = c.pricing_cached_per_1m {
                        entry["pricing_cached_per_1m"] = serde_json::json!(cached);
                    }
                }
                // A time-of-day combo has no single rate, so publishing the flat
                // columns alone would advertise a tariff that changes at the next
                // window edge. Publish the calendar, the band covering *this*
                // instant, and the instant it was resolved at — the same band
                // `record_row_at` would bill a request arriving right now.
                if let Some(schedule) = c.pricing_schedule.as_ref() {
                    let as_of = chrono::Utc::now();
                    let rate = schedule.rate_at(as_of);
                    entry["pricing_schedule"] = serde_json::json!(schedule);
                    entry["pricing_input_per_1m"] = serde_json::json!(rate.input_per_1m);
                    entry["pricing_output_per_1m"] = serde_json::json!(rate.output_per_1m);
                    entry["pricing_cached_per_1m"] = match rate.cached_per_1m {
                        Some(cached) => serde_json::json!(cached),
                        // `null` means "cache reads bill at the input rate".
                        None => serde_json::Value::Null,
                    };
                    entry["pricing_as_of"] = serde_json::json!(as_of.timestamp());
                }
                data.push(entry);
            }
            let count = data.len();
            Ok(Json(serde_json::json!({ "count": count, "data": data })))
        })
        .await
}

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/", axum::routing::get(list_models_admin))
        .route("/custom", axum::routing::post(create_custom_model))
        .route("/bulk-toggle", axum::routing::post(bulk_toggle_models))
        .route("/sync-models-dev", axum::routing::post(sync_models_dev))
        .route("/{id}/refresh", axum::routing::post(refresh_models))
        .route("/{id}/toggle", axum::routing::post(toggle_model))
        .route(
            "/{id}/test",
            axum::routing::post(test_model).route_layer(axum::middleware::from_fn(
                crate::disconnect::client_disconnect_middleware,
            )),
        )
        .route(
            "/{id}",
            axum::routing::delete(delete_model).patch(update_model),
        )
        .route("/catalog", axum::routing::get(catalog))
}

pub async fn toggle_model(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let active = body
        .get("active")
        .and_then(serde_json::Value::as_bool)
        .ok_or_else(|| CoreError::Validation("missing 'active' bool".into()))?;
    let pool = std::sync::Arc::clone(s.db_pool());
    tokio::task::spawn_blocking(move || -> Result<Json<serde_json::Value>, ApiError> {
        let w = pool
            .try_writer_for(std::time::Duration::from_secs(5))
            .ok_or_else(|| ApiError(CoreError::Internal("writer lock timeout".into())))?;
        core_models::set_active(&w, ModelRowId(id), active)?;
        Ok(Json(serde_json::json!({ "id": id, "active": active })))
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))?
}

pub async fn bulk_toggle_models(
    State(s): State<AppState>,
    Json(body): Json<core_admin::BulkToggleInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = std::sync::Arc::clone(s.db_pool());
    tokio::task::spawn_blocking(move || -> Result<Json<serde_json::Value>, ApiError> {
        let w = pool
            .try_writer_for(std::time::Duration::from_secs(5))
            .ok_or_else(|| ApiError(CoreError::Internal("writer lock timeout".into())))?;
        let updated = core_admin::set_active_bulk(&w, body)?;
        Ok(Json(serde_json::json!({
            "updated": updated,
        })))
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))?
}

crate::admin_entity_action_handler! {
    pub async fn delete_model(
        State(s) with writer(w),
        Path(id): Path<i64>,
    ) -> Result<Json<serde_json::Value>, ApiError> {
        let removed = core_models::delete(&w, ModelRowId(id))?;
        Ok(Json(serde_json::json!({ "id": id, "deleted": removed })))
    }
}

pub async fn update_model(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(input): Json<core_admin::UpdateModelInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = std::sync::Arc::clone(s.db_pool());
    tokio::task::spawn_blocking(move || -> Result<Json<serde_json::Value>, ApiError> {
        let w = pool
            .try_writer_for(std::time::Duration::from_secs(5))
            .ok_or_else(|| ApiError(CoreError::Internal("writer lock timeout".into())))?;
        core_admin::update_model(&w, ModelRowId(id), input)?;
        Ok(Json(serde_json::json!({ "id": id, "updated": true })))
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))?
}

pub async fn create_custom_model(
    State(s): State<AppState>,
    Json(input): Json<core_admin::CreateCustomModelInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let pool = std::sync::Arc::clone(s.db_pool());
    tokio::task::spawn_blocking(move || -> Result<Json<serde_json::Value>, ApiError> {
        let w = pool
            .try_writer_for(std::time::Duration::from_secs(5))
            .ok_or_else(|| ApiError(CoreError::Internal("writer lock timeout".into())))?;
        let row_id = core_admin::create_custom_model(&w, input)?;
        Ok(Json(serde_json::json!({ "row_id": row_id.0 })))
    })
    .await
    .map_err(|e| ApiError(CoreError::Internal(format!("spawn failed: {e}"))))?
}

pub async fn list_models_admin(
    State(s): State<AppState>,
    axum::extract::Query(q): axum::extract::Query<ListModelsQuery>,
) -> Result<Json<Vec<core_models::Model>>, ApiError> {
    // Read-only SELECT — use the READER.
    let mut list = s.db_pool().spawn_read(core_models::list_all).await?;
    if let Some(p) = q.provider_id {
        list.retain(|m| m.provider_id.as_str() == p);
    }
    for m in &mut list {
        let inferred_type = openproxy_types::capabilities::infer_model_type(m.model_id.as_str());
        let effective_type = openproxy_types::capabilities::resolve_effective_model_type(
            &m.model_type,
            m.custom,
            inferred_type,
        );
        if m.model_type.as_ref() != effective_type {
            m.model_type = effective_type.to_string().into_boxed_str();
        }
    }
    Ok(Json(list))
}

pub async fn sync_models_dev(
    State(s): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let upstream = Arc::clone(s.upstream_client());
    let db_pool = Arc::clone(s.db_pool());
    let result = openproxy_core::models_dev_sync::run_one_shot(db_pool, upstream).await;
    let msg = match result {
        Ok(m) => m,
        Err(e) => return Err(ApiError(e)),
    };
    Ok(Json(serde_json::json!({ "message": msg })))
}

/// Query string for `POST /admin/models/:id/refresh` — lets the caller
/// override the refresh TTL in seconds and pin a specific account.
#[derive(Debug, Default, Deserialize)]
pub struct RefreshQuery {
    /// Cache TTL in seconds for the discovered rows. Defaults to 1 hour.
    pub ttl_seconds: Option<i64>,
    /// Account id whose API key will be used. Required when the provider
    /// has more than one account; otherwise the first account wins. The
    /// API key is decrypted on the fly and is never logged or echoed.
    pub account_id: Option<i64>,
}

/// `GET /admin/models` — every row in the `models` table.
#[derive(Debug, Default, Deserialize)]
pub struct ListModelsQuery {
    pub provider_id: Option<String>,
}

pub async fn refresh_models(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<RefreshQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    run_refresh(s, id, q).await
}

pub(crate) async fn run_refresh(
    s: AppState,
    id: i64,
    q: RefreshQuery,
) -> Result<Json<serde_json::Value>, ApiError> {
    let row_id = ModelRowId(id);
    let provider_id = {
        let found = s
            .db_pool()
            .spawn_read(move |r| core_models::get_by_row_id(r, row_id))
            .await?;
        match found {
            Some(m) => m.provider_id,
            None => {
                return Err(ApiError(CoreError::model_not_found(
                    "<unknown>",
                    format!("row_id={}", row_id.0),
                )));
            }
        }
    };

    let provider_q = super::providers::ProviderRefreshQuery {
        ttl_seconds: q.ttl_seconds,
        account_id: q.account_id,
    };
    super::providers::run_provider_refresh(s, provider_id.as_str(), provider_q).await
}

pub(crate) async fn resolve_adapter(
    s: &AppState,
    provider_id: &ProviderId,
    builtin: &[adapters::ProviderAdapterEnum],
) -> Result<adapters::ProviderAdapterEnum, CoreError> {
    // 1. Built-in adapter?
    if let Some(a) = builtin.iter().find(|a| a.id() == provider_id) {
        return Ok(adapters::ProviderAdapterEnum::clone(a));
    }
    // 2. Custom provider in DB → build adapter on-the-fly.
    // `core_providers::get` is a SELECT — use the READER so this lookup
    // doesn't serialize through the writer mutex (chat hot path).
    let pid = provider_id.clone();
    let provider_row = s
        .db_pool()
        .spawn_read(move |r| core_providers::get(r, &pid))
        .await
        .map_err(|e| CoreError::ProviderNotFound(format!("{provider_id}: {e}")))?;
    match provider_row {
        Some(row) => Ok(adapters::ProviderAdapterEnum::Custom(Box::new(
            adapters::CustomAdapter::from_provider_row(&row),
        ))),
        None => Err(CoreError::ProviderNotFound(provider_id.to_string())),
    }
}
