//! Shared unary execution primitives: target resolution, API key retrieval,
//! circuit breaker / cooldown checking, and standard usage logging.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest, UpstreamResponse,
};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_pipeline::circuit_breaker::{CircuitBreakerKey, CircuitBreakerRegistry, Health};
use openproxy_types::{
    CoreError, EndpointKind, ModelId, Result, TargetFormat,
    ids::{AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId},
};

use crate::{
    accounts, models, providers,
    routing::{self, RoutingPlan},
};

mod usage;

pub use usage::{
    UnaryUsageArgs, UnaryUsageArgsBuilder, record_unary_target_success_async, record_unary_usage,
    record_unary_usage_async,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnaryTarget {
    pub provider: ProviderId,
    pub account_id: Option<AccountId>,
    pub model_row_id: Option<ModelRowId>,
    pub combo_target_id: Option<ComboTargetId>,
    pub upstream_model: String,
    pub combo_id: Option<ComboId>,
}

pub type AudioTarget = UnaryTarget;
pub type AudioTargets = UnaryTarget;
pub type ImageTarget = UnaryTarget;
pub type ImageTargets = UnaryTarget;
pub type EmbeddingTarget = UnaryTarget;
pub type EmbeddingTargets = UnaryTarget;

fn is_target_allowed(key: Option<&crate::api_keys::ApiKey>, provider: &str, model: &str) -> bool {
    let Some(key) = key else { return true };
    key.is_provider_allowed(provider) && key.is_model_allowed(model, Some(provider))
}

fn resolve_single_target(
    r: &rusqlite::Connection,
    target: openproxy_types::ComboTarget,
    req_model: &str,
    combo_id: ComboId,
    maybe_key: Option<&crate::api_keys::ApiKey>,
) -> Option<UnaryTarget> {
    let (provider, upstream_model, model_row_id) = if let Some(model_row_id) = target.model_row_id {
        let model = models::get_by_row_id(r, model_row_id).ok().flatten()?;
        (
            model.provider_id,
            model.model_id.as_str().to_string(),
            Some(model_row_id),
        )
    } else {
        (target.provider_id.clone(), req_model.to_string(), None)
    };

    if !is_target_allowed(maybe_key, provider.as_str(), &upstream_model) {
        return None;
    }

    Some(UnaryTarget {
        provider,
        account_id: target.account_id,
        model_row_id,
        combo_target_id: Some(target.id),
        upstream_model,
        combo_id: Some(combo_id),
    })
}

fn resolve_combo_plan(
    r: &rusqlite::Connection,
    combo_id: ComboId,
    targets: Vec<openproxy_types::ComboTarget>,
    req_model: &str,
    api_key_id: Option<ApiKeyId>,
    endpoint_kind: EndpointKind,
) -> Result<Vec<UnaryTarget>> {
    let targets = routing::flatten_targets(r, targets)
        .map_err(|e| CoreError::Validation(format!("flatten_targets failed: {e}")))?;
    let targets = routing::expand_account_rotation(r, targets)
        .map_err(|e| CoreError::Validation(format!("expand_account_rotation failed: {e}")))?;

    let maybe_key =
        api_key_id.and_then(|key_id| crate::api_keys::get_by_id(r, key_id).ok().flatten());

    let unary_targets: Vec<UnaryTarget> = targets
        .into_iter()
        .filter_map(|t| resolve_single_target(r, t, req_model, combo_id, maybe_key.as_ref()))
        .collect();

    if unary_targets.is_empty() {
        return Err(CoreError::Validation(format!(
            "combo has no permitted target suitable for {endpoint_kind}"
        )));
    }
    Ok(unary_targets)
}

fn handle_model_not_found(
    db_pool: &DbPool,
    api_key_id: Option<ApiKeyId>,
    model: &str,
    hint: Option<&str>,
    endpoint_kind: EndpointKind,
    started: Instant,
) -> Result<Vec<UnaryTarget>> {
    record_unary_usage(
        db_pool,
        &UnaryUsageArgs::builder(&ProviderId::new(""), model, endpoint_kind)
            .request_id(RequestId::new())
            .api_key_id(api_key_id)
            .status_code(404)
            .error_msg(Some("model_not_found".to_string()))
            .total_ms(started.elapsed().as_millis() as u64)
            .build(),
    );
    let mut msg = format!("model not found: {model}");
    if let Some(h) = hint {
        let _ = write!(msg, " (hint: {h})");
    }
    Err(CoreError::ModelNotFound {
        provider: "<unknown>".into(),
        model: msg,
    })
}

pub fn resolve_unary_targets(
    db_pool: &DbPool,
    routing_plan: RoutingPlan,
    req_model: &str,
    endpoint_kind: EndpointKind,
    api_key_id: Option<ApiKeyId>,
    started: Instant,
) -> Result<Vec<UnaryTarget>> {
    match routing_plan {
        RoutingPlan::Combo {
            combo_id, targets, ..
        } => {
            let r = db_pool.reader();
            resolve_combo_plan(&r, combo_id, targets, req_model, api_key_id, endpoint_kind)
        }
        RoutingPlan::NotFound { model, hint } => handle_model_not_found(
            db_pool,
            api_key_id,
            &model,
            hint.as_deref(),
            endpoint_kind,
            started,
        ),
    }
}

/// Preserve the synchronous resolution contract without running its queries or
/// not-found usage write on a Tokio worker.
pub async fn resolve_unary_targets_async(
    db_pool: &DbPool,
    routing_plan: RoutingPlan,
    req_model: &str,
    endpoint_kind: EndpointKind,
    api_key_id: Option<ApiKeyId>,
    started: Instant,
) -> Result<Vec<UnaryTarget>> {
    let pool = db_pool.clone();
    let model = req_model.to_owned();
    tokio::task::spawn_blocking(move || {
        resolve_unary_targets(
            &pool,
            routing_plan,
            &model,
            endpoint_kind,
            api_key_id,
            started,
        )
    })
    .await
    .map_err(|error| CoreError::Internal(format!("unary resolution join failed: {error}")))?
}

pub fn resolve_api_key(
    db_pool: &DbPool,
    master_key: &MasterKey,
    account_id: Option<AccountId>,
    provider_id: &ProviderId,
) -> Result<String> {
    let conn = db_pool.reader();
    resolve_api_key_on_connection(&conn, master_key, account_id, provider_id)
}

fn resolve_api_key_on_connection(
    conn: &rusqlite::Connection,
    master_key: &MasterKey,
    account_id: Option<AccountId>,
    provider_id: &ProviderId,
) -> Result<String> {
    match account_id {
        Some(id) => accounts::decrypt_api_key(conn, id, master_key),
        None => match providers::get(conn, provider_id)? {
            Some(p) if matches!(p.auth_type, providers::AuthType::None) => Ok(String::new()),
            _ => Err(CoreError::Auth(format!(
                "no api key available for provider '{provider_id}'"
            ))),
        },
    }
}

/// Retrieve credentials without acquiring a SQLite guard on the Tokio worker.
pub async fn resolve_api_key_async(
    db_pool: &DbPool,
    master_key: &MasterKey,
    account_id: Option<AccountId>,
    provider_id: &ProviderId,
) -> Result<String> {
    let master_key = master_key.clone();
    let provider_id = provider_id.clone();
    db_pool
        .spawn_read(move |conn| {
            resolve_api_key_on_connection(conn, &master_key, account_id, &provider_id)
        })
        .await
}

pub fn is_target_available(
    db_pool: &DbPool,
    circuit_breaker: &CircuitBreakerRegistry,
    account_id: Option<AccountId>,
    combo_target_id: Option<ComboTargetId>,
) -> bool {
    if let Some(account_id) = account_id
        && circuit_breaker.is_healthy(CircuitBreakerKey::Account(account_id)) == Health::Unhealthy
    {
        tracing::debug!("Account {account_id:?} is unhealthy via circuit breaker, skipping");
        return false;
    }

    if let Some(target_id) = combo_target_id {
        let is_cooling_down = {
            let r = db_pool.reader();
            is_target_cooling_down(&r, target_id)
        };
        if is_cooling_down {
            tracing::debug!("Combo target {target_id:?} is in cooldown, skipping");
            return false;
        }
    }

    true
}

fn is_target_cooling_down(conn: &rusqlite::Connection, target_id: ComboTargetId) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM target_cooldowns WHERE combo_target_id = ?1 AND datetime(cooldown_until) > datetime('now')",
        rusqlite::params![target_id.0],
        |row| row.get::<_, i64>(0),
    )
    .is_ok_and(|count| count > 0)
}

