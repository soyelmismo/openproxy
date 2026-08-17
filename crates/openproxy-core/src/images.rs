//! Image generation service: routing resolution, multi-target dispatch, and usage recording.

use std::sync::Arc;
use std::time::Instant;

use http::HeaderValue;
use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest, UpstreamResponse,
};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_pipeline::circuit_breaker::{CircuitBreakerKey, CircuitBreakerRegistry};
use openproxy_types::{
    CoreError, EndpointKind, ImageData, ImageGenerationRequest, ImageGenerationResponse, Result,
    ids::{AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId},
};
use serde::Deserialize;

use crate::routing::{self, RoutingPlan};

#[derive(Clone, Debug)]
pub struct MultipartFile {
    pub name: String,
    pub file_name: String,
    pub content_type: String,
    pub bytes: bytes::Bytes,
}

#[derive(Clone, Debug)]
pub struct ParsedImageMultipartBody {
    pub model_name: String,
    pub files: Vec<MultipartFile>,
    pub form_fields: Vec<(String, String)>,
}

pub use crate::unary::{
    UnaryTarget as ImageTargets, is_target_available, record_unary_usage, resolve_api_key,
    resolve_unary_targets,
};

pub fn resolve_image_targets(
    db_pool: &DbPool,
    routing_plan: RoutingPlan,
    req_model: &str,
    api_key_id: Option<ApiKeyId>,
    started: Instant,
) -> Result<Vec<ImageTargets>> {
    resolve_unary_targets(
        db_pool,
        routing_plan,
        req_model,
        EndpointKind::Image,
        api_key_id,
        started,
    )
}

struct HordePollContext<'a> {
    request_id: &'a str,
    trace_id: &'a str,
    started: Instant,
    response_format: Option<&'a str>,
    has_alternatives: bool,
}

pub struct ImageUsageArgs<'a> {
    pub db_pool: &'a DbPool,
    pub request_id: RequestId,
    pub api_key_id: Option<ApiKeyId>,
    pub provider_id: &'a ProviderId,
    pub account_id: Option<AccountId>,
    pub combo_id: Option<ComboId>,
    pub combo_target_id: Option<ComboTargetId>,
    pub model_row_id: Option<ModelRowId>,
    pub upstream_model_id: &'a str,
    pub status_code: u16,
    pub error_msg: Option<String>,
    pub total_ms: u64,
}

