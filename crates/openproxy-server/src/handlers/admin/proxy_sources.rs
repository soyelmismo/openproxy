use super::{ApiError, Arc, CoreError};
use crate::extractors::{DbReader, DbWriter};
use axum::{Json, extract::Path};
use openproxy_core::free_proxies::{
    CreateProxySourceInput, ProxySource, UpdateProxySourceInput, create_proxy_source,
    delete_proxy_source, get_proxy_source, list_proxy_sources, test_proxy_source_url,
    update_proxy_source,
};

pub fn router() -> axum::Router<crate::state::AppState> {
    axum::Router::new()
        .route("/", axum::routing::get(list_sources).post(create_source))
        .route("/test", axum::routing::post(test_source_url))
        .route("/reorder", axum::routing::post(reorder_proxy_sources))
        .route("/{id}/test", axum::routing::post(test_source_by_id))
        .route(
            "/{id}",
            axum::routing::put(update_source).delete(delete_source),
        )
}

pub async fn list_sources(DbReader(r): DbReader) -> Result<Json<Vec<ProxySource>>, ApiError> {
    let list = list_proxy_sources(&r)?;
    Ok(Json(list))
}

fn validate_create_source_input(body: &CreateProxySourceInput) -> Result<(), ApiError> {
    if body.name.trim().is_empty() || body.url.trim().is_empty() {
        return Err(ApiError(CoreError::Validation(
            "name and url are required".into(),
        )));
    }
    Ok(())
}

fn spawn_source_sync_and_test(pool: Arc<openproxy_db::DbPool>) {
    tokio::spawn(async move {
        let Ok(summary) = openproxy_core::free_proxies::sync_all_providers(Arc::clone(&pool)).await
        else {
            return;
        };
        if summary.added > 0 || summary.fetched > 0 {
            openproxy_core::free_proxies::test_all_proxies_background(pool);
        }
    });
}

pub async fn create_source(
    axum::extract::State(state): axum::extract::State<crate::state::AppState>,
    DbWriter(w): DbWriter,
    Json(body): Json<CreateProxySourceInput>,
) -> Result<Json<ProxySource>, ApiError> {
    validate_create_source_input(&body)?;
    let src = create_proxy_source(&w, &body)?;
    spawn_source_sync_and_test(Arc::clone(state.db_pool()));
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

fn validate_source_deletion(conn: &rusqlite::Connection, id: &str) -> Result<(), ApiError> {
    let Some(source) = get_proxy_source(conn, id)? else {
        return Err(ApiError(CoreError::Validation(format!(
            "proxy source '{id}' not found"
        ))));
    };

    if source.is_builtin {
        return Err(ApiError(CoreError::Validation(
            "Cannot delete built-in proxy sources".into(),
        )));
    }
    Ok(())
}

pub async fn delete_source(
    DbWriter(w): DbWriter,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    validate_source_deletion(&w, &id)?;
    if !delete_proxy_source(&w, &id)? {
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

fn fetch_source_url_by_id(r: &rusqlite::Connection, id: &str) -> Result<String, ApiError> {
    let src = get_proxy_source(r, id)?
        .ok_or_else(|| CoreError::Validation(format!("proxy source '{id}' not found")))?;
    Ok(src.url)
}

pub async fn test_source_by_id(
    DbReader(r): DbReader,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let url = fetch_source_url_by_id(&r, &id)?;
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
    DbWriter(w): DbWriter,
    Json(body): Json<ReorderProxySourcesInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    openproxy_core::free_proxies::reorder_proxy_sources(&w, &body.ids)?;
    Ok(Json(serde_json::json!({ "reordered": true })))
}
