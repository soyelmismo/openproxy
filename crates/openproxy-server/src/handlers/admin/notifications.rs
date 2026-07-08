use super::*;
use axum::{
    Json,
    extract::{Path, Query, State},
};

pub async fn list_notifications(
    State(s): State<AppState>,
    Query(q): Query<NotificationsQuery>,
) -> ApiResult<Json<Vec<openproxy_core::notifications::NotificationRow>>> {
    let body: Result<Json<Vec<openproxy_core::notifications::NotificationRow>>, ApiError> = async {
        let unread_only = q.unread.unwrap_or(false);
        let limit = q.limit.unwrap_or(50);
        // Read-only SELECT — use the READER so the dashboard's poll
        // doesn't serialize through the writer mutex.
        let r = s.db_pool().reader();
        let rows = openproxy_core::notifications::list(&r, unread_only, limit, q.before_id)
            .map_err(|e| CoreError::Internal(format!("core_notifications::list: {}", e)))?;
        Ok(Json(rows))
    }
    .await;
    body.into()
}

pub async fn notifications_unread_count(
    State(s): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let r = s.db_pool().reader();
        let count = openproxy_core::notifications::unread_count(&r)
            .map_err(|e| CoreError::Internal(format!("core_notifications::unread_count: {}", e)))?;
        Ok(Json(serde_json::json!({ "count": count })))
    }
    .await;
    body.into()
}

pub async fn mark_notification_read(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        openproxy_core::notifications::mark_read(&w, id)
            .map_err(|e| CoreError::Internal(format!("core_notifications::mark_read: {}", e)))?;
        Ok(Json(serde_json::json!({ "ok": true })))
    }
    .await;
    body.into()
}

pub async fn mark_all_notifications_read(
    State(s): State<AppState>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let updated = openproxy_core::notifications::mark_all_read(&w).map_err(|e| {
            CoreError::Internal(format!("core_notifications::mark_all_read: {}", e))
        })?;
        Ok(Json(serde_json::json!({ "updated": updated })))
    }
    .await;
    body.into()
}

pub async fn archive_notification(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        openproxy_core::notifications::archive(&w, id)
            .map_err(|e| CoreError::Internal(format!("core_notifications::archive: {}", e)))?;
        Ok(Json(serde_json::json!({ "ok": true })))
    }
    .await;
    body.into()
}

pub async fn delete_notification(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    let body: Result<Json<serde_json::Value>, ApiError> = async {
        let w = s.db_pool().writer();
        let deleted = openproxy_core::notifications::delete(&w, id)
            .map_err(|e| {
                CoreError::Internal(format!("core_notifications::delete: {}", e))
            })?;
        if deleted {
            Ok(Json(serde_json::json!({ "ok": true })))
        } else {
            // Map "not eligible" to HTTP 400 (Validation) so the
            // client can distinguish "delete refused" from "delete
            // succeeded".
            Err(ApiError(CoreError::Validation(
                "notification not deletable (kind=model_* within 30-day audit window, or row does not exist)".into(),
            )))
        }
    }
    .await;
    body.into()
}