pub fn record_image_usage_row(args: ImageUsageArgs<'_>) {
    record_unary_usage(
        args.db_pool,
        &crate::unary::UnaryUsageArgs {
            request_id: args.request_id,
            api_key_id: args.api_key_id,
            provider_id: args.provider_id,
            account_id: args.account_id,
            combo_id: args.combo_id,
            combo_target_id: args.combo_target_id,
            model_row_id: args.model_row_id,
            upstream_model_id: args.upstream_model_id,
            prompt_tokens: None,
            completion_tokens: None,
            status_code: args.status_code,
            error_msg: args.error_msg,
            total_ms: args.total_ms,
            endpoint_kind: EndpointKind::Image,
        },
    );
}

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

    for (k, v) in adapter.build_headers(
        api_key,
        openproxy_types::TargetFormat::Openai,
        &openproxy_types::ModelId::new(upstream_model_id),
    ) {
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::from_bytes(k.as_bytes()),
            axum::http::HeaderValue::from_str(&v),
        ) {
            upstream_req.headers.insert(name, val);
        }
    }

    for (k, v) in &adapter.config().extra_headers {
        if let Ok(hn) = axum::http::HeaderName::from_bytes(k.as_bytes())
            && let Ok(hv) = axum::http::HeaderValue::from_str(v)
        {
            upstream_req.headers.insert(hn, hv);
        }
    }

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
    let routing_plan = {
        let r = db_pool.reader();
        routing::resolve(&r, &req.model)?
    };

    // 2. Resolve image targets.
    let targets = resolve_image_targets(db_pool, routing_plan, &req.model, api_key_id, started)?;

    let request_id = RequestId::new();
    let mut last_error = None;
    let mut attempt = 0;

    // 3. Multi-target dispatch loop.
    for target in &targets {
        attempt += 1;
        let trace_id = format!("{request_id}:{attempt}");
        let has_alternatives = targets.iter().skip(attempt).any(|t| t.provider.as_str() != "horde");

        if !is_target_available(
            db_pool,
            circuit_breaker,
            target.account_id,
            target.combo_target_id,
        ) {
            continue;
        }

        // Publish live log in-flight stage event
        openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
            request_id: request_id.to_string(),
            trace_id: trace_id.clone(),
            provider_id: Some(target.provider.as_str().to_string()),
            upstream_model_id: Some(target.upstream_model.clone()),
            stage: "attempt_started".to_string(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: None,
            ttft_ms: None,
            status_code: None,
            error: None,
            stop_reason: None,
            timestamp: None,
            endpoint_kind: Some(openproxy_types::EndpointKind::Image),
        });

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
                if let Some(account_id) = target.account_id {
                    circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
                }
                openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
                    request_id: request_id.to_string(),
                    trace_id: trace_id.clone(),
                    provider_id: Some(target.provider.as_str().to_string()),
                    upstream_model_id: Some(target.upstream_model.clone()),
                    stage: "failed".to_string(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    connect_ms: None,
                    ttft_ms: None,
                    status_code: Some(503),
                    error: Some(format!("{e:?}")),
                    stop_reason: None,
                    timestamp: None,
                    endpoint_kind: Some(openproxy_types::EndpointKind::Image),
                });
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
                if let Some(account_id) = target.account_id {
                    circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
                }
                openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
                    request_id: request_id.to_string(),
                    trace_id: trace_id.clone(),
                    provider_id: Some(target.provider.as_str().to_string()),
                    upstream_model_id: Some(target.upstream_model.clone()),
                    stage: "failed".to_string(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    connect_ms: None,
                    ttft_ms: None,
                    status_code: Some(status_code),
                    error: Some(format!("{err:?}")),
                    stop_reason: None,
                    timestamp: None,
                    endpoint_kind: Some(openproxy_types::EndpointKind::Image),
                });
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
            if let Some(account_id) = target.account_id {
                circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
            }
            let err_text = String::from_utf8_lossy(&body_bytes);
            openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
                request_id: request_id.to_string(),
                trace_id: trace_id.clone(),
                provider_id: Some(target.provider.as_str().to_string()),
                upstream_model_id: Some(target.upstream_model.clone()),
                stage: "failed".to_string(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                connect_ms: None,
                ttft_ms: None,
                status_code: Some(status_code),
                error: Some(err_text.to_string()),
                stop_reason: None,
                timestamp: None,
                endpoint_kind: Some(openproxy_types::EndpointKind::Image),
            });
            tracing::warn!(
                "Image target returned error status: provider={}, status={}, body={}",
                target.provider,
                status_code,
                err_text
            );
            let err = if status_code == 429 {
                CoreError::RateLimited {
                    provider: target.provider.as_str().to_string(),
                    retry_after_ms: 1000,
                    is_proxy_rotated: false,
                }
            } else if status_code == 401 || status_code == 403 {
                CoreError::Auth(err_text.to_string())
            } else if status_code == 400 {
                CoreError::Validation(err_text.to_string())
            } else {
                CoreError::UpstreamError {
                    status: status_code,
                    provider: target.provider.as_str().to_string(),
                    model: target.upstream_model.clone(),
                    body: err_text.to_string(),
                    is_proxy_rotated: false,
                }
            };
            last_error = Some(err);
            continue;
        }

        // Parse upstream response into standard ImageGenerationResponse.
        let parsed_response: ImageGenerationResponse = if target.provider.as_str() == "horde" || status_code == 202 {
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
                    if let Some(account_id) = target.account_id {
                        circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
                    }
                    openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
                        request_id: request_id.to_string(),
                        trace_id: trace_id.clone(),
                        provider_id: Some(target.provider.as_str().to_string()),
                        upstream_model_id: Some(target.upstream_model.clone()),
                        stage: "failed".to_string(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: Some(504),
                        error: Some(format!("{err:?}")),
                        stop_reason: None,
                        timestamp: None,
                        endpoint_kind: Some(openproxy_types::EndpointKind::Image),
                    });
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
        openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
            request_id: request_id.to_string(),
            trace_id: trace_id.clone(),
            provider_id: Some(target.provider.as_str().to_string()),
            upstream_model_id: Some(target.upstream_model.clone()),
            stage: "completed".to_string(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: None,
            ttft_ms: None,
            status_code: Some(status_code),
            error: None,
            stop_reason: None,
            timestamp: None,
            endpoint_kind: Some(openproxy_types::EndpointKind::Image),
        });

        // Record usage row in openproxy-db.
        let total_ms = started.elapsed().as_millis() as u64;
        record_image_usage_row(ImageUsageArgs {
            db_pool,
            request_id,
            api_key_id,
            provider_id: &target.provider,
            account_id: target.account_id,
            combo_id: target.combo_id,
            combo_target_id: target.combo_target_id,
            model_row_id: target.model_row_id,
            upstream_model_id: &target.upstream_model,
            status_code,
            error_msg: None,
            total_ms,
        });

        tracing::info!("Image request succeeded after {attempt} attempts");
        return Ok(parsed_response);
    }

    Err(last_error.unwrap_or_else(|| CoreError::Internal("No valid targets found".into())))
}

