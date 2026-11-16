//! System One service: routing resolution, multi-target dispatch, and usage recording.

use std::sync::Arc;
use std::time::Instant;

use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::{UpstreamClient, UpstreamResponse};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry;
use openproxy_types::{
    CoreError, EndpointKind, Result,
    ids::ApiKeyId,
    systemone::{SystemOneRequest, SystemOneResponse},
};

use crate::routing::{self, RoutingPlan};

pub use crate::unary::{
    HostLocal, UnaryCharge, UnaryCreditLease, UnaryTarget as SystemOneTargets, UnaryTarget,
    UnaryUsageArgs, is_target_available, map_upstream_status_error,
    record_unary_target_success_async, record_unary_usage, record_unary_usage_async,
    resolve_api_key, resolve_unary_targets,
};

pub type SystemOneUsageArgs<'a> = UnaryUsageArgs<'a>;

/// Whether this target is answered by the host-local decision engine rather
/// than an HTTP upstream.
///
/// Must stay the same predicate as the short-circuit in
/// [`dispatch_single_system_one`]: the budget gate relies on it to know that no
/// metered upstream is spent.
#[cfg(feature = "laya-engine")]
fn is_native_system_one_target(target: &UnaryTarget) -> bool {
    target.provider.as_str() == "laya" && openproxy_adapters::laya_engine::is_enabled()
}

#[cfg(not(feature = "laya-engine"))]
fn is_native_system_one_target(_target: &UnaryTarget) -> bool {
    false
}

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
    crate::unary::call_unary_upstream(
        upstream_client,
        adapter,
        upstream_url,
        api_key,
        upstream_model_id,
        payload,
        false,
    )
    .await
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
    #[cfg(feature = "laya-engine")]
    if is_native_system_one_target(target) {
        let mut req_clone = req.clone();
        if req_clone.model.is_none() {
            req_clone.model = Some(target.upstream_model.clone());
        }
        let parsed_response = tokio::task::spawn_blocking(move || {
            openproxy_adapters::laya_engine::execute_decision(&req_clone)
        })
        .await
        .map_err(|e| CoreError::Internal(format!("laya_engine join error: {e}")))??;

        crate::guarded_unary_target!(record_success: circuit_breaker, target);
        return Ok((parsed_response, 200));
    }

    let upstream_url = adapter.build_system_one_url();
    let api_key = crate::unary::resolve_api_key_async(
        db_pool,
        master_key,
        target.account_id,
        &target.provider,
    )
    .await?;

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

    let (body_bytes, status_code) =
        crate::unary::collect_unary_response(response, circuit_breaker, target, "SystemOne")
            .await?;

    crate::guarded_unary_target!(record_success: circuit_breaker, target);

    let parsed_response: SystemOneResponse = serde_json::from_slice(&body_bytes).map_err(|e| {
        CoreError::Internal(format!(
            "Failed to parse SystemOne response from upstream: {e}"
        ))
    })?;

    Ok((parsed_response, status_code))
}

pub async fn execute_system_one(
    deps: crate::unary::UnaryDeps<'_>,
    req: SystemOneRequest,
    api_key_id: Option<ApiKeyId>,
    recording: bool,
) -> Result<SystemOneResponse> {
    let crate::unary::UnaryDeps {
        db_pool,
        adapters,
        upstream_client,
        circuit_breaker,
        master_key,
    } = deps;
    let started = Instant::now();

    let mut credit = UnaryCreditLease::admit(db_pool, api_key_id).await?;

    let req_model = req.model.as_deref().unwrap_or("jev-latest");
    let routing_plan = routing::resolve_routing(db_pool, req_model).await?;
    let targets = crate::unary::resolve_unary_targets_async(
        db_pool,
        routing_plan,
        req_model,
        EndpointKind::SystemOne,
        api_key_id,
        started,
    )
    .await?;

    let mut last_error = None;
    let mut attempt = 0;

    for target in targets {
        crate::guarded_unary_target!(check_async: db_pool, circuit_breaker, target);

        let Some(adapter) = adapters.iter().find(|a| a.id() == &target.provider) else {
            tracing::warn!("No adapter found for provider {}", target.provider);
            continue;
        };

        let bound = match credit
            .authorize_dispatch(
                db_pool,
                &target,
                UnaryCharge::SystemOne(&req),
                HostLocal::confirm(&target, is_native_system_one_target(&target)),
            )
            .await
        {
            Ok(bound) => bound,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
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
                let (input_tokens, output_tokens) =
                    parsed_response.usage.as_ref().map_or((None, None), |u| {
                        (Some(u.input_tokens as u32), Some(u.output_tokens as u32))
                    });

                let (request_body, response_body) =
                    crate::unary::serialize_recorded_bodies(recording, &req, &parsed_response);

                let mut args = crate::unary::UnaryUsageArgs::from_target(
                    &target,
                    EndpointKind::SystemOne,
                    status_code,
                    total_ms,
                )
                .with_api_key_id(api_key_id)
                .with_json(request_body, response_body);
                if input_tokens.is_some() || output_tokens.is_some() {
                    args = args.with_tokens(input_tokens, output_tokens);
                }
                credit.settle_quoted(db_pool, &args, bound.as_ref()).await?;

                tracing::info!("SystemOne request succeeded after {attempt} attempts");
                return Ok(parsed_response);
            }
            Err(e) => {
                last_error = Some(e);
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| CoreError::Internal("No valid SystemOne targets found".into())))
}
