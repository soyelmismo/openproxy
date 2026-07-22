use super::*;
use axum::{
    Json,
    extract::{Path, Query, State},
};
use openproxy_adapters::upstream::is_private_or_reserved;

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
            query.protocol.as_deref(),
            query.search.as_deref(),
            query.limit,
            query.offset,
        )?;
        Ok(Json(list))
    }
}

pub async fn get_proxy_summary(
    State(s): State<AppState>,
) -> ApiResult<Json<openproxy_core::free_proxies::ProxySummary>> {
    crate::api_try! {
        let r = s.db_pool().reader();
        let summary = openproxy_core::free_proxies::get_proxy_summary(&r)?;
        Ok(Json(summary))
    }
}

pub async fn sync_proxies(
    State(s): State<AppState>,
) -> ApiResult<Json<openproxy_core::free_proxies::SyncSummary>> {
    let res: Result<Json<openproxy_core::free_proxies::SyncSummary>, crate::error::ApiError> = async {
        let summary = openproxy_core::free_proxies::sync_all_providers(s.db_pool().clone()).await?;
        Ok(Json(summary))
    }.await;
    res.into()
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
        // Reject literal private/reserved IP addresses (SSRF protection).
        // Hostnames are allowed — only reject IPs that parse to private ranges.
        let host_str = body.host.trim();
        if let Ok(ip) = host_str.parse::<std::net::IpAddr>()
            && is_private_or_reserved(&ip)
        {
            return Err(ApiError(CoreError::Validation(format!(
                "host '{host_str}' resolves to a private/reserved IP and is not allowed"
            ))));
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
    let res: Result<Json<openproxy_core::free_proxies::FreeProxy>, crate::error::ApiError> = async {
        let p = openproxy_core::free_proxies::test_single_proxy(s.db_pool().clone(), &id).await?;
        Ok(Json(p))
    }.await;
    res.into()
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