pub async fn dispatch_image_multipart_request(
    upstream_client: &Arc<UpstreamClient>,
    adapter: &ProviderAdapterEnum,
    upstream_url: &str,
    api_key: &str,
    upstream_model_id: &str,
    body: &ParsedImageMultipartBody,
) -> Result<UpstreamResponse> {
    let boundary = format!("----WebKitFormBoundary{}", uuid::Uuid::new_v4().simple());
    let mut payload = Vec::new();

    // model field
    payload.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    payload.extend_from_slice(b"Content-Disposition: form-data; name=\"model\"\r\n\r\n");
    payload.extend_from_slice(upstream_model_id.as_bytes());
    payload.extend_from_slice(b"\r\n");

    // form fields
    for (k, v) in &body.form_fields {
        if k == "model" {
            continue;
        }
        payload.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        payload.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{k}\"\r\n\r\n").as_bytes(),
        );
        payload.extend_from_slice(v.as_bytes());
        payload.extend_from_slice(b"\r\n");
    }

    // files
    for file in &body.files {
        payload.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        payload.extend_from_slice(
            format!(
                "Content-Disposition: form-data; name=\"{}\"; filename=\"{}\"\r\n",
                file.name, file.file_name
            )
            .as_bytes(),
        );
        payload.extend_from_slice(
            format!("Content-Type: {}\r\n\r\n", file.content_type).as_bytes(),
        );
        payload.extend_from_slice(&file.bytes);
        payload.extend_from_slice(b"\r\n");
    }

    // end boundary
    payload.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let content_type = format!("multipart/form-data; boundary={boundary}");
    let mut upstream_req = UpstreamRequest::post_multipart(
        upstream_url,
        &content_type,
        bytes::Bytes::from(payload),
    );

    for (k, v) in adapter.build_headers(
        api_key,
        openproxy_types::TargetFormat::Openai,
        &openproxy_types::ModelId::new(upstream_model_id),
    ) {
        if k.eq_ignore_ascii_case("content-type") {
            continue;
        }
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::from_bytes(k.as_bytes()),
            axum::http::HeaderValue::from_str(&v),
        ) {
            upstream_req.headers.insert(name, val);
        }
    }

    for (k, v) in &adapter.config().extra_headers {
        if let Ok(hn) = axum::http::HeaderName::from_bytes(k.as_bytes())
            && let Ok(hv) = axum::http::HeaderValue::from_str(v)
        {
            upstream_req.headers.insert(hn, hv);
        }
    }

    let cancel = CancellationToken::new();
    upstream_client
        .call(upstream_req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("{upstream_url}: {e:?}")))
}

