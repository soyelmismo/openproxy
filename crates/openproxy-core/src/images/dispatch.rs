//! Image generation request dispatch and execution.

use std::sync::Arc;
use std::time::Instant;

use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest, UpstreamResponse,
};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_pipeline::circuit_breaker::{CircuitBreakerKey, CircuitBreakerRegistry};
use openproxy_types::{
    CoreError, EndpointKind, ImageGenerationRequest, ImageGenerationResponse, Result,
    ids::{ApiKeyId, RequestId},
};

use crate::images::horde_poll::{HordePollContext, poll_horde_image_generation};
use crate::images::resolve_image_targets;
use crate::routing;
use crate::unary::{
    UnaryUsageArgs, apply_adapter_headers, map_upstream_status_error, record_unary_usage,
    resolve_api_key,
};

pub async fn dispatch_image_request(
    upstream_client: &Arc<UpstreamClient>,
    adapter: &ProviderAdapterEnum,
    upstream_url: &str,
    api_key: &str,
    upstream_model_id: &str,
    req: &ImageGenerationRequest,
) -> Result<UpstreamResponse> {
    let payload = adapter.format_image_request(req, upstream_model_id)?;
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

pub async fn execute_image_generation(
    db_pool: &DbPool,
    adapters: &[ProviderAdapterEnum],
    upstream_client: &Arc<UpstreamClient>,
    circuit_breaker: &CircuitBreakerRegistry,
    master_key: &MasterKey,
    req: ImageGenerationRequest,
    api_key_id: Option<ApiKeyId>,
) -> Result<ImageGenerationResponse> {
    let started = Instant::now();

    // 1. Resolve routing plan.
    let routing_plan = routing::resolve_routing(db_pool, &req.model).await?;

    // 2. Resolve image targets.
    let targets = resolve_image_targets(db_pool, routing_plan, &req.model, api_key_id, started)?;

    let request_id = RequestId::new();
    let mut last_error = None;
    let mut attempt = 0;

    // 3. Multi-target dispatch loop.
    for target in &targets {
        attempt += 1;
        let trace_id = format!("{request_id}:{attempt}");
        let has_alternatives = targets
            .iter()
            .skip(attempt)
            .any(|t| t.provider.as_str() != "horde");

        crate::guarded_unary_target!(check: db_pool, circuit_breaker, target);

        // Publish live log in-flight stage event
        openproxy_types::emit_stage_event!(
            request_id: request_id,
            trace_id: trace_id,
            stage: "attempt_started",
            elapsed_ms: started.elapsed().as_millis() as u64,
            provider_id: target.provider.as_str(),
            upstream_model_id: target.upstream_model.clone(),
            endpoint_kind: openproxy_types::EndpointKind::Image,
        );

        // Adapter resolution.
        let Some(adapter) = adapters
            .iter()
            .find(|a| a.id() == &target.provider)
            .cloned()
        else {
            last_error = Some(CoreError::Internal(format!(
                "no adapter registered for provider '{}'",
                target.provider
            )));
            continue;
        };
        let upstream_url = adapter.build_image_url();

        // Credentials decryption via master key.
        let api_key =
            match resolve_api_key(db_pool, master_key, target.account_id, &target.provider) {
                Ok(k) => k,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

        // Dispatch upstream.
        let effective_upstream_model = if target.provider.as_str() == "horde" {
            let horde_models: Vec<&str> = targets
                .iter()
                .filter(|t| t.provider.as_str() == "horde" && t.account_id == target.account_id)
                .map(|t| t.upstream_model.as_str())
                .collect();
            if horde_models.len() > 1 {
                horde_models.join(",")
            } else {
                target.upstream_model.clone()
            }
        } else {
            target.upstream_model.clone()
        };

        let response = match dispatch_image_request(
            upstream_client,
            &adapter,
            &upstream_url,
            &api_key,
            &effective_upstream_model,
            &req,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                crate::guarded_unary_target!(record_failure: circuit_breaker, target);
                openproxy_types::emit_stage_event!(
                    request_id: request_id,
                    trace_id: trace_id,
                    stage: "failed",
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    provider_id: target.provider.as_str(),
                    upstream_model_id: target.upstream_model.clone(),
                    status_code: 503,
                    error: format!("{e:?}"),
                    endpoint_kind: openproxy_types::EndpointKind::Image,
                );
                tracing::warn!(
                    "Image target failed (connection error): provider={}, error={:?}",
                    target.provider,
                    e
                );
                last_error = Some(e);
                continue;
            }
        };

        let status_code = response.status.as_u16();
        let body_bytes = match response.collect().await {
            Ok(b) => b,
            Err(e) => {
                let err = CoreError::UpstreamConnection(format!("read body: {e:?}"));
                crate::guarded_unary_target!(record_failure: circuit_breaker, target);
                openproxy_types::emit_stage_event!(
                    request_id: request_id,
                    trace_id: trace_id,
                    stage: "failed",
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    provider_id: target.provider.as_str(),
                    upstream_model_id: target.upstream_model.clone(),
                    status_code: status_code,
                    error: format!("{err:?}"),
                    endpoint_kind: openproxy_types::EndpointKind::Image,
                );
                tracing::warn!(
                    "Image target body read failed: provider={}, error={:?}",
                    target.provider,
                    err
                );
                last_error = Some(err);
                continue;
            }
        };

        if status_code >= 400 {
            crate::guarded_unary_target!(record_failure: circuit_breaker, target);
            let err_text = String::from_utf8_lossy(&body_bytes);
            openproxy_types::emit_stage_event!(
                request_id: request_id,
                trace_id: trace_id,
                stage: "failed",
                elapsed_ms: started.elapsed().as_millis() as u64,
                provider_id: target.provider.as_str(),
                upstream_model_id: target.upstream_model.clone(),
                status_code: status_code,
                error: err_text.to_string(),
                endpoint_kind: openproxy_types::EndpointKind::Image,
            );
            tracing::warn!(
                "Image target returned error status: provider={}, status={}, body={}",
                target.provider,
                status_code,
                err_text
            );
            let err = map_upstream_status_error(
                status_code,
                target.provider.as_str(),
                &target.upstream_model,
                &err_text,
            );
            last_error = Some(err);
            continue;
        }

        // Parse upstream response into standard ImageGenerationResponse.
        let parsed_response: ImageGenerationResponse = if target.provider.as_str() == "horde"
            || status_code == 202
        {
            let poll_ctx = HordePollContext {
                request_id: &request_id.to_string(),
                trace_id: &trace_id,
                started,
                response_format: req.response_format.as_deref(),
                has_alternatives,
            };
            match poll_horde_image_generation(
                upstream_client,
                &adapter,
                &api_key,
                &body_bytes,
                poll_ctx,
            )
            .await
            {
                Ok(res) => res,
                Err(e) => {
                    let err = CoreError::UpstreamConnection(format!("horde generation error: {e}"));
                    crate::guarded_unary_target!(record_failure: circuit_breaker, target);
                    openproxy_types::emit_stage_event!(
                        request_id: request_id,
                        trace_id: trace_id,
                        stage: "failed",
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        provider_id: target.provider.as_str(),
                        upstream_model_id: target.upstream_model.clone(),
                        status_code: 504,
                        error: format!("{err:?}"),
                        endpoint_kind: openproxy_types::EndpointKind::Image,
                    );
                    tracing::warn!("Horde generation polling failed: {err:?}");
                    last_error = Some(err);
                    continue;
                }
            }
        } else {
            match serde_json::from_slice(&body_bytes) {
                Ok(res) => res,
                Err(e) => {
                    let err = CoreError::Parse(format!("failed to parse image response: {e}"));
                    last_error = Some(err);
                    continue;
                }
            }
        };

        if let Some(account_id) = target.account_id {
            circuit_breaker.record_success(CircuitBreakerKey::Account(account_id));
        }

        // Publish live log completed event
        openproxy_types::emit_stage_event!(
            request_id: request_id,
            trace_id: trace_id,
            stage: "completed",
            elapsed_ms: started.elapsed().as_millis() as u64,
            provider_id: target.provider.as_str(),
            upstream_model_id: target.upstream_model.clone(),
            status_code: status_code,
            endpoint_kind: openproxy_types::EndpointKind::Image,
        );

        // Record usage row in openproxy-db.
        let total_ms = started.elapsed().as_millis() as u64;
        record_unary_usage(
            db_pool,
            &UnaryUsageArgs {
                request_id,
                api_key_id,
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
                endpoint_kind: EndpointKind::Image,
            },
        );

        tracing::info!("Image request succeeded after {attempt} attempts");
        return Ok(parsed_response);
    }

    Err(last_error.unwrap_or_else(|| CoreError::Internal("No valid targets found".into())))
}
