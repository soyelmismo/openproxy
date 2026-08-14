use super::*;
use crate::extractors::{DbReader, DbWriter};
use axum::{
    Json,
    extract::{Path, Query, State},
};
use openproxy_adapters::upstream::is_private_or_reserved;

pub async fn list_proxies(
    DbReader(r): DbReader,
    Query(query): Query<ListProxiesQuery>,
) -> Result<Json<Vec<openproxy_core::free_proxies::FreeProxy>>, ApiError> {
    let list = openproxy_core::free_proxies::list_proxies(
        &r,
        query.source.as_deref(),
        query.status.as_deref(),
        query.protocol.as_deref(),
        query.search.as_deref(),
        query.limit,
        query.offset,
    )?;
    Ok(Json(list))
}

pub async fn get_proxy_summary(
    DbReader(r): DbReader,
) -> Result<Json<openproxy_core::free_proxies::ProxySummary>, ApiError> {
    let summary = openproxy_core::free_proxies::get_proxy_summary(&r)?;
    Ok(Json(summary))
}

pub async fn sync_proxies(
    State(s): State<AppState>,
) -> Result<Json<openproxy_core::free_proxies::SyncSummary>, ApiError> {
    let summary = openproxy_core::free_proxies::sync_all_providers(Arc::clone(s.db_pool())).await?;
    Ok(Json(summary))
}

pub async fn create_custom_proxy(
    DbWriter(w): DbWriter,
    Json(body): Json<CreateCustomProxyInput>,
) -> Result<Json<openproxy_core::free_proxies::FreeProxy>, ApiError> {
    if body.host.trim().is_empty() || body.port == 0 {
        return Err(ApiError(CoreError::Validation(
            "host and port are required".into(),
        )));
    }
    let host_str = body.host.trim();
    if let Ok(ip) = host_str.parse::<std::net::IpAddr>()
        && is_private_or_reserved(&ip)
    {
        return Err(ApiError(CoreError::Validation(format!(
            "host '{host_str}' resolves to a private/reserved IP and is not allowed"
        ))));
    }
    let p = openproxy_core::free_proxies::add_custom_proxy(
        &w,
        body.host.trim(),
        body.port,
        body.r#type.trim(),
        body.country_code.as_deref().map(str::trim),
        body.username.as_deref().map(str::trim),
        body.password.as_deref().map(str::trim),
    )?;
    Ok(Json(p))
}

pub async fn test_proxy(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<openproxy_core::free_proxies::FreeProxy>, ApiError> {
    let p = openproxy_core::free_proxies::test_single_proxy(Arc::clone(s.db_pool()), &id).await?;
    Ok(Json(p))
}

pub async fn test_all_proxies(
    State(s): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    openproxy_core::free_proxies::test_all_proxies_background(Arc::clone(s.db_pool()));
    Ok(Json(serde_json::json!({ "status": "started" })))
}

pub async fn delete_proxy(
    DbWriter(w): DbWriter,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    openproxy_core::free_proxies::delete_proxy(&w, &id)?;
    Ok(Json(serde_json::json!({ "status": "deleted" })))
}

pub async fn get_proxy_test_url(
    DbReader(r): DbReader,
) -> Result<Json<serde_json::Value>, ApiError> {
    let url = openproxy_db::app_config::load_proxy_test_url(&r)?;
    Ok(Json(serde_json::json!({ "proxy_test_url": url })))
}

#[derive(serde::Deserialize)]
pub struct UpdateProxyTestUrlInput {
    pub proxy_test_url: String,
}

pub async fn update_proxy_test_url(
    DbWriter(w): DbWriter,
    Json(body): Json<UpdateProxyTestUrlInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let url = body.proxy_test_url.trim();
    if url.is_empty() {
        return Err(ApiError(CoreError::Validation(
            "url cannot be empty".into(),
        )));
    }
    openproxy_db::app_config::save_proxy_test_url(&w, url)?;
    Ok(Json(serde_json::json!({ "proxy_test_url": url })))
}
