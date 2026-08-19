//! Admin HTTP handlers and shared submodules.

pub mod accounts;
pub mod api_keys;
pub mod auth;
pub mod combos;
pub mod debug;
pub mod models;
pub mod notifications;
pub mod oauth;
pub mod providers;
pub mod proxies;
pub mod proxy_sources;
pub mod runtime;
pub mod usage;

#[cfg(test)]
pub mod tests;

pub(crate) use auth::{admin_auth_middleware, authenticate_admin_ws};
pub(crate) use models::{TestOptions, resolve_adapter, run_test_for_model};
pub(crate) use oauth::refresh_oauth_if_needed;
pub(crate) use openproxy_db::combos as core_combos;
pub(crate) use openproxy_types::combos as types_combos;

/// Assemble all modular admin sub-routers into the unified admin REST router.
pub fn admin_api_routes() -> axum::Router<AppState> {
    axum::Router::new()
        .nest("/config", runtime::router())
        .nest("/providers", providers::router())
        .nest("/accounts", accounts::router())
        .nest("/combos", combos::router())
        .nest("/usage", usage::router())
        .nest("/debug", debug::router())
        .route(
            "/recording",
            axum::routing::get(debug::get_recording).post(debug::set_recording),
        )
        .nest("/models", models::router())
        .nest("/keys", api_keys::router())
        .nest("/proxies", proxies::router())
        .nest("/proxy-sources", proxy_sources::router())
        .nest("/notifications", notifications::router())
        .nest("/oauth", oauth::router())
        .fallback(|| async {
            (
                axum::http::StatusCode::NOT_FOUND,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"error":{"code":"not_found","message":"endpoint not found"}}"#,
            )
        })
}

pub use accounts::AccountListQuery;
pub use combos::ReorderComboTargetsInput;
pub use debug::{DebugLogsQuery, DebugLogsResponse};
pub use models::{ListModelsQuery, RefreshQuery, TestModelInput};
pub use notifications::NotificationsQuery;
pub use providers::{PROVIDER_REFRESH_DEFAULT_TTL_SECS, ProviderRefreshQuery, ProviderWithOAuth};
pub use proxies::{CreateCustomProxyInput, ListProxiesQuery};
pub use runtime::RuntimeConfigResponse;
pub use usage::{
    ClientWsMessage, DetailQuery, ERRORS_DEFAULT_LIMIT, NotifRxEvent, RecentQuery,
    USAGE_RECENT_DEFAULT_LIMIT, USAGE_RECENT_MAX_LIMIT, USAGE_RECENT_MAX_SINCE_ID,
    UsageDetailResponse, UsageStreamQuery, WS_OUTBOX_CAPACITY,
};

pub(crate) use crate::{error::ApiError, state::AppState};
pub(crate) use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
pub(crate) use futures::StreamExt;
pub(crate) use openproxy_adapters::adapters;
pub(crate) use openproxy_core::{
    accounts as core_accounts, analytics, api_keys as core_api_keys,
    config::{CircuitBreakerConfig, RacingConfig, RetriesConfig, TimeoutsConfig},
    models as core_models, oauth as core_oauth, providers as core_providers, seed,
    usage as core_usage,
    usage::UsageFilter,
};
pub(crate) use openproxy_db as core_db;
pub(crate) use openproxy_db::conn::ADMIN_LOCK_TIMEOUT;
pub(crate) use openproxy_types::{
    CoreError,
    ids::{
        AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId, TraceId,
    },
};
pub(crate) use serde::{Deserialize, Serialize};
pub(crate) use serde_json::json;
pub(crate) use std::sync::Arc;