fn paeth_predictor(a: u8, b: u8, c: u8) -> u8 {
    let a_i = i32::from(a);
    let b_i = i32::from(b);
    let c_i = i32::from(c);
    let p = a_i + b_i - c_i;
    let pa = (p - a_i).abs();
    let pb = (p - b_i).abs();
    let pc = (p - c_i).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn png_crc32_chunk(chunk_type: [u8; 4], data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in chunk_type.iter().chain(data.iter()) {
        crc ^= u32::from(b);
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

fn write_png_chunk(buf: &mut Vec<u8>, chunk_type: [u8; 4], data: &[u8]) {
    let len = data.len() as u32;
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&chunk_type);
    buf.extend_from_slice(data);
    let crc = png_crc32_chunk(chunk_type, data);
    buf.extend_from_slice(&crc.to_be_bytes());
}

/// Extract an inpainting mask from the transparent alpha channel of a PNG image.
/// Returns `Some(mask_png_bytes)` if transparency was found, or `None` if the image
/// has no transparency, is opaque, or is not a valid 8-bit RGBA/GA PNG.
pub fn extract_png_alpha_mask(image_bytes: &[u8]) -> Option<Vec<u8>> {
    const PNG_SIG: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    if !image_bytes.starts_with(&PNG_SIG) {
        return None;
    }

    let mut offset = 8;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut color_type = 0u8;
    let mut idat_data = Vec::new();

    while offset + 8 <= image_bytes.len() {
        let chunk_len = u32::from_be_bytes([
            image_bytes[offset],
            image_bytes[offset + 1],
            image_bytes[offset + 2],
            image_bytes[offset + 3],
        ]) as usize;
        let chunk_type = &image_bytes[offset + 4..offset + 8];
        let data_start = offset + 8;
        let data_end = data_start + chunk_len;
        if data_end + 4 > image_bytes.len() {
            return None;
        }

        if chunk_type == b"IHDR" {
            if chunk_len < 13 {
                return None;
            }
            let data = &image_bytes[data_start..data_end];
            width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
            height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
            let bit_depth = data[8];
            color_type = data[9];
            let compression = data[10];
            let filter = data[11];
            let interlace = data[12];

            if bit_depth != 8
                || compression != 0
                || filter != 0
                || interlace != 0
                || (color_type != 6 && color_type != 4)
            {
                return None;
            }
        } else if chunk_type == b"IDAT" {
            idat_data.extend_from_slice(&image_bytes[data_start..data_end]);
        } else if chunk_type == b"IEND" {
            break;
        }

        offset = data_end + 4; // chunk data + 4 bytes CRC
    }

    if width == 0 || height == 0 || idat_data.is_empty() {
        return None;
    }

    let decompressed = miniz_oxide::inflate::decompress_to_vec_zlib(&idat_data).ok()?;
    let bpp: usize = if color_type == 6 { 4 } else { 2 };
    let line_len = (width as usize).checked_mul(bpp)?;
    let stride = 1 + line_len;
    let expected_len = (height as usize).checked_mul(stride)?;

    if decompressed.len() != expected_len {
        return None;
    }

    let mut prev_row = vec![0u8; line_len];
    let mut curr_row = vec![0u8; line_len];
    let mut mask_scanlines = Vec::with_capacity(height as usize * (1 + width as usize));
    let mut has_transparency = false;

    for y in 0..height as usize {
        let filter_type = decompressed[y * stride];
        let raw_data = &decompressed[y * stride + 1..(y + 1) * stride];

        for (i, &x) in raw_data.iter().enumerate() {
            let a = if i >= bpp { curr_row[i - bpp] } else { 0 };
            let b = prev_row[i];
            let c = if i >= bpp { prev_row[i - bpp] } else { 0 };

            curr_row[i] = match filter_type {
                0 => x,
                1 => x.wrapping_add(a),
                2 => x.wrapping_add(b),
                3 => x.wrapping_add(u16::midpoint(u16::from(a), u16::from(b)) as u8),
                4 => x.wrapping_add(paeth_predictor(a, b, c)),
                _ => x,
            };
        }

        mask_scanlines.push(0); // None filter for mask row
        for px in 0..width as usize {
            let alpha = if color_type == 6 {
                curr_row[px * 4 + 3]
            } else {
                curr_row[px * 2 + 1]
            };

            if alpha < 255 {
                has_transparency = true;
                mask_scanlines.push(255);
            } else {
                mask_scanlines.push(0);
            }
        }

        prev_row.copy_from_slice(&curr_row);
    }

    if !has_transparency {
        return None;
    }

    let compressed_mask = miniz_oxide::deflate::compress_to_vec_zlib(&mask_scanlines, 6);
    let mut out = Vec::with_capacity(33 + compressed_mask.len() + 12);
    out.extend_from_slice(&PNG_SIG);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(8); // bit depth 8
    ihdr.push(0); // color type 0 (Grayscale)
    ihdr.push(0); // compression method (zlib)
    ihdr.push(0); // filter method (adaptive)
    ihdr.push(0); // interlace method (none)
    write_png_chunk(&mut out, *b"IHDR", &ihdr);
    write_png_chunk(&mut out, *b"IDAT", &compressed_mask);
    write_png_chunk(&mut out, *b"IEND", &[]);

    Some(out)
}

async fn dispatch_horde_img2img(
    upstream_client: &Arc<UpstreamClient>,
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

    let source_mask_b64 = if explicit_mask_b64.is_some() {
        explicit_mask_b64
    } else {
        extract_png_alpha_mask(&source_image.bytes)
            .map(|mask_bytes| base64::engine::general_purpose::STANDARD.encode(&mask_bytes))
    };

    let prompt = body
        .form_fields
        .iter()
        .find(|(k, _)| k == "prompt")
        .map(|(_, v)| v.clone())
        .unwrap_or_default();

    let negative_prompt = body
        .form_fields
        .iter()
        .find(|(k, _)| k == "negative_prompt")
        .map(|(_, v)| v.clone());

    let size = body
        .form_fields
        .iter()
        .find(|(k, _)| k == "size")
        .map(|(_, v)| v.clone());

    let quality = body
        .form_fields
        .iter()
        .find(|(k, _)| k == "quality")
        .map(|(_, v)| v.clone());

    let n = body
        .form_fields
        .iter()
        .find(|(k, _)| k == "n")
        .and_then(|(_, v)| v.parse::<u32>().ok());

    let seed = body
        .form_fields
        .iter()
        .find(|(k, _)| k == "seed")
        .and_then(|(_, v)| v.parse::<u64>().ok());

    let denoising_strength = body
        .form_fields
        .iter()
        .find(|(k, _)| k == "denoising_strength" || k == "strength")
        .and_then(|(_, v)| v.parse::<f32>().ok());

    let source_processing = body
        .form_fields
        .iter()
        .find(|(k, _)| k == "source_processing")
        .map(|(_, v)| v.as_str());

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
        Some(post_processing_list)
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
    for (k, v) in adapter.build_headers(
        api_key,
        openproxy_types::TargetFormat::Openai,
        &openproxy_types::ModelId::new(upstream_model_id),
    ) {
        if let (Ok(name), Ok(val)) = (
            axum::http::HeaderName::from_bytes(k.as_bytes()),
            axum::http::HeaderValue::from_str(&v),
        ) {
            upstream_req.headers.insert(name, val);
        }
    }

    for (k, v) in &adapter.config().extra_headers {
        if let Ok(hn) = axum::http::HeaderName::from_bytes(k.as_bytes())
            && let Ok(hv) = axum::http::HeaderValue::from_str(v)
        {
            upstream_req.headers.insert(hn, hv);
        }
    }

    let cancel = CancellationToken::new();
    let resp = upstream_client
        .call(upstream_req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("{upstream_url}: {e:?}")))?;

    Ok((resp, upstream_url))
}

pub struct ImageServiceContext<'a> {
    pub db_pool: &'a DbPool,
    pub adapters: &'a [ProviderAdapterEnum],
    pub upstream_client: &'a Arc<UpstreamClient>,
    pub circuit_breaker: &'a CircuitBreakerRegistry,
    pub master_key: &'a MasterKey,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImageMultipartKind {
    Edit,
    Variation,
}

