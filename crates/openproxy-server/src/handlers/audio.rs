//! `POST /v1/audio/transcriptions` — OpenAI-compatible Whisper endpoint.
//!
//! This is a *standalone* handler that does NOT route through the chat
//! [`Pipeline`](openproxy_pipeline::Pipeline). The pipeline is
//! deeply coupled to JSON request bodies, SSE streaming, token-based
//! usage, and retry/circuit-breaker semantics that don't fit the
//! multipart Whisper flow. Instead, the handler reuses:
//!
//! - **Auth**: the chat scope (any chat API key can transcribe), via
//!   [`crate::crate::middleware::auth::authenticate`].
//! - **Routing**: [`openproxy_core::routing::resolve`] to find the
//!   model. A model that matches a row in the `models` table goes
//!   direct; a `combo:<name>` matches a combo (the first model target
//!   is used); anything else is 404.
//! - **Adapter URL**: the provider adapter's
//!   [`ProviderAdapter::build_transcription_url`] for the upstream
//!   endpoint.
//! - **API key**: decrypted from the account row, mirroring the
//!   pipeline's `resolve_target_api_key` helper.
//!
//! The upstream call is dispatched via `UpstreamClient` directly (NOT via the
//! hyper-based `UpstreamClient`), so the 8 MiB response cap on
//! `UpstreamClient::call_inner` does not apply. `UpstreamClient` has
//! first-class `multipart::Form` support which simplifies the body
//! construction considerably.
//!
//! ## Usage recording
//!
//! A `usage` row is recorded best-effort with `prompt_tokens=None`,
//! `completion_tokens=None`, cost=0. Whisper bills by audio seconds
//! (not tokens); pricing can be layered in later by adding a
//! `audio_seconds` column and a per-model `Price::Audio` entry. For
//! now the row preserves the same shape as chat usage rows so the
//! dashboard's Live Logs tail and analytics queries see the request.
//!
//! ## Body size
//!
//! The default axum body limit of 32 MiB (set in `router.rs`) covers
//! Whisper's 25 MB upload ceiling; no per-route override is needed.

