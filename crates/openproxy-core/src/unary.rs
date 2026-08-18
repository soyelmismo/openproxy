//! Shared unary execution primitives: target resolution, API key retrieval,
//! circuit breaker / cooldown checking, and standard usage logging.

use std::fmt::Write as _;
use std::time::Instant;

use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::UpstreamRequest;
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_pipeline::circuit_breaker::{CircuitBreakerKey, CircuitBreakerRegistry, Health};
use openproxy_types::{
    CoreError, EndpointKind, ModelId, Result, TargetFormat, UsageInput,
    ids::{
        AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId, TraceId,
    },
};

use crate::{
    accounts, cost, models, providers,
    routing::{self, RoutingPlan},
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
            let targets = routing::flatten_targets(&r, targets)
                .map_err(|e| CoreError::Validation(format!("flatten_targets failed: {e}")))?;
            let targets = routing::expand_account_rotation(&r, targets).map_err(|e| {
                CoreError::Validation(format!("expand_account_rotation failed: {e}"))
            })?;

            let maybe_key = if let Some(key_id) = api_key_id {
                crate::api_keys::get_by_id(&r, key_id).ok().flatten()
            } else {
                None
            };

            let mut unary_targets = Vec::with_capacity(targets.len());
            for target in targets {
                if let Some(model_row_id) = target.model_row_id {
                    let (provider, upstream_model) = {
                        let Ok(Some(model)) = models::get_by_row_id(&r, model_row_id) else {
                            continue;
                        };
                        (model.provider_id, model.model_id.as_str().to_string())
                    };
                    if let Some(key) = &maybe_key {
                        if !key.is_provider_allowed(provider.as_str()) {
                            continue;
                        }
                        if !key.is_model_allowed(&upstream_model, Some(provider.as_str())) {
                            continue;
                        }
                    }
                    unary_targets.push(UnaryTarget {
                        provider,
                        account_id: target.account_id,
                        model_row_id: Some(model_row_id),
                        combo_target_id: Some(target.id),
                        upstream_model,
                        combo_id: Some(combo_id),
                    });
                } else {
                    let provider = target.provider_id.clone();
                    let upstream_model = req_model.to_string();
                    if let Some(key) = &maybe_key {
                        if !key.is_provider_allowed(provider.as_str()) {
                            continue;
                        }
                        if !key.is_model_allowed(&upstream_model, Some(provider.as_str())) {
                            continue;
                        }
                    }
                    unary_targets.push(UnaryTarget {
                        provider,
                        account_id: target.account_id,
                        model_row_id: None,
                        combo_target_id: Some(target.id),
                        upstream_model,
                        combo_id: Some(combo_id),
                    });
                }
            }
            if unary_targets.is_empty() {
                return Err(CoreError::Validation(format!(
                    "combo has no permitted target suitable for {endpoint_kind}"
                )));
            }
            Ok(unary_targets)
        }
        RoutingPlan::NotFound { model, hint } => {
            record_unary_usage(
                db_pool,
                &UnaryUsageArgs {
                    request_id: RequestId::new(),
                    api_key_id,
                    provider_id: &ProviderId::new(""),
                    account_id: None,
                    combo_id: None,
                    combo_target_id: None,
                    model_row_id: None,
                    upstream_model_id: &model,
                    prompt_tokens: None,
                    completion_tokens: None,
                    status_code: 404,
                    error_msg: Some("model_not_found".to_string()),
                    total_ms: started.elapsed().as_millis() as u64,
                    endpoint_kind,
                },
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
    }
}

pub fn resolve_api_key(
    db_pool: &DbPool,
    master_key: &MasterKey,
    account_id: Option<AccountId>,
    provider_id: &ProviderId,
) -> Result<String> {
    match account_id {
        Some(id) => {
            let r = db_pool.reader();
            accounts::decrypt_api_key(&r, id, master_key)
        }
        None => {
            let r = db_pool.reader();
            match providers::get(&r, provider_id)? {
                Some(p) if matches!(p.auth_type, providers::AuthType::None) => Ok(String::new()),
                _ => Err(CoreError::Auth(format!(
                    "no api key available for provider '{provider_id}'"
                ))),
            }
        }
    }
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
            r.query_row(
                "SELECT COUNT(*) FROM target_cooldowns WHERE combo_target_id = ?1 AND datetime(cooldown_until) > datetime('now')",
                rusqlite::params![target_id.0],
                |row| row.get::<_, i64>(0),
            )
            .is_ok_and(|c| c > 0)
        };
        if is_cooling_down {
            tracing::debug!("Combo target {target_id:?} is in cooldown, skipping");
            return false;
        }
    }

    true
}

/// Standardizes target availability checking and circuit breaker failure recording
/// across unary endpoints (images, audio, embeddings).
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

