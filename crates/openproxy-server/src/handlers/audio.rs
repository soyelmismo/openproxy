//! `POST /v1/audio/transcriptions` — OpenAI-compatible Whisper endpoint.
//!
//! This is a *standalone* handler that does NOT route through the chat
//! [`Pipeline`](openproxy_core::pipeline::Pipeline). The pipeline is
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
use openproxy_core::{
    CoreError, accounts, adapters,
    adapters::ProviderAdapter,
    cost,
    ids::{AccountId, ApiKeyId, ComboId, ModelRowId, ProviderId, RequestId, TraceId},
    models, providers,
    routing::{self, RoutingPlan},
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

    // 4. Translate routing plan.
    let targets = match translate_audio_routing_plan(&state, routing_plan, api_key_id, started)? {
        Some(t) => t,
        None => {
            // Already handled by error or 404 in translate helper.
            unreachable!()
        }
    };

    // 5. Look up the adapter and build URL.
    let adapter = state
        .adapters()
        .into_iter()
        .find(|a| a.id() == &targets.provider_id)
        .ok_or_else(|| {
            ApiError(CoreError::Internal(format!(
                "no adapter registered for provider '{}'",
                targets.provider_id
            )))
        })?;
    let upstream_url = adapter.build_transcription_url();

    // 6. Resolve the API key.
    let api_key = resolve_api_key(&state, targets.account_id, &targets.provider_id)?;

    // 7. Build and dispatch.
    let response = dispatch_audio_request(
        &state,
        adapter,
        &upstream_url,
        &api_key,
        &targets.upstream_model_id,
        parsed_body,
    )
    .await?;

    let status_code = response.status;
    let content_type = response
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();
    let body_bytes = response
        .collect()
        .await
        .map_err(|e| ApiError(CoreError::UpstreamConnection(format!("read body: {:?}", e))))?;

    let total_ms = started.elapsed().as_millis() as u64;
    let error_msg = if status_code.as_u16() < 400 {
        None
    } else {
        Some(format!("upstream status {}", status_code))
    };

    // 9. Record usage.
    let _ = record_audio_usage_row(AudioUsageArgs {
        state: &state,
        request_id: RequestId::new(),
        api_key_id,
        provider_id: &targets.provider_id,
        account_id: targets.account_id,
        combo_id: targets.combo_id,
        model_row_id: targets.model_row_id,
        upstream_model_id: &targets.upstream_model_id,
        status_code: status_code.as_u16(),
        error_msg,
        total_ms,
    });

    // 10. Return response.
    build_audio_response(status_code.as_u16(), &content_type, body_bytes)
}

struct ParsedAudioBody {
    model_name: String,
    file_bytes: Vec<u8>,
    file_name: String,
    file_content_type: String,
    form_fields: Vec<(String, String)>,
}

async fn parse_multipart_body(mut multipart: Multipart) -> Result<ParsedAudioBody, ApiError> {
    let mut model_name = String::new();
    let mut file_bytes: Option<Vec<u8>> = None;
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
                file_bytes = Some(field.bytes().await.unwrap_or_default().to_vec());
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

fn translate_audio_routing_plan(
    state: &AppState,
    routing_plan: RoutingPlan,
    api_key_id: Option<ApiKeyId>,
    started: Instant,
) -> Result<Option<AudioTargets>, ApiError> {
    match routing_plan {
        RoutingPlan::Direct {
            provider_id,
            account_id,
            model_row_id,
            model_id,
        } => Ok(Some(AudioTargets {
            provider_id,
            account_id,
            model_row_id: Some(model_row_id),
            upstream_model_id: model_id,
            combo_id: None,
        })),
        RoutingPlan::Combo {
            combo_id, targets, ..
        } => {
            let target = targets
                .into_iter()
                .find(|t| t.model_row_id.is_some())
                .ok_or_else(|| {
                    ApiError(CoreError::Validation(
                        "combo has no model target suitable for transcription".into(),
                    ))
                })?;
            let model_row_id = target.model_row_id.expect("checked above");
            let (provider_id, upstream_model_id) = {
                let r = state.db_pool().reader();
                let model = models::get_by_row_id(&r, model_row_id)
                    .map_err(ApiError)?
                    .ok_or_else(|| {
                        ApiError(CoreError::ModelNotFound {
                            provider: target.provider_id.to_string(),
                            model: format!("row_id={}", model_row_id.0),
                        })
                    })?;
                (model.provider_id, model.model_id.as_str().to_string())
            };
            Ok(Some(AudioTargets {
                provider_id,
                account_id: target.account_id,
                model_row_id: Some(model_row_id),
                upstream_model_id,
                combo_id: Some(combo_id),
            }))
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
) -> Result<openproxy_core::upstream::UpstreamResponse, ApiError> {
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
    let mut req = openproxy_core::upstream::UpstreamRequest::post_multipart(
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
    let cancel = openproxy_core::upstream::CancellationToken::new();
    client
        .call(req, openproxy_core::upstream::TimeoutProfile::Quota, cancel)
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
    use openproxy_core::cost::UsageInput;
    let input = UsageInput {
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
        endpoint_kind: openproxy_core::endpoint::EndpointKind::Audio,
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
