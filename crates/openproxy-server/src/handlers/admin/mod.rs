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

pub(crate) use crate::{error::ApiError, state::AppState};
