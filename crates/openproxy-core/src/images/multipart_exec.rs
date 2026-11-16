//! Execution loop for multipart image requests and Horde img2img handling.

use std::time::Instant;

use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest, UpstreamResponse,
};
use openproxy_pipeline::circuit_breaker::CircuitBreakerKey;
use openproxy_types::{
    CoreError, EndpointKind, ImageGenerationRequest, ImageGenerationResponse, Result,
    ids::{ApiKeyId, RequestId},
};

use crate::images::horde_poll::{HordePollContext, poll_horde_image_generation};
use crate::images::multipart::{
    ImageMultipartKind, ImageServiceContext, ParsedImageMultipartBody,
    dispatch_image_multipart_request,
};
use crate::images::png_mask::extract_png_alpha_mask;
use crate::routing;
use crate::unary::{
    UnaryCharge, UnaryCreditLease, UnaryUsageArgs, apply_adapter_headers,
    map_upstream_status_error, resolve_api_key_async,
};

pub(crate) async fn dispatch_horde_img2img(
    upstream_client: &std::sync::Arc<UpstreamClient>,
    adapter: &ProviderAdapterEnum,
    api_key: &str,
    upstream_model_id: &str,
    body: &ParsedImageMultipartBody,
    _kind: ImageMultipartKind,
) -> Result<(UpstreamResponse, String)> {
    let base_url = adapter.config().base_url.as_str();
    let upstream_url = format!("{base_url}/generate/async");

    let ProviderAdapterEnum::Horde(horde_adapter) = adapter else {
        return Err(CoreError::Internal("expected horde adapter".into()));
    };

    let source_image = body
        .files
        .iter()
        .find(|f| f.name == "image" || f.name == "file")
        .or_else(|| body.files.first())
        .ok_or_else(|| CoreError::Validation("missing source image for horde img2img".into()))?;

    use base64::Engine as _;
    let source_image_b64 = base64::engine::general_purpose::STANDARD.encode(&source_image.bytes);

    let explicit_mask_b64 = body
        .files
        .iter()
        .find(|f| f.name == "mask")
        .map(|f| base64::engine::general_purpose::STANDARD.encode(&f.bytes));

    let source_mask_b64 = explicit_mask_b64.or_else(|| {
        extract_png_alpha_mask(&source_image.bytes)
            .map(|m| base64::engine::general_purpose::STANDARD.encode(&m))
    });

    let mut prompt = String::new();
    let (mut negative_prompt, mut size, mut quality) = (None, None, None);
    let (mut n, mut seed, mut denoising_strength) = (None, None, None);
    let mut source_processing = None;

    for (k, v) in &body.form_fields {
        match k.as_str() {
            "prompt" => prompt = v.clone(),
            "negative_prompt" => negative_prompt = Some(v.clone()),
            "size" => size = Some(v.clone()),
            "quality" => quality = Some(v.clone()),
            "n" => {
                if let Ok(parsed_n) = v.trim().parse::<u32>() {
                    n = Some(n.unwrap_or(0).max(parsed_n.max(1)));
                }
            }
            "seed" => seed = v.parse::<u64>().ok(),
            "denoising_strength" | "strength" => denoising_strength = v.parse::<f32>().ok(),
            "source_processing" => source_processing = Some(v.as_str()),
            _ => {}
        }
    }

    let mut post_processing_list = Vec::new();
    for (k, v) in &body.form_fields {
        if k == "post_processing" || k == "post_processing[]" || k == "post" {
            for part in v.split(',') {
                let trimmed = part.trim();
                if !trimmed.is_empty() && !post_processing_list.contains(&trimmed.to_string()) {
                    post_processing_list.push(trimmed.to_string());
                }
            }
        }
    }
    let post_processing = if post_processing_list.is_empty() {
        None
    } else {
        Some(post_processing_list.into_boxed_slice())
    };

    let dummy_req = ImageGenerationRequest {
        prompt,
        model: upstream_model_id.to_string(),
        n,
        quality,
        response_format: None,
        size,
        style: None,
        user: None,
        aspect_ratio: None,
        seed,
        negative_prompt,
        post_processing,
    };

    let payload = horde_adapter.build_horde_payload(
        &dummy_req,
        upstream_model_id,
        Some(source_image_b64),
        source_mask_b64,
        source_processing,
        denoising_strength,
    )?;

    let mut upstream_req = UpstreamRequest::post_json(&upstream_url, payload);
    apply_adapter_headers(
        &mut upstream_req,
        adapter,
        api_key,
        upstream_model_id,
        false,
    );

    let cancel = CancellationToken::new();
    let resp = upstream_client
        .call(upstream_req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("{upstream_url}: {e:?}")))?;

    Ok((resp, upstream_url))
}