/// Async counterpart preserving fail-open cooldown query behavior.
pub async fn is_target_available_async(
    db_pool: &DbPool,
    circuit_breaker: &CircuitBreakerRegistry,
    account_id: Option<AccountId>,
    combo_target_id: Option<ComboTargetId>,
) -> bool {
    if account_id.is_some_and(|id| {
        circuit_breaker.is_healthy(CircuitBreakerKey::Account(id)) == Health::Unhealthy
    }) {
        tracing::debug!(
            ?account_id,
            "Account is unhealthy via circuit breaker, skipping"
        );
        return false;
    }
    let Some(target_id) = combo_target_id else {
        return true;
    };
    let is_cooling_down = db_pool
        .spawn_read(move |conn| Ok(is_target_cooling_down(conn, target_id)))
        .await
        .unwrap_or(false);
    if is_cooling_down {
        tracing::debug!(?target_id, "Combo target is in cooldown, skipping");
    }
    !is_cooling_down
}

/// Availability check and circuit-breaker failure recording for unary endpoints
/// (images, audio, embeddings).
#[macro_export]
macro_rules! guarded_unary_target {
    (check: $db_pool:expr, $circuit_breaker:expr, $target:expr $(,)?) => {
        if !$crate::unary::is_target_available(
            $db_pool,
            $circuit_breaker,
            $target.account_id,
            $target.combo_target_id,
        ) {
            continue;
        }
    };
    (check_async: $db_pool:expr, $circuit_breaker:expr, $target:expr $(,)?) => {
        if !$crate::unary::is_target_available_async(
            $db_pool,
            $circuit_breaker,
            $target.account_id,
            $target.combo_target_id,
        )
        .await
        {
            continue;
        }
    };
    (record_failure: $circuit_breaker:expr, $target:expr $(,)?) => {
        if let Some(account_id) = $target.account_id {
            $circuit_breaker.record_failure(
                openproxy_pipeline::circuit_breaker::CircuitBreakerKey::Account(account_id),
            );
        }
    };
    (record_success: $circuit_breaker:expr, $target:expr $(,)?) => {
        if let Some(account_id) = $target.account_id {
            $circuit_breaker.record_success(
                openproxy_pipeline::circuit_breaker::CircuitBreakerKey::Account(account_id),
            );
        }
    };
}