#[derive(Debug)]
pub struct UnaryUsageArgs<'a> {
    pub request_id: RequestId,
    pub api_key_id: Option<ApiKeyId>,
    pub provider_id: &'a ProviderId,
    pub account_id: Option<AccountId>,
    pub combo_id: Option<ComboId>,
    pub combo_target_id: Option<ComboTargetId>,
    pub model_row_id: Option<ModelRowId>,
    pub upstream_model_id: &'a str,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub status_code: u16,
    pub error_msg: Option<String>,
    pub total_ms: u64,
    pub endpoint_kind: EndpointKind,
}

pub fn record_unary_usage(db_pool: &DbPool, args: &UnaryUsageArgs<'_>) {
    let input = UsageInput {
        proxy_url: None,
        proxy_status: None,
        is_proxy_rotated: false,
        request_id: args.request_id,
        trace_id: TraceId::new().to_string(),
        attempt: 1,
        provider_id: args.provider_id.clone(),
        account_id: args.account_id,
        combo_id: args.combo_id,
        combo_target_id: args.combo_target_id,
        model_row_id: args.model_row_id,
        upstream_model_id: args.upstream_model_id.to_string(),
        prompt_tokens: args.prompt_tokens,
        completion_tokens: args.completion_tokens,
        cached_tokens: None,
        connect_ms: None,
        ttft_ms: None,
        total_ms: args.total_ms,
        status_code: args.status_code,
        error_msg: args.error_msg.clone(),
        race_total: 1,
        race_lost: false,
        api_key_id: args.api_key_id,
        request_body_json: None,
        response_body_json: None,
        request_headers: None,
        response_headers: None,
        error_message: args.error_msg.clone(),
        race_attempts: 1,
        is_streaming: false,
        stream_complete: false,
        stop_reason: None,
        compression_savings_pct: None,
        compression_techniques: None,
        client_response: true,
        prompt_tokens_estimated: false,
        completion_tokens_estimated: false,
        endpoint_kind: args.endpoint_kind,
    };
    let Some(w) = db_pool.try_writer_for(std::time::Duration::from_millis(100)) else {
        tracing::warn!("hot-path writer lock timeout on unary usage row; dropping");
        return;
    };
    let _ = cost::record(&w, &input);
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
        _ => CoreError::UpstreamError {
            status: status_code,
            provider: provider.to_string(),
            model: model.to_string(),
            body: body_msg,
            is_proxy_rotated: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry;
    use openproxy_types::config::CircuitBreakerConfig;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn open_test_pool() -> (DbPool, std::path::PathBuf) {
        let n = TEST_COUNTER.fetch_add(1, Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("openproxy-unary-test-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("unary.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            openproxy_db::migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    #[test]
    fn test_resolve_unary_targets_not_found() {
        let (pool, _path) = open_test_pool();
        let plan = RoutingPlan::NotFound {
            model: "nonexistent".to_string(),
            hint: Some("check model name".to_string()),
        };
        let err = resolve_unary_targets(
            &pool,
            plan,
            "nonexistent",
            EndpointKind::Image,
            None,
            Instant::now(),
        )
        .unwrap_err();
        assert!(matches!(err, CoreError::ModelNotFound { .. }));
    }

    #[test]
    fn test_is_target_available_healthy() {
        let (pool, _path) = open_test_pool();
        let cb = CircuitBreakerRegistry::new(&CircuitBreakerConfig::default());
        assert!(is_target_available(&pool, &cb, None, None));
    }

    #[test]
    fn test_map_upstream_status_error() {
        let err_429 = map_upstream_status_error(429, "openai", "gpt-4o", "rate limited");
        assert!(matches!(err_429, CoreError::RateLimited { .. }));

        let err_401 = map_upstream_status_error(401, "openai", "gpt-4o", "unauthorized");
        assert!(matches!(err_401, CoreError::Auth(_)));

        let err_403 = map_upstream_status_error(403, "openai", "gpt-4o", "forbidden");
        assert!(matches!(err_403, CoreError::Auth(_)));

        let err_400 = map_upstream_status_error(400, "openai", "gpt-4o", "bad request");
        assert!(matches!(err_400, CoreError::Validation(_)));

        let err_500 = map_upstream_status_error(500, "openai", "gpt-4o", "server error");
        assert!(matches!(
            err_500,
            CoreError::UpstreamError { status: 500, .. }
        ));

        let err_empty = map_upstream_status_error(502, "openai", "gpt-4o", "");
        match err_empty {
            CoreError::UpstreamError { status, body, .. } => {
                assert_eq!(status, 502);
                assert_eq!(body, "upstream status 502");
            }
            _ => panic!("expected UpstreamError"),
        }
    }
}
