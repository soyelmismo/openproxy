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
    CoreError, EndpointKind, ModelId, Result, TargetFormat, UsageInput,
    ids::{
        AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId, TraceId,
    },
};

use crate::{
    accounts, models, providers,
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

#[derive(Debug, Clone)]
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
    pub request_body_json: Option<bytes::Bytes>,
    pub response_body_json: Option<serde_json::Value>,
}

impl<'a> UnaryUsageArgs<'a> {
    pub fn builder(
        provider_id: &'a ProviderId,
        upstream_model_id: &'a str,
        endpoint_kind: EndpointKind,
    ) -> UnaryUsageArgsBuilder<'a> {
        UnaryUsageArgsBuilder {
            request_id: RequestId::new(),
            api_key_id: None,
            provider_id,
            account_id: None,
            combo_id: None,
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id,
            prompt_tokens: None,
            completion_tokens: None,
            status_code: 200,
            error_msg: None,
            total_ms: 0,
            endpoint_kind,
            request_body_json: None,
            response_body_json: None,
        }
    }

    pub fn from_target(
        target: &'a UnaryTarget,
        endpoint_kind: EndpointKind,
        status_code: u16,
        total_ms: u64,
    ) -> Self {
        Self {
            request_id: RequestId::new(),
            api_key_id: None,
            provider_id: &target.provider,
            account_id: target.account_id,
            combo_id: target.combo_id,
            combo_target_id: target.combo_target_id,
            model_row_id: target.model_row_id,
            upstream_model_id: &target.upstream_model,
            prompt_tokens: None,
            completion_tokens: None,
            status_code,
            error_msg: None,
            total_ms,
            endpoint_kind,
            request_body_json: None,
            response_body_json: None,
        }
    }

    pub fn with_request_id(mut self, request_id: RequestId) -> Self {
        self.request_id = request_id;
        self
    }

    pub fn with_api_key_id(mut self, api_key_id: Option<ApiKeyId>) -> Self {
        self.api_key_id = api_key_id;
        self
    }

    pub fn with_tokens(
        mut self,
        prompt_tokens: Option<u32>,
        completion_tokens: Option<u32>,
    ) -> Self {
        self.prompt_tokens = prompt_tokens;
        self.completion_tokens = completion_tokens;
        self
    }

    pub fn with_json(
        mut self,
        request_body_json: Option<bytes::Bytes>,
        response_body_json: Option<serde_json::Value>,
    ) -> Self {
        self.request_body_json = request_body_json;
        self.response_body_json = response_body_json;
        self
    }
}

pub struct UnaryUsageArgsBuilder<'a> {
    request_id: RequestId,
    api_key_id: Option<ApiKeyId>,
    provider_id: &'a ProviderId,
    account_id: Option<AccountId>,
    combo_id: Option<ComboId>,
    combo_target_id: Option<ComboTargetId>,
    model_row_id: Option<ModelRowId>,
    upstream_model_id: &'a str,
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    status_code: u16,
    error_msg: Option<String>,
    total_ms: u64,
    endpoint_kind: EndpointKind,
    request_body_json: Option<bytes::Bytes>,
    response_body_json: Option<serde_json::Value>,
}

impl<'a> UnaryUsageArgsBuilder<'a> {
    pub fn request_id(mut self, request_id: RequestId) -> Self {
        self.request_id = request_id;
        self
    }

    pub fn api_key_id(mut self, api_key_id: Option<ApiKeyId>) -> Self {
        self.api_key_id = api_key_id;
        self
    }

    pub fn status_code(mut self, status_code: u16) -> Self {
        self.status_code = status_code;
        self
    }

    pub fn error_msg(mut self, error_msg: Option<String>) -> Self {
        self.error_msg = error_msg;
        self
    }

    pub fn total_ms(mut self, total_ms: u64) -> Self {
        self.total_ms = total_ms;
        self
    }

    pub fn prompt_tokens(mut self, prompt_tokens: Option<u32>) -> Self {
        self.prompt_tokens = prompt_tokens;
        self
    }

    pub fn completion_tokens(mut self, completion_tokens: Option<u32>) -> Self {
        self.completion_tokens = completion_tokens;
        self
    }

    pub fn request_body_json(mut self, request_body_json: Option<bytes::Bytes>) -> Self {
        self.request_body_json = request_body_json;
        self
    }

    pub fn response_body_json(mut self, response_body_json: Option<serde_json::Value>) -> Self {
        self.response_body_json = response_body_json;
        self
    }

