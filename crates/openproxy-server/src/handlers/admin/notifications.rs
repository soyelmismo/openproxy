use super::*;
use axum::{
    Json,
    extract::{Path, Query},
};
use crate::extractors::{DbReader, DbWriter};

pub async fn list_notifications(
    DbReader(r): DbReader,
    Query(q): Query<NotificationsQuery>,
) -> Result<Json<Vec<openproxy_core::notifications::NotificationRow>>, ApiError> {
    let unread_only = q.unread.unwrap_or(false);
    let limit = q.limit.unwrap_or(50);
    let rows = openproxy_core::notifications::list(&r, unread_only, limit, q.before_id)
        .map_err(|e| CoreError::Internal(format!("core_notifications::list: {}", e)))?;
    Ok(Json(rows))
}

pub async fn notifications_unread_count(
    DbReader(r): DbReader,
) -> Result<Json<serde_json::Value>, ApiError> {
    let count = openproxy_core::notifications::unread_count(&r)
        .map_err(|e| CoreError::Internal(format!("core_notifications::unread_count: {}", e)))?;
    Ok(Json(serde_json::json!({ "count": count })))
}

pub async fn mark_notification_read(
    DbWriter(w): DbWriter,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    openproxy_core::notifications::mark_read(&w, id)
        .map_err(|e| CoreError::Internal(format!("core_notifications::mark_read: {}", e)))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn mark_all_notifications_read(
    DbWriter(w): DbWriter,
) -> Result<Json<serde_json::Value>, ApiError> {
    let updated = openproxy_core::notifications::mark_all_read(&w).map_err(|e| {
        CoreError::Internal(format!("core_notifications::mark_all_read: {}", e))
    })?;
    Ok(Json(serde_json::json!({ "updated": updated })))
}

pub async fn archive_notification(
    DbWriter(w): DbWriter,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    openproxy_core::notifications::archive(&w, id)
        .map_err(|e| CoreError::Internal(format!("core_notifications::archive: {}", e)))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

pub async fn delete_notification(
    DbWriter(w): DbWriter,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let deleted = openproxy_core::notifications::delete(&w, id)
        .map_err(|e| {
            CoreError::Internal(format!("core_notifications::delete: {}", e))
        })?;
    if deleted {
        Ok(Json(serde_json::json!({ "ok": true })))
    } else {
        Err(ApiError(CoreError::Validation(
            "notification not deletable (kind=model_* within 30-day audit window, or row does not exist)".into(),
        )))
    }
}
