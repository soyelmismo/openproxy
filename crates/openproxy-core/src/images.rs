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

    if let Some((auth_name, auth_value)) = adapter.build_auth_header(api_key)
        && !auth_name.is_empty()
        && let Ok(k) = axum::http::HeaderName::from_bytes(auth_name.as_bytes())
        && let Ok(v) = axum::http::HeaderValue::from_str(&auth_value)
    {
        upstream_req.headers.insert(k, v);
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

    let mut last_error = None;
    let mut attempt = 0;

    // 3. Multi-target dispatch loop.
    for target in targets {
        attempt += 1;

        if !is_target_available(
            db_pool,
            circuit_breaker,
            target.account_id,
            target.combo_target_id,
        ) {
            continue;
        }

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
        let response = match dispatch_image_request(
            upstream_client,
            &adapter,
            &upstream_url,
            &api_key,
            &target.upstream_model,
            &req,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                if let Some(account_id) = target.account_id {
                    circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
                }
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
            tracing::warn!(
                "Image target returned error status: provider={}, status={}, body={}",
                target.provider,
                status_code,
                err_text
            );
            last_error = Some(CoreError::UpstreamConnection(format!(
                "upstream status {status_code}: {err_text}"
            )));
            continue;
        }

        // Parse upstream response into standard ImageGenerationResponse.
        let parsed_response: ImageGenerationResponse = if target.provider.as_str() == "horde" || status_code == 202 {
            match poll_horde_image_generation(
                upstream_client,
                &adapter,
                &api_key,
                &body_bytes,
                req.response_format.as_deref(),
            )
            .await
            {
                Ok(res) => res,
                Err(e) => {
                    let err = CoreError::UpstreamConnection(format!("horde generation error: {e}"));
                    if let Some(account_id) = target.account_id {
                        circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
                    }
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

        // Record usage row in openproxy-db.
        let total_ms = started.elapsed().as_millis() as u64;
        record_image_usage_row(ImageUsageArgs {
            db_pool,
            request_id: RequestId::new(),
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

    if let Some((auth_name, auth_value)) = adapter.build_auth_header(api_key)
        && !auth_name.is_empty()
        && let Ok(k) = axum::http::HeaderName::from_bytes(auth_name.as_bytes())
        && let Ok(v) = axum::http::HeaderValue::from_str(&auth_value)
    {
        upstream_req.headers.insert(k, v);
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

    let mut last_error = None;
    let mut attempt = 0;

    // 3. Multi-target dispatch loop.
    for target in targets {
        attempt += 1;

        if !is_target_available(
            ctx.db_pool,
            ctx.circuit_breaker,
            target.account_id,
            target.combo_target_id,
        ) {
            continue;
        }

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
        let upstream_url = match kind {
            ImageMultipartKind::Edit => adapter.build_image_edits_url(),
            ImageMultipartKind::Variation => adapter.build_image_variations_url(),
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

        // Dispatch upstream.
        let response = match dispatch_image_multipart_request(
            ctx.upstream_client,
            &adapter,
            &upstream_url,
            &api_key,
            &target.upstream_model,
            &body,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                if let Some(account_id) = target.account_id {
                    ctx.circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
                }
                tracing::warn!(
                    "Image multipart target failed (connection error): provider={}, error={:?}",
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
                    ctx.circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
                }
                tracing::warn!(
                    "Image multipart target body read failed: provider={}, error={:?}",
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
            tracing::warn!(
                "Image multipart target returned error status: provider={}, status={}, body={}",
                target.provider,
                status_code,
                err_text
            );
            last_error = Some(CoreError::UpstreamConnection(format!(
                "upstream status {status_code}: {err_text}"
            )));
            continue;
        }

        // Parse upstream response into standard ImageGenerationResponse.
        let parsed_response: ImageGenerationResponse = match serde_json::from_slice(&body_bytes) {
            Ok(res) => res,
            Err(e) => {
                let err = CoreError::Parse(format!("failed to parse image response: {e}"));
                last_error = Some(err);
                continue;
            }
        };

        if let Some(account_id) = target.account_id {
            ctx.circuit_breaker.record_success(CircuitBreakerKey::Account(account_id));
        }

        // Record usage row in openproxy-db.
        let total_ms = started.elapsed().as_millis() as u64;
        record_image_usage_row(ImageUsageArgs {
            db_pool: ctx.db_pool,
            request_id: RequestId::new(),
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

        tracing::info!("Image multipart request succeeded after {attempt} attempts");
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

async fn poll_horde_image_generation(
    upstream_client: &Arc<UpstreamClient>,
    adapter: &ProviderAdapterEnum,
    api_key: &str,
    initial_body: &[u8],
    response_format: Option<&str>,
) -> Result<ImageGenerationResponse> {
    let submit_resp: HordeAsyncSubmitResponse = serde_json::from_slice(initial_body)
        .map_err(|e| CoreError::Parse(format!("failed to parse horde submit response: {e}")))?;

    let Some(job_id) = submit_resp.id else {
        return Err(CoreError::UpstreamConnection(format!(
            "horde did not return a job ID: {:?}",
            submit_resp.message.or(submit_resp.error)
        )));
    };

    let base_url = adapter.config().base_url.as_str();
    let check_url = format!("{base_url}/generate/check/{job_id}");
    let status_url = format!("{base_url}/generate/status/{job_id}");

    let auth_headers = adapter.build_headers(
        api_key,
        openproxy_types::TargetFormat::Openai,
        &openproxy_types::ModelId::new(""),
    );

    let timeout = std::time::Duration::from_secs(120);
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

        if check.faulted == Some(true) {
            return Err(CoreError::UpstreamConnection("horde job faulted".into()));
        }

        if check.done == Some(true) || check.finished.unwrap_or(0) > 0 {
            break;
        }
    }

    if start.elapsed() >= timeout {
        let mut del_req = UpstreamRequest::delete(&status_url);
        for (k, v) in &auth_headers {
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
        return Err(CoreError::UpstreamTimeout {
            phase: "horde_polling".into(),
            ms: 120_000,
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
        return Err(CoreError::UpstreamConnection(
            "horde returned no generations".into(),
        ));
    }

    let mut data = Vec::with_capacity(generations.len());
    for gen_item in generations {
        if gen_item.censored == Some(true) || gen_item.state.as_deref() == Some("censored") {
            return Err(CoreError::UpstreamConnection(format!(
                "generation censored by worker {}",
                gen_item.worker_name.as_deref().unwrap_or("unknown")
            )));
        }
        if gen_item.state.as_deref() == Some("csam") {
            return Err(CoreError::UpstreamConnection(
                "generation rejected (csam filter)".into(),
            ));
        }

        let Some(img) = gen_item.img else {
            continue;
        };

        if img.starts_with("http://") || img.starts_with("https://") {
            if response_format == Some("b64_json") {
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
        } else {
            data.push(ImageData {
                url: None,
                b64_json: Some(img),
                revised_prompt: None,
            });
        }
    }

    if data.is_empty() {
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
}