async fn execute_image_multipart(
    ctx: &ImageServiceContext<'_>,
    body: ParsedImageMultipartBody,
    kind: ImageMultipartKind,
    api_key_id: Option<ApiKeyId>,
) -> Result<ImageGenerationResponse> {
    let started = Instant::now();

    // 1. Resolve routing plan.
    let routing_plan = {
        let r = ctx.db_pool.reader();
        routing::resolve(&r, &body.model_name)?
    };

    // 2. Resolve image targets.
    let targets = resolve_image_targets(ctx.db_pool, routing_plan, &body.model_name, api_key_id, started)?;

    let request_id = RequestId::new();
    let mut last_error = None;
    let mut attempt = 0;

    // 3. Multi-target dispatch loop.
    for target in &targets {
        attempt += 1;
        let trace_id = format!("{request_id}:{attempt}");
        let has_alternatives = targets.iter().skip(attempt).any(|t| t.provider.as_str() != "horde");

        if !is_target_available(
            ctx.db_pool,
            ctx.circuit_breaker,
            target.account_id,
            target.combo_target_id,
        ) {
            continue;
        }

        // Publish live log in-flight stage event
        openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
            request_id: request_id.to_string(),
            trace_id: trace_id.clone(),
            provider_id: Some(target.provider.as_str().to_string()),
            upstream_model_id: Some(target.upstream_model.clone()),
            stage: "attempt_started".to_string(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: None,
            ttft_ms: None,
            status_code: None,
            error: None,
            stop_reason: None,
            timestamp: None,
            endpoint_kind: Some(openproxy_types::EndpointKind::Image),
        });

        // Adapter resolution.
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

        // Credentials decryption via master key.
        let api_key =
            match resolve_api_key(ctx.db_pool, ctx.master_key, target.account_id, &target.provider) {
                Ok(k) => k,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };

        // --- Horde special path: convert multipart to JSON img2img ---
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
                    if let Some(account_id) = target.account_id {
                        ctx.circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
                    }
                    openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
                        request_id: request_id.to_string(),
                        trace_id: trace_id.clone(),
                        provider_id: Some(target.provider.as_str().to_string()),
                        upstream_model_id: Some(target.upstream_model.clone()),
                        stage: "failed".to_string(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: Some(503),
                        error: Some(format!("{e:?}")),
                        stop_reason: None,
                        timestamp: None,
                        endpoint_kind: Some(openproxy_types::EndpointKind::Image),
                    });
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
                    if let Some(account_id) = target.account_id {
                        ctx.circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
                    }
                    openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
                        request_id: request_id.to_string(),
                        trace_id: trace_id.clone(),
                        provider_id: Some(target.provider.as_str().to_string()),
                        upstream_model_id: Some(target.upstream_model.clone()),
                        stage: "failed".to_string(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: Some(503),
                        error: Some(format!("{e:?}")),
                        stop_reason: None,
                        timestamp: None,
                        endpoint_kind: Some(openproxy_types::EndpointKind::Image),
                    });
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
                if let Some(account_id) = target.account_id {
                    ctx.circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
                }
                openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
                    request_id: request_id.to_string(),
                    trace_id: trace_id.clone(),
                    provider_id: Some(target.provider.as_str().to_string()),
                    upstream_model_id: Some(target.upstream_model.clone()),
                    stage: "failed".to_string(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    connect_ms: None,
                    ttft_ms: None,
                    status_code: Some(status_code),
                    error: Some(format!("{err:?}")),
                    stop_reason: None,
                    timestamp: None,
                    endpoint_kind: Some(openproxy_types::EndpointKind::Image),
                });
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
            if let Some(account_id) = target.account_id {
                ctx.circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
            }
            let err_text = String::from_utf8_lossy(&body_bytes);
            openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
                request_id: request_id.to_string(),
                trace_id: trace_id.clone(),
                provider_id: Some(target.provider.as_str().to_string()),
                upstream_model_id: Some(target.upstream_model.clone()),
                stage: "failed".to_string(),
                elapsed_ms: started.elapsed().as_millis() as u64,
                connect_ms: None,
                ttft_ms: None,
                status_code: Some(status_code),
                error: Some(err_text.to_string()),
                stop_reason: None,
                timestamp: None,
                endpoint_kind: Some(openproxy_types::EndpointKind::Image),
            });
            tracing::warn!(
                "Image multipart target returned error status: provider={}, status={}, body={}",
                target.provider,
                status_code,
                err_text
            );
            let err = if status_code == 429 {
                CoreError::RateLimited {
                    provider: target.provider.as_str().to_string(),
                    retry_after_ms: 1000,
                    is_proxy_rotated: false,
                }
            } else if status_code == 401 || status_code == 403 {
                CoreError::Auth(err_text.to_string())
            } else if status_code == 400 {
                CoreError::Validation(err_text.to_string())
            } else {
                CoreError::UpstreamError {
                    status: status_code,
                    provider: target.provider.as_str().to_string(),
                    model: target.upstream_model.clone(),
                    body: err_text.to_string(),
                    is_proxy_rotated: false,
                }
            };
            last_error = Some(err);
            continue;
        }

        // Parse upstream response — Horde requires async polling.
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
                    if let Some(account_id) = target.account_id {
                        ctx.circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
                    }
                    openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
                        request_id: request_id.to_string(),
                        trace_id: trace_id.clone(),
                        provider_id: Some(target.provider.as_str().to_string()),
                        upstream_model_id: Some(target.upstream_model.clone()),
                        stage: "failed".to_string(),
                        elapsed_ms: started.elapsed().as_millis() as u64,
                        connect_ms: None,
                        ttft_ms: None,
                        status_code: Some(504),
                        error: Some(format!("{err:?}")),
                        stop_reason: None,
                        timestamp: None,
                        endpoint_kind: Some(openproxy_types::EndpointKind::Image),
                    });
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
            ctx.circuit_breaker.record_success(CircuitBreakerKey::Account(account_id));
        }

        // Publish live log completed event
        openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
            request_id: request_id.to_string(),
            trace_id: trace_id.clone(),
            provider_id: Some(target.provider.as_str().to_string()),
            upstream_model_id: Some(target.upstream_model.clone()),
            stage: "completed".to_string(),
            elapsed_ms: started.elapsed().as_millis() as u64,
            connect_ms: None,
            ttft_ms: None,
            status_code: Some(status_code),
            error: None,
            stop_reason: None,
            timestamp: None,
            endpoint_kind: Some(openproxy_types::EndpointKind::Image),
        });

        // Record usage row in openproxy-db.
        let total_ms = started.elapsed().as_millis() as u64;
        record_image_usage_row(ImageUsageArgs {
            db_pool: ctx.db_pool,
            request_id,
            api_key_id,
            provider_id: &target.provider,
            account_id: target.account_id,
            combo_id: target.combo_id,
            combo_target_id: target.combo_target_id,
            model_row_id: target.model_row_id,
            upstream_model_id: &target.upstream_model,
            status_code,
            error_msg: None,
            total_ms,
        });

        tracing::info!("Image multipart request succeeded after {attempt} attempts, url={upstream_url}");
        return Ok(parsed_response);
    }

    Err(last_error.unwrap_or_else(|| CoreError::Internal("No valid targets found".into())))
}

