//! Image generation request dispatch and execution.

use std::sync::Arc;
use std::time::Instant;

use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::{UpstreamClient, UpstreamResponse};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_pipeline::circuit_breaker::{CircuitBreakerKey, CircuitBreakerRegistry};
use openproxy_types::{
    CoreError, EndpointKind, ImageGenerationRequest, ImageGenerationResponse, Result,
    ids::{ApiKeyId, RequestId},
};

use crate::images::horde_poll::{HordePollContext, poll_horde_image_generation};
use crate::routing;
use crate::unary::{
    UnaryCharge, UnaryCreditLease, UnaryUsageArgs, map_upstream_status_error, resolve_api_key_async,
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

    // Take the key's turn before resolving or dispatching: a second concurrent
    // image request for the same prepaid key must read its balance only after
    // this request's debit is persisted.
    let mut credit = UnaryCreditLease::admit(db_pool, api_key_id).await?;

    let routing_plan = routing::resolve_routing(db_pool, &req.model).await?;

    let targets =
        super::resolve_image_targets_async(db_pool, routing_plan, &req.model, api_key_id, started)
            .await?;

    let request_id = RequestId::new();
    // The image count is the only persistible billable quantity: the image
    // price multiplies the `prompt_tokens` slot. An absent or zero `n` is the
    // API default of one image, which is what the upstream will generate.
    let images = req.n.filter(|count| *count > 0).unwrap_or(1);
    let mut last_error = None;
    let mut attempt = 0;

    for target in &targets {
        attempt += 1;
        let trace_id = format!("{request_id}:{attempt}");
        let has_alternatives = targets
            .iter()
            .skip(attempt)
            .any(|t| t.provider.as_str() != "horde");

        crate::guarded_unary_target!(check_async: db_pool, circuit_breaker, target);

        // Price the generation before it is dispatched. A prepaid key never
        // sends a request whose maximum cost does not fit its balance, and an
        // unpriced image model is refused instead of being recorded at zero.
        let bound = match credit
            .authorize_dispatch(
                db_pool,
                target,
                UnaryCharge::Image {
                    images: Some(images),
                },
                None,
            )
            .await
        {
            Ok(bound) => bound,
            Err(error) => {
                last_error = Some(error);
                continue;
            }
        };

        openproxy_types::emit_stage_event!(
            request_id: request_id,
            trace_id: trace_id,
            stage: "attempt_started",
            elapsed_ms: started.elapsed().as_millis() as u64,
            provider_id: target.provider.as_str(),
            upstream_model_id: target.upstream_model.clone(),
            endpoint_kind: openproxy_types::EndpointKind::Image,
        );

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

        let api_key =
            match resolve_api_key_async(db_pool, master_key, target.account_id, &target.provider)
                .await
            {
                Ok(k) => k,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

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

        crate::guarded_unary_target!(record_success: circuit_breaker, target);

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

        let total_ms = started.elapsed().as_millis() as u64;
        let (req_json, resp_json) =
            crate::unary::serialize_recorded_bodies(true, &req, &parsed_response);
        let delivered_images = super::multipart::effective_delivered_images(
            Some(images),
            parsed_response.data.len(),
        );
        let args = UnaryUsageArgs::from_target(target, EndpointKind::Image, status_code, total_ms)
            .with_request_id(request_id)
            .with_api_key_id(api_key_id)
            .with_tokens(Some(delivered_images), None)
            .with_json(req_json, resp_json);
        // The debit rides on this row; persist it before releasing the key.
        credit.settle_quoted(db_pool, &args, bound.as_ref()).await?;

        tracing::info!("Image request succeeded after {attempt} attempts");
        return Ok(parsed_response);
    }

    Err(last_error.unwrap_or_else(|| CoreError::Internal("No valid targets found".into())))
}