use axum::{
    extract::{Multipart, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    response::Response,
};
use openproxy_adapters::adapters;
use openproxy_core::{
    accounts, cost, models, providers,
    routing::{self, RoutingPlan},
};
use openproxy_pipeline::circuit_breaker::CircuitBreakerKey;
use openproxy_types::{
    CoreError,
    ids::{AccountId, ApiKeyId, ComboId, ModelRowId, ProviderId, RequestId, TraceId},
};
use std::time::Instant;

use crate::{error::ApiError, middleware::auth::authenticate, state::AppState};

/// `POST /v1/audio/transcriptions`.
///
/// See the module docs for the full design. The handler:
/// 1. Parses the multipart body (`file`, `model`, and optional fields).
/// 2. Authenticates via the chat scope.
/// 3. Resolves routing for the model.
/// 4. Looks up the adapter, builds the upstream URL, decrypts the API key.
/// 5. Forwards the request to the upstream via `UpstreamClient`.
/// 6. Returns the upstream response verbatim (body + Content-Type + status).
/// 7. Records a best-effort usage row.
pub async fn transcribe(
    State(state): State<AppState>,
    headers: HeaderMap,
    multipart: Multipart,
) -> Result<Response, ApiError> {
    let started = Instant::now();

    // 1. Parse the multipart body.
    let parsed_body = parse_multipart_body(multipart).await?;

    // 2. Authenticate (chat scope).
    let auth_result = authenticate(&state, &headers, &parsed_body.model_name)?;
    let api_key_id: Option<ApiKeyId> = auth_result.as_ref().map(|r| r.key_id);

    // 3. Resolve routing.
    let routing_plan = {
        let w = state.db_pool().writer();
        routing::resolve(&w, &parsed_body.model_name)?
    };

    if let RoutingPlan::Combo { combo_id, .. } = &routing_plan
        && let Some(auth) = &auth_result
        && let Some(allowed) = &auth.allowed_combos
        && !allowed.is_empty()
        && !allowed.contains(&combo_id.0)
    {
        return Err(ApiError(CoreError::Auth(
            "combo not allowed for this key".into(),
        )));
    }

    // 4. Resolve audio targets.
    let targets = resolve_audio_targets(&state, routing_plan, api_key_id, started)?;

    let adapters = state.adapters();
    let mut last_error = None;
    let mut attempt = 0;

    // 5. Multi-target dispatch loop
    for target in targets {
        attempt += 1;
        let adapter = match adapters
            .iter()
            .find(|a| a.id() == &target.provider_id)
            .cloned()
        {
            Some(a) => a,
            None => {
                last_error = Some(ApiError(CoreError::Internal(format!(
                    "no adapter registered for provider '{}'",
                    target.provider_id
                ))));
                continue;
            }
        };
        let upstream_url = adapter.build_transcription_url();

        let api_key = match resolve_api_key(&state, target.account_id, &target.provider_id) {
            Ok(k) => k,
            Err(e) => {
                last_error = Some(e);
                continue;
            }
        };

        // Clone the body for this attempt
        let body_clone = ParsedAudioBody {
            model_name: parsed_body.model_name.clone(),
            file_bytes: parsed_body.file_bytes.clone(),
            file_name: parsed_body.file_name.clone(),
            file_content_type: parsed_body.file_content_type.clone(),
            form_fields: parsed_body.form_fields.clone(),
        };

        // Dispatch
        let response = match dispatch_audio_request(
            &state,
            adapter.clone(),
            &upstream_url,
            &api_key,
            &target.upstream_model_id,
            body_clone,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                if let Some(account_id) = target.account_id {
                    state
                        .circuit_breaker()
                        .record_failure(CircuitBreakerKey::Account(account_id));
                }
                tracing::warn!(
                    "Audio target failed (connection error): provider={}, error={:?}",
                    target.provider_id,
                    e
                );
                last_error = Some(e);
                continue;
            }
        };

        let status_code = response.status;
        let content_type = response
            .headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/json")
            .to_string();

        let body_bytes = match response.collect().await {
            Ok(b) => b,
            Err(e) => {
                let err = ApiError(CoreError::UpstreamConnection(format!("read body: {:?}", e)));
                if let Some(account_id) = target.account_id {
                    state
                        .circuit_breaker()
                        .record_failure(CircuitBreakerKey::Account(account_id));
                }
                tracing::warn!(
                    "Audio target body read failed: provider={}, error={:?}",
                    target.provider_id,
                    err
                );
                last_error = Some(err);
                continue;
            }
        };

        if status_code.as_u16() >= 400 {
            if let Some(account_id) = target.account_id {
                state
                    .circuit_breaker()
                    .record_failure(CircuitBreakerKey::Account(account_id));
            }
            tracing::warn!(
                "Audio target returned error status: provider={}, status={}",
                target.provider_id,
                status_code
            );
            last_error = Some(ApiError(CoreError::UpstreamConnection(format!(
                "upstream status {}",
                status_code
            ))));
            continue;
        }

        // Success! Record usage and return.
        let total_ms = started.elapsed().as_millis() as u64;
        let _ = record_audio_usage_row(AudioUsageArgs {
            state: &state,
            request_id: RequestId::new(),
            api_key_id,
            provider_id: &target.provider_id,
            account_id: target.account_id,
            combo_id: target.combo_id,
            model_row_id: target.model_row_id,
            upstream_model_id: &target.upstream_model_id,
            status_code: status_code.as_u16(),
            error_msg: None,
            total_ms,
        });

        tracing::info!("Audio request succeeded after {} attempts", attempt);
        return build_audio_response(status_code.as_u16(), &content_type, body_bytes);
    }

    Err(last_error
        .unwrap_or_else(|| ApiError(CoreError::Internal("No valid targets found".into()))))
}

struct ParsedAudioBody {
    model_name: String,
    file_bytes: bytes::Bytes,
    file_name: String,
    file_content_type: String,
    form_fields: Vec<(String, String)>,
}