    pub fn build(self) -> UnaryUsageArgs<'a> {
        UnaryUsageArgs {
            request_id: self.request_id,
            api_key_id: self.api_key_id,
            provider_id: self.provider_id,
            account_id: self.account_id,
            combo_id: self.combo_id,
            combo_target_id: self.combo_target_id,
            model_row_id: self.model_row_id,
            upstream_model_id: self.upstream_model_id,
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            status_code: self.status_code,
            error_msg: self.error_msg,
            total_ms: self.total_ms,
            endpoint_kind: self.endpoint_kind,
            request_body_json: self.request_body_json,
            response_body_json: self.response_body_json,
        }
    }
}

fn unary_usage_input(args: &UnaryUsageArgs<'_>) -> UsageInput {
    UsageInput {
        proxy_url: None,
        proxy_status: None,
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
        api_key_id: args.api_key_id,
        request_body_json: args.request_body_json.clone(),
        response_body_json: args.response_body_json.clone(),
        request_headers: None,
        response_headers: None,
        error_message: args.error_msg.clone(),
        race_attempts: 1,
        stop_reason: None,
        compression_savings_pct: None,
        compression_techniques: None,
        pii_redacted: None,
        flags: openproxy_types::usage::USAGE_FLAG_CLIENT_RESPONSE,
        endpoint_kind: args.endpoint_kind,
    }
}

/// Blocking counterpart for synchronous routing code and non-Tokio consumers.
pub fn record_unary_usage(db_pool: &DbPool, args: &UnaryUsageArgs<'_>) {
    let job =
        openproxy_pipeline::worker::BackgroundJob::RecordUsage(Box::new(unary_usage_input(args)));
    let result = {
        let conn = db_pool.writer_arc();
        openproxy_pipeline::worker::process_job(&conn, job)
    };
    if let Err(error) = result {
        tracing::error!(%error, "unary usage persistence failed; admitted jobs remain journaled");
    }
}

/// Async callers wait for persistence without acquiring SQLite locks on Tokio.
pub async fn record_unary_usage_async(db_pool: &DbPool, args: &UnaryUsageArgs<'_>) {
    let job =
        openproxy_pipeline::worker::BackgroundJob::RecordUsage(Box::new(unary_usage_input(args)));
    let conn = db_pool.writer_arc();
    match tokio::task::spawn_blocking(move || openproxy_pipeline::worker::process_job(&conn, job))
        .await
    {
        Ok(Ok(())) => {}
        result => tracing::error!(
            ?result,
            "unary usage persistence failed; admitted jobs remain journaled"
        ),
    }
}

/// Helper to record successful unary execution usage asynchronously.
pub async fn record_unary_target_success_async(
    db_pool: &DbPool,
    target: &UnaryTarget,
    endpoint_kind: EndpointKind,
    api_key_id: Option<ApiKeyId>,
    status_code: u16,
    total_ms: u64,
    tokens: (Option<u32>, Option<u32>),
) {
    let mut args = UnaryUsageArgs::from_target(target, endpoint_kind, status_code, total_ms)
        .with_api_key_id(api_key_id);
    if tokens.0.is_some() || tokens.1.is_some() {
        args = args.with_tokens(tokens.0, tokens.1);
    }
    record_unary_usage_async(db_pool, &args).await;
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
mod tests {
    use super::*;
    use openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry;
    use openproxy_types::config::CircuitBreakerConfig;

    fn open_test_pool() -> (DbPool, std::path::PathBuf) {
        let pool = DbPool::test_pool_with_prefix("openproxy-unary-test").expect("open pool");
        let path = pool.path().to_path_buf();
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
    fn test_unary_usage_args_builder_and_from_target() {
        let (pool, _path) = open_test_pool();
        let provider = ProviderId::new("openai");
        let target = UnaryTarget {
            provider: provider.clone(),
            account_id: None,
            model_row_id: None,
            combo_target_id: None,
            upstream_model: "text-embedding-3-small".to_string(),
            combo_id: None,
        };

        let args = UnaryUsageArgs::from_target(&target, EndpointKind::Embedding, 200, 50)
            .with_tokens(Some(10), None);
        assert_eq!(args.provider_id, &provider);
        assert_eq!(args.upstream_model_id, "text-embedding-3-small");
        assert_eq!(args.endpoint_kind, EndpointKind::Embedding);
        assert_eq!(args.status_code, 200);
        assert_eq!(args.total_ms, 50);
        assert_eq!(args.prompt_tokens, Some(10));
        assert_eq!(args.completion_tokens, None);

        let built = UnaryUsageArgs::builder(&provider, "gpt-4o", EndpointKind::Chat)
            .status_code(200)
            .total_ms(120)
            .build();
        assert_eq!(built.provider_id, &provider);
        assert_eq!(built.upstream_model_id, "gpt-4o");
        assert_eq!(built.status_code, 200);
        assert_eq!(built.total_ms, 120);

        record_unary_usage(&pool, &args);
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
