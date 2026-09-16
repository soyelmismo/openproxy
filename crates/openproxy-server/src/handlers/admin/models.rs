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
    let r = s.db_pool().reader();
    let mut list = core_models::list_all(&r)?;
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
        let r = s.db_pool().reader();
        let found = core_models::get_by_row_id(&r, row_id)?;
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

pub(crate) fn resolve_adapter(
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
    let r = s.db_pool().reader();
    let provider_row = core_providers::get(&r, provider_id)
        .map_err(|e| CoreError::ProviderNotFound(format!("{provider_id}: {e}")))?;
    drop(r);
    match provider_row {
        Some(row) => Ok(adapters::ProviderAdapterEnum::Custom(Box::new(
            adapters::CustomAdapter::from_provider_row(&row),
        ))),
        None => Err(CoreError::ProviderNotFound(provider_id.to_string())),
    }
}