async fn parse_multipart_body(mut multipart: Multipart) -> Result<ParsedAudioBody, ApiError> {
    let mut model_name = String::new();
    let mut file_bytes: Option<bytes::Bytes> = None;
    let mut file_name = String::from("audio");
    let mut file_content_type = String::from("application/octet-stream");
    let mut form_fields: Vec<(String, String)> = Vec::new();

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError(CoreError::Validation(format!("multipart parse: {e}"))))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "model" => {
                model_name = field.text().await.unwrap_or_default();
            }
            "file" => {
                file_name = field.file_name().unwrap_or("audio").to_string();
                file_content_type = field
                    .content_type()
                    .unwrap_or("application/octet-stream")
                    .to_string();
                file_bytes = Some(field.bytes().await.unwrap_or_default());
            }
            _ => {
                let value = field.text().await.unwrap_or_default();
                form_fields.push((name, value));
            }
        }
    }

    let file_bytes = file_bytes.ok_or_else(|| {
        ApiError(CoreError::Validation(
            "missing 'file' part in multipart body".into(),
        ))
    })?;
    if file_bytes.is_empty() {
        return Err(ApiError(CoreError::Validation(
            "empty 'file' part in multipart body".into(),
        )));
    }
    if model_name.is_empty() {
        return Err(ApiError(CoreError::Validation(
            "missing 'model' field in multipart body".into(),
        )));
    }

    Ok(ParsedAudioBody {
        model_name,
        file_bytes,
        file_name,
        file_content_type,
        form_fields,
    })
}

struct AudioTargets {
    provider_id: ProviderId,
    account_id: Option<AccountId>,
    model_row_id: Option<ModelRowId>,
    upstream_model_id: String,
    combo_id: Option<ComboId>,
}

fn resolve_audio_targets(
    state: &AppState,
    routing_plan: RoutingPlan,
    api_key_id: Option<ApiKeyId>,
    started: Instant,
) -> Result<Vec<AudioTargets>, ApiError> {
    match routing_plan {
        RoutingPlan::Combo {
            combo_id, targets, ..
        } => {
            let r = state.db_pool().reader();
            let targets = openproxy_core::routing::flatten_targets(&r, targets).map_err(|e| {
                ApiError(CoreError::Validation(format!(
                    "flatten_targets failed: {}",
                    e
                )))
            })?;
            let targets =
                openproxy_core::routing::expand_account_rotation(&r, targets).map_err(|e| {
                    ApiError(CoreError::Validation(format!(
                        "expand_account_rotation failed: {}",
                        e
                    )))
                })?;

            let mut audio_targets = Vec::with_capacity(targets.len());
            for target in targets {
                if let Some(model_row_id) = target.model_row_id {
                    let (provider_id, upstream_model_id) = {
                        let model = match models::get_by_row_id(&r, model_row_id) {
                            Ok(Some(m)) => m,
                            _ => continue, // skip invalid models
                        };
                        (model.provider_id, model.model_id.as_str().to_string())
                    };
                    audio_targets.push(AudioTargets {
                        provider_id,
                        account_id: target.account_id,
                        model_row_id: Some(model_row_id),
                        upstream_model_id,
                        combo_id: Some(combo_id),
                    });
                }
            }
            if audio_targets.is_empty() {
                return Err(ApiError(CoreError::Validation(
                    "combo has no model target suitable for transcription".into(),
                )));
            }
            Ok(audio_targets)
        }
        RoutingPlan::NotFound { model, hint } => {
            let _ = record_audio_usage_row(AudioUsageArgs {
                state,
                request_id: RequestId::new(),
                api_key_id,
                provider_id: &ProviderId::new(""),
                account_id: None,
                combo_id: None,
                model_row_id: None,
                upstream_model_id: &model,
                status_code: 404,
                error_msg: Some("model_not_found".to_string()),
                total_ms: started.elapsed().as_millis() as u64,
            });
            let mut msg = format!("model not found: {}", model);
            if let Some(h) = hint {
                msg.push_str(&format!(" (hint: {})", h));
            }
            Err(ApiError(CoreError::ModelNotFound {
                provider: "<unknown>".into(),
                model: msg,
            }))
        }
    }
}