pub async fn execute_image_edit(
    db_pool: &DbPool,
    adapters: &[ProviderAdapterEnum],
    upstream_client: &Arc<UpstreamClient>,
    circuit_breaker: &CircuitBreakerRegistry,
    master_key: &MasterKey,
    body: ParsedImageMultipartBody,
    api_key_id: Option<ApiKeyId>,
) -> Result<ImageGenerationResponse> {
    let ctx = ImageServiceContext {
        db_pool,
        adapters,
        upstream_client,
        circuit_breaker,
        master_key,
    };
    execute_image_multipart(&ctx, body, ImageMultipartKind::Edit, api_key_id).await
}

pub async fn execute_image_variation(
    db_pool: &DbPool,
    adapters: &[ProviderAdapterEnum],
    upstream_client: &Arc<UpstreamClient>,
    circuit_breaker: &CircuitBreakerRegistry,
    master_key: &MasterKey,
    body: ParsedImageMultipartBody,
    api_key_id: Option<ApiKeyId>,
) -> Result<ImageGenerationResponse> {
    let ctx = ImageServiceContext {
        db_pool,
        adapters,
        upstream_client,
        circuit_breaker,
        master_key,
    };
    execute_image_multipart(&ctx, body, ImageMultipartKind::Variation, api_key_id).await
}