pub fn apply_adapter_headers(
    req: &mut UpstreamRequest,
    adapter: &ProviderAdapterEnum,
    api_key: &str,
    upstream_model_id: &str,
    skip_content_type: bool,
) {
    for (k, v) in adapter.build_headers(
        api_key,
        TargetFormat::Openai,
        &ModelId::new(upstream_model_id),
    ) {
        if skip_content_type && k.eq_ignore_ascii_case("content-type") {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::from_bytes(k.as_bytes()),
            axum::http::HeaderValue::from_str(&v),
        ) {
            req.headers.insert(name, val);
        }
    }

    for (k, v) in &adapter.config().extra_headers {
        if let Ok(hn) = axum::http::HeaderName::from_bytes(k.as_bytes())
            && let Ok(hv) = axum::http::HeaderValue::from_str(v)
        {
            req.headers.insert(hn, hv);
        }
    }
}

pub async fn call_unary_upstream(
    upstream_client: &Arc<UpstreamClient>,
    adapter: &ProviderAdapterEnum,
    upstream_url: &str,
    api_key: &str,
    upstream_model_id: &str,
    payload: Bytes,
    skip_content_type: bool,
) -> Result<UpstreamResponse> {
    let mut upstream_req = UpstreamRequest::post_json(upstream_url, payload);
    apply_adapter_headers(
        &mut upstream_req,
        adapter,
        api_key,
        upstream_model_id,
        skip_content_type,
    );
    let cancel = CancellationToken::new();
    upstream_client
        .call(upstream_req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("{upstream_url}: {e:?}")))
}

pub fn map_upstream_status_error(
    status_code: u16,
    provider: &str,
    model: &str,
    err_body: &str,
) -> CoreError {
    let body_msg = if err_body.trim().is_empty() {
        format!("upstream status {status_code}")
    } else {
        err_body.to_string()
    };
    match status_code {
        429 => CoreError::RateLimited {
            provider: provider.to_string(),
            retry_after_ms: 1000,
            is_proxy_rotated: false,
        },
        401 | 403 => CoreError::Auth(body_msg),
        400 => CoreError::Validation(body_msg),
        _ => CoreError::upstream(status_code, provider, model, body_msg),
    }
}

#[cfg(test)]
mod tests;