async fn dispatch_audio_request(
    state: &AppState,
    adapter: adapters::ProviderAdapterEnum,
    upstream_url: &str,
    api_key: &str,
    upstream_model_id: &str,
    body: ParsedAudioBody,
) -> Result<openproxy_adapters::upstream::UpstreamResponse, ApiError> {
    let Some((auth_name, auth_value)) = adapter.build_auth_header(api_key) else {
        return Err(ApiError(CoreError::Validation("Invalid API Key".into())));
    };

    let boundary = format!("----WebKitFormBoundary{}", uuid::Uuid::new_v4().simple());
    let mut payload = Vec::new();

    // model field
    payload.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    payload.extend_from_slice(b"Content-Disposition: form-data; name=\"model\"\r\n\r\n");
    payload.extend_from_slice(upstream_model_id.as_bytes());
    payload.extend_from_slice(b"\r\n");

    // form fields
    for (k, v) in &body.form_fields {
        payload.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
        payload.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{}\"\r\n\r\n", k).as_bytes(),
        );
        payload.extend_from_slice(v.as_bytes());
        payload.extend_from_slice(b"\r\n");
    }

    // file field
    payload.extend_from_slice(format!("--{}\r\n", boundary).as_bytes());
    payload.extend_from_slice(
        format!(
            "Content-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\n",
            body.file_name
        )
        .as_bytes(),
    );
    payload
        .extend_from_slice(format!("Content-Type: {}\r\n\r\n", body.file_content_type).as_bytes());
    payload.extend_from_slice(&body.file_bytes);
    payload.extend_from_slice(b"\r\n");

    // end
    payload.extend_from_slice(format!("--{}--\r\n", boundary).as_bytes());

    let content_type = format!("multipart/form-data; boundary={}", boundary);
    let mut req = openproxy_adapters::upstream::UpstreamRequest::post_multipart(
        upstream_url,
        content_type,
        bytes::Bytes::from(payload),
    );

    if !auth_name.is_empty()
        && let Ok(k) = axum::http::HeaderName::from_bytes(auth_name.as_bytes())
        && let Ok(v) = axum::http::HeaderValue::from_str(&auth_value)
    {
        req.headers.insert(k, v);
    }
    for (k, v) in &adapter.config().extra_headers {
        if let Ok(hn) = axum::http::HeaderName::from_bytes(k.as_bytes())
            && let Ok(hv) = axum::http::HeaderValue::from_str(v)
        {
            req.headers.insert(hn, hv);
        }
    }

    let client = state.upstream_client();
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    client
        .call(
            req,
            openproxy_adapters::upstream::TimeoutProfile::Quota,
            cancel,
        )
        .await
        .map_err(|e| {
            ApiError(CoreError::UpstreamConnection(format!(
                "{}: {:?}",
                upstream_url, e
            )))
        })
}

fn build_audio_response(
    status_code: u16,
    content_type: &str,
    body: bytes::Bytes,
) -> Result<Response, ApiError> {
    let mut builder = Response::builder()
        .status(StatusCode::from_u16(status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR));
    if let Ok(v) = HeaderValue::from_str(content_type) {
        builder = builder.header(axum::http::header::CONTENT_TYPE, v);
    }
    Ok(builder
        .body(axum::body::Body::from(body))
        .unwrap_or_else(|_| {
            let mut res = Response::new(axum::body::Body::empty());
            *res.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            res
        }))
}

