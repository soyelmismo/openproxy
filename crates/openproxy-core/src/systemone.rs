//! System One service: routing resolution, multi-target dispatch, and usage recording.

use std::sync::Arc;
use std::time::Instant;

use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest, UpstreamResponse,
};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry;
use openproxy_types::{
    CoreError, EndpointKind, Result,
    ids::{ApiKeyId, RequestId},
    systemone::{SystemOneRequest, SystemOneResponse},
};

use crate::routing::{self, RoutingPlan};

pub use crate::unary::{
    UnaryTarget as SystemOneTargets, UnaryTarget, UnaryUsageArgs, apply_adapter_headers,
    is_target_available, map_upstream_status_error, record_unary_usage, resolve_api_key,
    resolve_unary_targets,
};

pub type SystemOneUsageArgs<'a> = UnaryUsageArgs<'a>;

pub fn resolve_system_one_targets(
    db_pool: &DbPool,
    routing_plan: RoutingPlan,
    req_model: &str,
    api_key_id: Option<ApiKeyId>,
    started: Instant,
) -> Result<Vec<SystemOneTargets>> {
    resolve_unary_targets(
        db_pool,
        routing_plan,
        req_model,
        EndpointKind::SystemOne,
        api_key_id,
        started,
    )
}

pub async fn dispatch_system_one_request(
    upstream_client: &Arc<UpstreamClient>,
    adapter: &ProviderAdapterEnum,
    upstream_url: &str,
    api_key: &str,
    upstream_model_id: &str,
    req: &SystemOneRequest,
) -> Result<UpstreamResponse> {
    let payload = adapter.format_system_one_request(req, upstream_model_id)?;
    let mut upstream_req = UpstreamRequest::post_json(upstream_url, payload);

    apply_adapter_headers(
        &mut upstream_req,
        adapter,
        api_key,
        upstream_model_id,
        false,
    );

    let cancel = CancellationToken::new();
    upstream_client
        .call(upstream_req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("{upstream_url}: {e:?}")))
}

async fn dispatch_single_system_one(
    db_pool: &DbPool,
    upstream_client: &Arc<UpstreamClient>,
    circuit_breaker: &CircuitBreakerRegistry,
    master_key: &MasterKey,
    adapter: &ProviderAdapterEnum,
    target: &UnaryTarget,
    req: &SystemOneRequest,
) -> Result<(SystemOneResponse, u16)> {
    let upstream_url = adapter.build_system_one_url();
    let api_key = resolve_api_key(db_pool, master_key, target.account_id, &target.provider)?;

    let response = dispatch_system_one_request(
        upstream_client,
        adapter,
        &upstream_url,
        &api_key,
        &target.upstream_model,
        req,
    )
    .await
    .map_err(|e| {
        crate::guarded_unary_target!(record_failure: circuit_breaker, target);
        tracing::warn!(
            "SystemOne target failed (connection error): provider={}, error={:?}",
            target.provider,
            e
        );
        e
    })?;

    let body_bytes = response
        .body
        .collect_all()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("{upstream_url}: {e:?}")))?;

    if response.status.as_u16() != 200 {
        crate::guarded_unary_target!(record_failure: circuit_breaker, target);
        let err_body_str = std::str::from_utf8(&body_bytes).unwrap_or("");
        tracing::warn!(
            "SystemOne target failed with status {}: provider={}, body={}",
            response.status.as_u16(),
            target.provider,
            err_body_str
        );
        return Err(map_upstream_status_error(
            response.status.as_u16(),
            target.provider.as_str(),
            &target.upstream_model,
            err_body_str,
        ));
    }

    crate::guarded_unary_target!(record_success: circuit_breaker, target);

    let parsed_response: SystemOneResponse =
        serde_json::from_slice(&body_bytes).map_err(|e| {
            CoreError::Internal(format!("Failed to parse SystemOne response from upstream: {e}"))
        })?;

    Ok((parsed_response, response.status.as_u16()))
}

pub async fn execute_system_one(
    db_pool: &DbPool,
    adapters: &[ProviderAdapterEnum],
    upstream_client: &Arc<UpstreamClient>,
    circuit_breaker: &CircuitBreakerRegistry,
    master_key: &MasterKey,
    req: SystemOneRequest,
    api_key_id: Option<ApiKeyId>,
) -> Result<SystemOneResponse> {
    let started = Instant::now();
    let req_model = req.model.as_deref().unwrap_or("jev-latest");
    let routing_plan = routing::resolve_routing(db_pool, req_model).await?;
    let targets = resolve_system_one_targets(db_pool, routing_plan, req_model, api_key_id, started)?;

    let mut last_error = None;
    let mut attempt = 0;

    for target in targets {
        if !is_target_available(db_pool, circuit_breaker, target.account_id, target.combo_target_id) {
            continue;
        }

        let Some(adapter) = adapters.iter().find(|a| a.id() == &target.provider) else {
            tracing::warn!("No adapter found for provider {}", target.provider);
            continue;
        };

        attempt += 1;
        match dispatch_single_system_one(
            db_pool,
            upstream_client,
            circuit_breaker,
            master_key,
            adapter,
            &target,
            &req,
        )
        .await
        {
            Ok((parsed_response, status_code)) => {
                let total_ms = started.elapsed().as_millis() as u64;
                let (input_tokens, output_tokens) = parsed_response
                    .usage
                    .as_ref()
                    .map_or((None, None), |u| (Some(u.input_tokens as u32), Some(u.output_tokens as u32)));

                record_unary_usage(
                    db_pool,
                    &UnaryUsageArgs {
                        request_id: RequestId::new(),
                        api_key_id,
                        provider_id: &target.provider,
                        account_id: target.account_id,
                        combo_id: target.combo_id,
                        combo_target_id: target.combo_target_id,
                        model_row_id: target.model_row_id,
                        upstream_model_id: &target.upstream_model,
                        prompt_tokens: input_tokens,
                        completion_tokens: output_tokens,
                        status_code,
                        error_msg: None,
                        total_ms,
                        endpoint_kind: EndpointKind::SystemOne,
                    },
                );

                tracing::info!("SystemOne request succeeded after {attempt} attempts");
                return Ok(parsed_response);
            }
            Err(e) => {
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| CoreError::Internal("No valid SystemOne targets found".into())))
}