/// Image count requested by a multipart body (`n` field), or `None` when the
/// field is present but unparseable.
///
/// The API default — and what upstreams generate for `n = 0` — is one image.
fn multipart_image_count(body: &ParsedImageMultipartBody) -> Option<u32> {
    let mut max_count: Option<u32> = None;
    for (key, raw) in &body.form_fields {
        if key == "n" {
            let parsed = raw.trim().parse::<u32>().ok()?;
            let normalized = parsed.max(1);
            max_count = Some(max_count.map_or(normalized, |prev| prev.max(normalized)));
        }
    }
    Some(max_count.unwrap_or(1))
}

pub(crate) async fn execute_image_multipart(
    ctx: &ImageServiceContext<'_>,
    body: ParsedImageMultipartBody,
    kind: ImageMultipartKind,
    api_key_id: Option<ApiKeyId>,
) -> Result<ImageGenerationResponse> {
    let started = Instant::now();

    // Take the key's turn before resolving or dispatching; the lane is released
    // only after this request's usage row (and its debit) is persisted.
    let mut credit = UnaryCreditLease::admit(ctx.db_pool, api_key_id).await?;

    // The requested image count is the only persistible billable quantity for
    // this endpoint. `None` means the field was present but not a number: a
    // prepaid request then has no bound and is refused instead of guessed.
    let images = multipart_image_count(&body);

    let routing_plan = routing::resolve_routing(ctx.db_pool, &body.model_name).await?;

    let targets = super::resolve_image_targets_async(
        ctx.db_pool,
        routing_plan,
        &body.model_name,
        api_key_id,
        started,
    )
    .await?;

    let request_id = RequestId::new();
    let mut last_error = None;
    let mut attempt = 0;

    for target in &targets {
        attempt += 1;
        let trace_id = format!("{request_id}:{attempt}");
        let has_alternatives = targets
            .iter()
            .skip(attempt)
            .any(|t| t.provider.as_str() != "horde");

        crate::guarded_unary_target!(check_async: ctx.db_pool, ctx.circuit_breaker, target);

        // Price the edit/variation before dispatch: a prepaid key never sends a
        // request whose maximum cost does not fit its balance, and a request
        // without a persistible image count is refused rather than recorded at
        // zero.
        let bound = match credit
            .authorize_dispatch(ctx.db_pool, target, UnaryCharge::Image { images }, None)
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

        let Some(adapter) = ctx
            .adapters
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

        let api_key = match resolve_api_key_async(
            ctx.db_pool,
            ctx.master_key,
            target.account_id,
            &target.provider,
        )
        .await
        {
            Ok(k) => k,
            Err(e) => {
                last_error = Some(e);
                continue;
            }
        };

        // Horde: convierte multipart a JSON img2img
        let is_horde = target.provider.as_str() == "horde";
        let effective_upstream_model = if is_horde {
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

        let (response, upstream_url) = if is_horde {
            let horde_result = dispatch_horde_img2img(
                ctx.upstream_client,
                &adapter,
                &api_key,
                &effective_upstream_model,
                &body,
                kind,
            )
            .await;
            match horde_result {
                Ok((resp, url)) => (resp, url),
                Err(e) => {
                    crate::guarded_unary_target!(record_failure: ctx.circuit_breaker, target);
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
                        "Horde img2img dispatch failed: provider={}, error={:?}",
                        target.provider,
                        e
                    );
                    last_error = Some(e);
                    continue;
                }
            }
        } else {
            let target_url = match kind {
                ImageMultipartKind::Edit => adapter.build_image_edits_url(),
                ImageMultipartKind::Variation => adapter.build_image_variations_url(),
            };
            let dispatch_result = dispatch_image_multipart_request(
                ctx.upstream_client,
                &adapter,
                &target_url,
                &api_key,
                &target.upstream_model,
                &body,
            )
            .await;
            match dispatch_result {
                Ok(r) => (r, target_url),
                Err(e) => {
                    crate::guarded_unary_target!(record_failure: ctx.circuit_breaker, target);
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
                        "Image multipart target failed (connection error): provider={}, url={}, error={:?}",
                        target.provider,
                        target_url,
                        e
                    );
                    last_error = Some(e);
                    continue;
                }
            }
        };

        let status_code = response.status.as_u16();
        let body_bytes = match response.collect().await {
            Ok(b) => b,
            Err(e) => {
                let err = CoreError::UpstreamConnection(format!("read body: {e:?}"));
                crate::guarded_unary_target!(record_failure: ctx.circuit_breaker, target);
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
                    "Image multipart body read failed: provider={}, error={:?}",
                    target.provider,
                    err
                );
                last_error = Some(err);
                continue;
            }
        };

        if status_code >= 400 {
            crate::guarded_unary_target!(record_failure: ctx.circuit_breaker, target);
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
                "Image multipart target returned error status: provider={}, status={}, body={}",
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

        // Horde requiere polling asíncrono
        let parsed_response: ImageGenerationResponse = if is_horde || status_code == 202 {
            let response_format = body
                .form_fields
                .iter()
                .find(|(k, _)| k == "response_format")
                .map(|(_, v)| v.as_str());
            let poll_ctx = HordePollContext {
                request_id: &request_id.to_string(),
                trace_id: &trace_id,
                started,
                response_format,
                has_alternatives,
            };
            match poll_horde_image_generation(
                ctx.upstream_client,
                &adapter,
                &api_key,
                &body_bytes,
                poll_ctx,
            )
            .await
            {
                Ok(res) => res,
                Err(e) => {
                    let err = CoreError::UpstreamConnection(format!("horde img2img error: {e}"));
                    crate::guarded_unary_target!(record_failure: ctx.circuit_breaker, target);
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
                    tracing::warn!("Horde img2img polling failed: {err:?}");
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
            ctx.circuit_breaker
                .record_success(CircuitBreakerKey::Account(account_id));
        }

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
        let resp_json = serde_json::to_value(&parsed_response).ok();
        let delivered_images = images.map(|req_n| {
            let actual = u32::try_from(parsed_response.data.len()).unwrap_or(req_n);
            req_n.max(actual)
        });
        let args = UnaryUsageArgs {
            request_id,
            api_key_id,
            provider_id: &target.provider,
            account_id: target.account_id,
            combo_id: target.combo_id,
            combo_target_id: target.combo_target_id,
            model_row_id: target.model_row_id,
            upstream_model_id: &target.upstream_model,
            prompt_tokens: delivered_images,
            completion_tokens: None,
            status_code,
            error_msg: None,
            total_ms,
            endpoint_kind: EndpointKind::Image,
            request_body_json: None,
            response_body_json: resp_json,
            audio_ms: None,
        };
        // The debit rides on this row; persist it before releasing the key.
        credit
            .settle_quoted(ctx.db_pool, &args, bound.as_ref())
            .await?;

        tracing::info!(
            "Image multipart request succeeded after {attempt} attempts, url={upstream_url}"
        );
        return Ok(parsed_response);
    }

    Err(last_error.unwrap_or_else(|| CoreError::Internal("No valid targets found".into())))
}