/// Resolve the upstream API key for an audio transcription request.
///
/// - `account_id = Some(_)`: decrypt the stored key for that account
///   (using the reader to avoid serializing through the writer mutex).
/// - `account_id = None` and the provider's `auth_type` is `None`:
///   return an empty string (anonymous access, e.g. a local Ollama
///   endpoint with no auth).
/// - `account_id = None` and the provider requires auth (Bearer,
///   XApiKey, etc.): return `CoreError::Auth` — the routing layer
///   didn't find a healthy account, and we have no credential to fall
///   back to.
fn resolve_api_key(
    state: &AppState,
    account_id: Option<AccountId>,
    provider_id: &ProviderId,
) -> Result<String, ApiError> {
    match account_id {
        Some(id) => {
            // SELECT by id — use the reader so we don't serialize
            // through the writer mutex (the chat hot path holds the
            // writer for routing resolution; we don't want to block on
            // it for a key read).
            let r = state.db_pool().reader();
            accounts::decrypt_api_key(&r, id, state.master_key()).map_err(ApiError)
        }
        None => {
            let r = state.db_pool().reader();
            match providers::get(&r, provider_id).map_err(ApiError)? {
                Some(p) if matches!(p.auth_type, providers::AuthType::None) => Ok(String::new()),
                _ => Err(ApiError(CoreError::Auth(format!(
                    "no healthy account with credentials for provider '{}'",
                    provider_id
                )))),
            }
        }
    }
}

/// Record a single best-effort `usage` row for an audio request.
///
/// Mirrors the chat handler's `record_model_not_found_usage_row` helper:
/// `prompt_tokens=None`, `completion_tokens=None`, `race_total=1`,
/// `race_lost=false`, `race_attempts=1`, `attempt=1`. The pricing layer
/// computes `cost_usd = 0` for `None`+`None` token inputs; Whisper
/// bills by audio seconds (not tokens), so the row's cost is always 0
/// until audio-seconds pricing is layered in.
///
/// Uses `try_writer_for(100ms)` so a long-running admin write cannot
/// stall the audio response — if the writer lock can't be acquired in
/// 100ms, the row is dropped (logged at WARN) and the request still
/// returns successfully. This matches the chat handler's MEDIUM-5 fix.
struct AudioUsageArgs<'a> {
    state: &'a AppState,
    request_id: RequestId,
    api_key_id: Option<ApiKeyId>,
    provider_id: &'a ProviderId,
    account_id: Option<AccountId>,
    combo_id: Option<ComboId>,
    model_row_id: Option<ModelRowId>,
    upstream_model_id: &'a str,
    status_code: u16,
    error_msg: Option<String>,
    total_ms: u64,
}

fn record_audio_usage_row(args: AudioUsageArgs<'_>) -> Result<(), ApiError> {
    let AudioUsageArgs {
        state,
        request_id,
        api_key_id,
        provider_id,
        account_id,
        combo_id,
        model_row_id,
        upstream_model_id,
        status_code,
        error_msg,
        total_ms,
    } = args;
    use openproxy_types::UsageInput;
    let input = UsageInput {
        proxy_url: None,
        proxy_status: None,
        is_proxy_rotated: false,
        request_id,
        trace_id: TraceId::new().to_string(),
        attempt: 1,
        provider_id: provider_id.clone(),
        account_id,
        combo_id,
        combo_target_id: None,
        model_row_id,
        upstream_model_id: upstream_model_id.to_string(),
        // Whisper bills by audio seconds, not tokens. Record None for
        // both so the cost computes to 0; pricing can be layered in
        // later by extending the pricing table with a per-model
        // audio-seconds rate.
        prompt_tokens: None,
        completion_tokens: None,
        cached_tokens: None,
        connect_ms: None,
        ttft_ms: None,
        total_ms,
        status_code,
        error_msg: error_msg.clone(),
        race_total: 1,
        race_lost: false,
        api_key_id,
        request_body_json: None,
        response_body_json: None,
        request_headers: None,
        response_headers: None,
        error_message: error_msg,
        race_attempts: 1,
        is_streaming: false,
        stream_complete: false,
        stop_reason: None,
        compression_savings_pct: None,
        compression_techniques: None,
        // The audio response was actually delivered to the HTTP client.
        client_response: true,
        prompt_tokens_estimated: false,
        completion_tokens_estimated: false,
        endpoint_kind: openproxy_types::EndpointKind::Audio,
    };
    let w = match state
        .db_pool()
        .try_writer_for(std::time::Duration::from_millis(100))
    {
        Some(w) => w,
        None => {
            tracing::warn!("hot-path writer lock timeout on audio usage row; dropping");
            return Ok(());
        }
    };
    let _ = cost::record(&w, &input).map_err(ApiError);
    Ok(())
}
