use super::*;
use crate::extractors::{DbReader, DbWriter};
use axum::{Json, extract::Path};
use openproxy_core::free_proxies::{
    CreateProxySourceInput, ProxySource, UpdateProxySourceInput, create_proxy_source,
    delete_proxy_source, get_proxy_source, list_proxy_sources, test_proxy_source_url,
    update_proxy_source,
};

pub async fn list_sources(DbReader(r): DbReader) -> Result<Json<Vec<ProxySource>>, ApiError> {
    let list = list_proxy_sources(&r)?;
    Ok(Json(list))
}

pub async fn create_source(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    DbWriter(w): DbWriter,
    Json(body): Json<CreateProxySourceInput>,
) -> Result<Json<ProxySource>, ApiError> {
    if body.name.trim().is_empty() || body.url.trim().is_empty() {
        return Err(ApiError(CoreError::Validation(
            "name and url are required".into(),
        )));
    }
    let src = create_proxy_source(&w, body)?;

    let pool = state.db_pool().clone();
    tokio::spawn(async move {
        if let Ok(summary) = openproxy_core::free_proxies::sync_all_providers(pool.clone()).await
            && (summary.added > 0 || summary.fetched > 0) {
                openproxy_core::free_proxies::test_all_proxies_background(pool);
            }
    });

    Ok(Json(src))
}

pub async fn update_source(
    DbWriter(w): DbWriter,
    Path(id): Path<String>,
    Json(body): Json<UpdateProxySourceInput>,
) -> Result<Json<ProxySource>, ApiError> {
    let src = update_proxy_source(&w, &id, body)?;
    Ok(Json(src))
}

pub async fn delete_source(
    DbWriter(w): DbWriter,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let source = get_proxy_source(&w, &id)?.ok_or_else(|| {
        ApiError(CoreError::Validation(format!(
            "proxy source '{id}' not found"
        )))
    })?;

    if source.is_builtin {
        return Err(ApiError(CoreError::Validation(
            "Cannot delete built-in proxy sources".into(),
        )));
    }

    let deleted = delete_proxy_source(&w, &id)?;
    if !deleted {
        return Err(ApiError(CoreError::Validation(format!(
            "proxy source '{id}' not found"
        ))));
    }
    Ok(Json(serde_json::json!({ "id": id, "deleted": true })))
}

#[derive(serde::Deserialize)]
pub struct TestSourceInput {
    pub url: Option<String>,
}

pub async fn test_source_by_id(
    db_reader: DbReader,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let url = {
        let DbReader(r) = db_reader;
        let src = get_proxy_source(&r, &id)?
            .ok_or_else(|| CoreError::Validation(format!("proxy source '{id}' not found")))?;
        src.url
    };
    let count = test_proxy_source_url(&url).await?;
    Ok(Json(serde_json::json!({
        "id": id,
        "url": url,
        "proxy_count": count
    })))
}

pub async fn test_source_url(
    Json(body): Json<TestSourceInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let url = body.url.as_deref().unwrap_or("").trim();
    if url.is_empty() {
        return Err(ApiError(CoreError::Validation("url is required".into())));
    }
    let count = test_proxy_source_url(url).await?;
    Ok(Json(serde_json::json!({
        "url": url,
        "proxy_count": count
    })))
}

#[derive(serde::Deserialize)]
pub struct ReorderProxySourcesInput {
    pub ids: Vec<String>,
}

pub async fn reorder_proxy_sources(
    DbWriter(mut w): DbWriter,
    Json(body): Json<ReorderProxySourcesInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let tx = w.transaction().map_err(|e| CoreError::Database {
        message: e.to_string(),
        source: Some(Box::new(e)),
    })?;

    let n = body.ids.len();
    for (i, id) in body.ids.iter().enumerate() {
        let p = ((n - i) * 10) as i32;
        tx.execute(
            "UPDATE proxy_sources SET priority = ?1 WHERE id = ?2",
            rusqlite::params![p, id],
        )
        .map_err(|e| CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })?;
    }
    tx.commit().map_err(|e| CoreError::Database {
        message: e.to_string(),
        source: Some(Box::new(e)),
    })?;

    Ok(Json(serde_json::json!({ "reordered": true })))
}
