use super::{ApiError, CoreError, Deserialize};
use crate::extractors::{DbReader, DbWriter};
use axum::{
    Json,
    extract::{Path, Query},
};

/// Query string for `GET /admin/api/notifications`.
#[derive(Debug, Default, Deserialize)]
pub struct NotificationsQuery {
    pub unread: Option<bool>,
    pub limit: Option<i64>,
    pub before_id: Option<i64>,
}

pub async fn list_notifications(
    DbReader(r): DbReader,
    Query(q): Query<NotificationsQuery>,
) -> Result<Json<Vec<openproxy_core::notifications::NotificationRow>>, ApiError> {
    let unread_only = q.unread.unwrap_or(false);
    let limit = q.limit.unwrap_or(50);
    let rows = openproxy_core::notifications::list(&r, unread_only, limit, q.before_id)
        .map_err(|e| CoreError::Internal(format!("core_notifications::list: {e}")))?;
    Ok(Json(rows))
}

pub async fn notifications_unread_count(
    DbReader(r): DbReader,
) -> Result<Json<serde_json::Value>, ApiError> {
    let count = openproxy_core::notifications::unread_count(&r)
        .map_err(|e| CoreError::Internal(format!("core_notifications::unread_count: {e}")))?;
    Ok(Json(serde_json::json!({ "count": count })))
}

macro_rules! notif_action_handler {
    (by_id: $fn_name:ident, $core_fn:ident) => {
        pub async fn $fn_name(
            DbWriter(w): DbWriter,
            Path(id): Path<i64>,
        ) -> Result<Json<serde_json::Value>, ApiError> {
            openproxy_core::notifications::$core_fn(&w, id)
                .map_err(|e| CoreError::Internal(format!("core_notifications::{}: {e}", stringify!($core_fn))))?;
            Ok(Json(serde_json::json!({ "ok": true })))
        }
    };
    (all: $fn_name:ident, $core_fn:ident) => {
        pub async fn $fn_name(
            DbWriter(w): DbWriter,
        ) -> Result<Json<serde_json::Value>, ApiError> {
            let updated = openproxy_core::notifications::$core_fn(&w)
                .map_err(|e| CoreError::Internal(format!("core_notifications::{}: {e}", stringify!($core_fn))))?;
            Ok(Json(serde_json::json!({ "updated": updated })))
        }
    };
}

notif_action_handler!(by_id: mark_notification_read, mark_read);
notif_action_handler!(all: mark_all_notifications_read, mark_all_read);
notif_action_handler!(all: archive_all_notifications, archive_all);
notif_action_handler!(by_id: archive_notification, archive);

pub async fn delete_notification(
    DbWriter(w): DbWriter,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let deleted = openproxy_core::notifications::delete(&w, id)
        .map_err(|e| CoreError::Internal(format!("core_notifications::delete: {e}")))?;
    if deleted {
        Ok(Json(serde_json::json!({ "ok": true })))
    } else {
        Err(ApiError(CoreError::Validation(
            "notification not deletable (kind=model_* within 30-day audit window, or row does not exist)".into(),
        )))
    }
}