#[derive(Debug, Deserialize)]
struct HordeAsyncSubmitResponse {
    id: Option<String>,
    message: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HordeCheckResponse {
    done: Option<bool>,
    finished: Option<u32>,
    faulted: Option<bool>,
    #[allow(dead_code)]
    wait_time: Option<u32>,
    #[allow(dead_code)]
    queue_position: Option<u32>,
    #[allow(dead_code)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HordeGenerationItem {
    #[allow(dead_code)]
    worker_id: Option<String>,
    worker_name: Option<String>,
    #[allow(dead_code)]
    model: Option<String>,
    state: Option<String>,
    img: Option<String>,
    censored: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct HordeStatusResponse {
    #[allow(dead_code)]
    done: Option<bool>,
    #[allow(dead_code)]
    faulted: Option<bool>,
    generations: Option<Vec<HordeGenerationItem>>,
    #[allow(dead_code)]
    error: Option<String>,
}

async fn cancel_horde_job(
    upstream_client: &Arc<UpstreamClient>,
    base_url: &str,
    job_id: &str,
    auth_headers: &[(String, String)],
) {
    let cancel_url = format!("{base_url}/generate/status/{job_id}");
    let mut del_req = UpstreamRequest::delete(&cancel_url);
    for (k, v) in auth_headers {
        if let (Ok(name), Ok(val)) = (
            http::header::HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            del_req.headers.insert(name, val);
        }
    }
    let _ = upstream_client
        .call(del_req, TimeoutProfile::Chat, CancellationToken::new())
        .await;
}

async fn poll_horde_image_generation(
    upstream_client: &Arc<UpstreamClient>,
    adapter: &ProviderAdapterEnum,
    api_key: &str,
    initial_body: &[u8],
    ctx: HordePollContext<'_>,
) -> Result<ImageGenerationResponse> {
    let submit_resp: HordeAsyncSubmitResponse = serde_json::from_slice(initial_body)
        .map_err(|e| CoreError::Parse(format!("failed to parse horde submit response: {e}")))?;

    let Some(job_id) = submit_resp.id else {
        let msg = submit_resp
            .message
            .or(submit_resp.error)
            .unwrap_or_else(|| "unknown horde submission error".to_string());
        return Err(CoreError::Validation(format!("horde submission failed: {msg}")));
    };

    let base_url = adapter.config().base_url.as_str();
    let check_url = format!("{base_url}/generate/check/{job_id}");
    let status_url = format!("{base_url}/generate/status/{job_id}");

    let auth_headers = adapter.build_headers(
        api_key,
        openproxy_types::TargetFormat::Openai,
        &openproxy_types::ModelId::new(""),
    );

    // If there are non-Horde fallback targets available, failover after 45s; otherwise wait up to 120s
    let timeout = if ctx.has_alternatives {
        std::time::Duration::from_secs(45)
    } else {
        std::time::Duration::from_secs(120)
    };
    let start = Instant::now();

    while start.elapsed() < timeout {
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

        let mut req = UpstreamRequest::get(&check_url);
        for (k, v) in &auth_headers {
            if let (Ok(name), Ok(val)) = (
                http::header::HeaderName::from_bytes(k.as_bytes()),
                HeaderValue::from_str(v),
            ) {
                req.headers.insert(name, val);
            }
        }

        let resp = upstream_client
            .call(req, TimeoutProfile::Chat, CancellationToken::new())
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("horde check error: {e:?}")))?;

        if resp.status.as_u16() != 200 {
            continue;
        }

        let body = resp
            .collect()
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("horde check body error: {e:?}")))?;

        let check: HordeCheckResponse = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("horde check parse error: {e}")))?;

        // Emit in-flight stage event to live logs
        openproxy_types::usage::publish_stage_event(openproxy_types::usage::StageEvent {
            request_id: ctx.request_id.to_string(),
            trace_id: ctx.trace_id.to_string(),
            provider_id: Some("horde".to_string()),
            upstream_model_id: None,
            stage: "waiting_upstream".to_string(),
            elapsed_ms: ctx.started.elapsed().as_millis() as u64,
            connect_ms: None,
            ttft_ms: None,
            status_code: Some(202),
            error: None,
            stop_reason: None,
            timestamp: None,
            endpoint_kind: Some(openproxy_types::EndpointKind::Image),
        });

        if check.faulted == Some(true) {
            cancel_horde_job(upstream_client, base_url, &job_id, &auth_headers).await;
            return Err(CoreError::UpstreamConnection("horde job faulted or worker unavailable".into()));
        }

        if check.done == Some(true) || check.finished.unwrap_or(0) > 0 {
            break;
        }
    }

    if start.elapsed() >= timeout {
        cancel_horde_job(upstream_client, base_url, &job_id, &auth_headers).await;
        return Err(CoreError::UpstreamTimeout {
            phase: "horde_polling".into(),
            ms: timeout.as_millis() as u64,
        });
    }

    // Fetch final status and images
    let mut req = UpstreamRequest::get(&status_url);
    for (k, v) in &auth_headers {
        if let (Ok(name), Ok(val)) = (
            http::header::HeaderName::from_bytes(k.as_bytes()),
            HeaderValue::from_str(v),
        ) {
            req.headers.insert(name, val);
        }
    }

    let resp = upstream_client
        .call(req, TimeoutProfile::Chat, CancellationToken::new())
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("horde status error: {e:?}")))?;

    let body = resp
        .collect()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("horde status body error: {e:?}")))?;

    let status: HordeStatusResponse = serde_json::from_slice(&body)
        .map_err(|e| CoreError::Parse(format!("horde status parse error: {e}")))?;

    let generations = status.generations.unwrap_or_default();
    if generations.is_empty() {
        cancel_horde_job(upstream_client, base_url, &job_id, &auth_headers).await;
        return Err(CoreError::UpstreamConnection(
            "horde returned no generations".into(),
        ));
    }

    let mut data = Vec::with_capacity(generations.len());
    for gen_item in generations {
        if gen_item.censored == Some(true) || gen_item.state.as_deref() == Some("censored") {
            cancel_horde_job(upstream_client, base_url, &job_id, &auth_headers).await;
            return Err(CoreError::Validation(format!(
                "generation censored by worker {}",
                gen_item.worker_name.as_deref().unwrap_or("unknown")
            )));
        }
        if gen_item.state.as_deref() == Some("csam") {
            cancel_horde_job(upstream_client, base_url, &job_id, &auth_headers).await;
            return Err(CoreError::Validation(
                "generation rejected: content safety violation (csam)".into(),
            ));
        }

        let Some(img) = gen_item.img else {
            continue;
        };

        if img.starts_with("http://") || img.starts_with("https://") {
            if ctx.response_format == Some("b64_json") {
                let dl_req = UpstreamRequest::get(&img);
                let dl_resp = upstream_client
                    .call(dl_req, TimeoutProfile::Chat, CancellationToken::new())
                    .await
                    .map_err(|e| {
                        CoreError::UpstreamConnection(format!("download image error: {e:?}"))
                    })?;
                let dl_bytes = dl_resp.collect().await.map_err(|e| {
                    CoreError::UpstreamConnection(format!("download image body error: {e:?}"))
                })?;
                use base64::Engine as _;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&dl_bytes);
                data.push(ImageData {
                    url: None,
                    b64_json: Some(b64),
                    revised_prompt: None,
                });
            } else {
                data.push(ImageData {
                    url: Some(img),
                    b64_json: None,
                    revised_prompt: None,
                });
            }
        } else if ctx.response_format == Some("url") {
            data.push(ImageData {
                url: Some(format!("data:image/webp;base64,{img}")),
                b64_json: None,
                revised_prompt: None,
            });
        } else {
            data.push(ImageData {
                url: None,
                b64_json: Some(img),
                revised_prompt: None,
            });
        }
    }

    if data.is_empty() {
        cancel_horde_job(upstream_client, base_url, &job_id, &auth_headers).await;
        return Err(CoreError::UpstreamConnection(
            "horde returned no valid images".into(),
        ));
    }

    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs()) as i64;

    Ok(ImageGenerationResponse { created, data })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_db as core_db;
    use std::path::PathBuf;

    fn fresh_pool() -> (core_db::DbPool, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir =
            std::env::temp_dir().join(format!("openproxy-image-test-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("state.db");
        let pool = core_db::DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            core_db::migrations::run(&mut w).expect("migrate");
        }
        (pool, dir)
    }

    #[test]
    fn test_resolve_image_targets_not_found() {
        let (pool, _dir) = fresh_pool();
        let plan = RoutingPlan::NotFound {
            model: "nonexistent-model".into(),
            hint: None,
        };
        let res = resolve_image_targets(&pool, plan, "nonexistent-model", None, Instant::now());
        assert!(matches!(res, Err(CoreError::ModelNotFound { .. })));
    }

    #[test]
    fn test_record_image_usage_row() {
        let (pool, _dir) = fresh_pool();
        let provider = ProviderId::new("openai");
        record_image_usage_row(ImageUsageArgs {
            db_pool: &pool,
            request_id: RequestId::new(),
            api_key_id: None,
            provider_id: &provider,
            account_id: None,
            combo_id: None,
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id: "dall-e-3",
            status_code: 200,
            error_msg: None,
            total_ms: 120,
        });

        let r = pool.reader();
        let count: i64 = r
            .query_row("SELECT COUNT(*) FROM usage WHERE endpoint_kind = 'image'", [], |row| {
                row.get(0)
            })
            .expect("query usage");
        assert_eq!(count, 1);
    }

    #[test]
    fn test_paeth_predictor() {
        assert_eq!(paeth_predictor(10, 10, 10), 10);
        assert_eq!(paeth_predictor(50, 100, 20), 100);
        assert_eq!(paeth_predictor(100, 50, 20), 100);
    }

    #[test]
    fn test_png_crc32() {
        // CRC32 of chunk b"IEND" with empty data
        let crc = png_crc32_chunk(*b"IEND", &[]);
        assert_eq!(crc, 0xAE42_6082);
    }

    fn create_test_rgba_png(width: u32, height: u32, pixels: &[[u8; 4]]) -> Vec<u8> {
        let mut raw_scanlines = Vec::new();
        for y in 0..height as usize {
            raw_scanlines.push(0u8); // filter 0
            for x in 0..width as usize {
                raw_scanlines.extend_from_slice(&pixels[y * width as usize + x]);
            }
        }
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(&raw_scanlines, 6);
        let mut out = Vec::new();
        out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.push(8);
        ihdr.push(6); // RGBA
        ihdr.push(0);
        ihdr.push(0);
        ihdr.push(0);
        write_png_chunk(&mut out, *b"IHDR", &ihdr);
        write_png_chunk(&mut out, *b"IDAT", &compressed);
        write_png_chunk(&mut out, *b"IEND", &[]);
        out
    }

    #[test]
    fn test_extract_png_alpha_mask_with_transparency() {
        // 2x2 image: top-left has alpha=0 (transparent), others have alpha=255 (opaque)
        let pixels = [
            [255, 0, 0, 0],     // transparent
            [0, 255, 0, 255],   // opaque
            [0, 0, 255, 255],   // opaque
            [255, 255, 0, 255], // opaque
        ];
        let png = create_test_rgba_png(2, 2, &pixels);
        let mask = extract_png_alpha_mask(&png);
        assert!(mask.is_some(), "expected mask to be extracted");

        let mask_bytes = mask.unwrap();
        assert!(mask_bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]));
    }

    #[test]
    fn test_extract_png_alpha_mask_opaque_returns_none() {
        // 2x2 image with no transparent pixels
        let pixels = [
            [255, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 0, 255],
        ];
        let png = create_test_rgba_png(2, 2, &pixels);
        let mask = extract_png_alpha_mask(&png);
        assert!(mask.is_none(), "opaque image should return None");
    }

    #[test]
    fn test_extract_png_alpha_mask_invalid_input() {
        assert!(extract_png_alpha_mask(b"").is_none());
        assert!(extract_png_alpha_mask(b"not a png image").is_none());
    }
}