/// Macro declarativa para estandarizar handlers de acción/eliminación sobre entidades administrativas.
#[macro_export]
macro_rules! admin_entity_action_handler {
    (
        $(#[$meta:meta])*
        pub async fn $fn_name:ident(
            State($state:ident): State<AppState>,
            Path($id:pat): Path<$id_ty:ty> $(,)?
        ) -> Result<Json<serde_json::Value>, ApiError> {
            exec: $action:expr,
            response: $resp:expr $(,)?
        }
    ) => {
        $(#[$meta])*
        pub async fn $fn_name(
            axum::extract::State($state): axum::extract::State<$crate::state::AppState>,
            axum::extract::Path($id): axum::extract::Path<$id_ty>,
        ) -> std::result::Result<axum::Json<serde_json::Value>, $crate::error::ApiError> {
            $action;
            Ok(axum::Json($resp))
        }
    };

    (
        $(#[$meta:meta])*
        pub async fn $fn_name:ident(
            State($state:ident) with writer($w:ident),
            Path($id:pat): Path<$id_ty:ty> $(,)?
        ) -> Result<Json<serde_json::Value>, ApiError> {
            exec: $action:expr,
            response: $resp:expr $(,)?
        }
    ) => {
        $(#[$meta])*
        pub async fn $fn_name(
            axum::extract::State($state): axum::extract::State<$crate::state::AppState>,
            axum::extract::Path($id): axum::extract::Path<$id_ty>,
        ) -> std::result::Result<axum::Json<serde_json::Value>, $crate::error::ApiError> {
            let $w = $state.db_pool().writer();
            $action;
            Ok(axum::Json($resp))
        }
    };

    (
        $(#[$meta:meta])*
        pub async fn $fn_name:ident(
            DbWriter($w:ident): DbWriter,
            Path($id:pat): Path<$id_ty:ty> $(,)?
        ) -> Result<Json<serde_json::Value>, ApiError> {
            exec: $action:expr,
            response: $resp:expr $(,)?
        }
    ) => {
        $(#[$meta])*
        pub async fn $fn_name(
            $crate::extractors::DbWriter($w): $crate::extractors::DbWriter,
            axum::extract::Path($id): axum::extract::Path<$id_ty>,
        ) -> std::result::Result<axum::Json<serde_json::Value>, $crate::error::ApiError> {
            $action;
            Ok(axum::Json($resp))
        }
    };

    (
        $(#[$meta:meta])*
        pub async fn $fn_name:ident(
            State($state:ident): State<AppState>,
            Path($id:pat): Path<$id_ty:ty> $(,)?
        ) -> Result<Json<serde_json::Value>, ApiError> $body:block
    ) => {
        $(#[$meta])*
        pub async fn $fn_name(
            axum::extract::State($state): axum::extract::State<$crate::state::AppState>,
            axum::extract::Path($id): axum::extract::Path<$id_ty>,
        ) -> std::result::Result<axum::Json<serde_json::Value>, $crate::error::ApiError> {
            $body
        }
    };

    (
        $(#[$meta:meta])*
        pub async fn $fn_name:ident(
            State($state:ident) with writer($w:ident),
            Path($id:pat): Path<$id_ty:ty> $(,)?
        ) -> Result<Json<serde_json::Value>, ApiError> $body:block
    ) => {
        $(#[$meta])*
        pub async fn $fn_name(
            axum::extract::State($state): axum::extract::State<$crate::state::AppState>,
            axum::extract::Path($id): axum::extract::Path<$id_ty>,
        ) -> std::result::Result<axum::Json<serde_json::Value>, $crate::error::ApiError> {
            let $w = $state.db_pool().writer();
            $body
        }
    };

    (
        $(#[$meta:meta])*
        pub async fn $fn_name:ident(
            DbWriter($w:ident): DbWriter,
            Path($id:pat): Path<$id_ty:ty> $(,)?
        ) -> Result<Json<serde_json::Value>, ApiError> $body:block
    ) => {
        $(#[$meta])*
        pub async fn $fn_name(
            $crate::extractors::DbWriter($w): $crate::extractors::DbWriter,
            axum::extract::Path($id): axum::extract::Path<$id_ty>,
        ) -> std::result::Result<axum::Json<serde_json::Value>, $crate::error::ApiError> {
            $body
        }
    };
}
