use super::*;
use axum::{
    Json,
    extract::{Path, Query, State},
};

pub async fn list_proxies(
    State(s): State<AppState>,
    Query(query): Query<ListProxiesQuery>,
) -> ApiResult<Json<Vec<openproxy_core::free_proxies::FreeProxy>>> {
    crate::api_try! {
        let r = s.db_pool().reader();
        let list = openproxy_core::free_proxies::list_proxies(
            &r,
            query.source.as_deref(),
            query.status.as_deref(),
        )?;
        Ok(Json(list))
    }
}

pub async fn sync_proxies(
    State(s): State<AppState>,
) -> ApiResult<Json<openproxy_core::free_proxies::SyncSummary>> {
    crate::api_try! {
        let summary = openproxy_core::free_proxies::sync_all_providers(s.db_pool().clone()).await?;
        Ok(Json(summary))
    }
}

pub async fn create_custom_proxy(
    State(s): State<AppState>,
    Json(body): Json<CreateCustomProxyInput>,
) -> ApiResult<Json<openproxy_core::free_proxies::FreeProxy>> {
    crate::api_try! {
        if body.host.trim().is_empty() || body.port == 0 {
            return Err(ApiError(CoreError::Validation(
                "host and port are required".into(),
            )));
        }
        let w = s.db_pool().writer();
        let p = openproxy_core::free_proxies::add_custom_proxy(
            &w,
            body.host.trim().to_string(),
            body.port,
            body.r#type.trim().to_string(),
            body.country_code.map(|c| c.trim().to_string()),
        )?;
        Ok(Json(p))
    }
}

pub async fn test_proxy(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<openproxy_core::free_proxies::FreeProxy>> {
    crate::api_try! {
        let p = openproxy_core::free_proxies::test_single_proxy(s.db_pool().clone(), &id).await?;
        Ok(Json(p))
    }
}

pub async fn test_all_proxies(State(s): State<AppState>) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        openproxy_core::free_proxies::test_all_proxies_background(s.db_pool().clone());
        Ok(Json(serde_json::json!({ "status": "started" })))
    }
}

pub async fn delete_proxy(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let w = s.db_pool().writer();
        openproxy_core::free_proxies::delete_proxy(&w, &id)?;
        Ok(Json(serde_json::json!({ "status": "deleted" })))
    }
}
