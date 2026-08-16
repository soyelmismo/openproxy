//! Audio transcription service: resolution, dispatch, and usage recording.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest, UpstreamResponse,
};
use openproxy_db::DbPool;
use openproxy_db::secrets::MasterKey;
use openproxy_pipeline::circuit_breaker::{CircuitBreakerKey, CircuitBreakerRegistry};
use openproxy_types::{
    CoreError, Result,
    ids::{AccountId, ApiKeyId, ComboId, ModelRowId, ProviderId, RequestId},
};

use crate::routing::{self, RoutingPlan};

#[derive(Clone, Debug)]
pub struct ParsedAudioBody {
    pub model_name: String,
    pub file_bytes: Bytes,
    pub file_name: String,
    pub file_content_type: String,
    pub form_fields: Vec<(String, String)>,
}

pub struct AudioTranscriptionResponse {
    pub status_code: u16,
    pub content_type: String,
    pub body_bytes: Bytes,
}

pub use crate::unary::{
    UnaryTarget as AudioTargets, is_target_available, record_unary_usage, resolve_api_key,
    resolve_unary_targets,
};

pub fn resolve_audio_targets(
    db_pool: &DbPool,
    routing_plan: RoutingPlan,
    api_key_id: Option<ApiKeyId>,
    started: Instant,
) -> Result<Vec<AudioTargets>> {
    resolve_unary_targets(
        db_pool,
        routing_plan,
        "audio",
        openproxy_types::EndpointKind::Audio,
        api_key_id,
        started,
    )
}

pub async fn dispatch_audio_request(
    upstream_client: &Arc<UpstreamClient>,
    adapter: ProviderAdapterEnum,
    upstream_url: &str,
    api_key: &str,
    upstream_model_id: &str,
    body: ParsedAudioBody,
) -> Result<UpstreamResponse> {
    let Some((auth_name, auth_value)) = adapter.build_auth_header(api_key) else {
        return Err(CoreError::Validation("Invalid API Key".into()));
    };

    let boundary = format!("----WebKitFormBoundary{}", uuid::Uuid::new_v4().simple());
    let mut payload = Vec::new();

    // model field
    payload.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    payload.extend_from_slice(b"Content-Disposition: form-data; name=\"model\"\r\n\r\n");
    payload.extend_from_slice(upstream_model_id.as_bytes());
    payload.extend_from_slice(b"\r\n");

    // form fields
    for (k, v) in &body.form_fields {
        payload.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
        payload.extend_from_slice(
            format!("Content-Disposition: form-data; name=\"{k}\"\r\n\r\n").as_bytes(),
        );
        payload.extend_from_slice(v.as_bytes());
        payload.extend_from_slice(b"\r\n");
    }

    // file field
    payload.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
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
    payload.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());

    let content_type = format!("multipart/form-data; boundary={boundary}");
    let mut req = UpstreamRequest::post_multipart(
        upstream_url,
        &content_type,
        Bytes::from(payload),
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

    let cancel = CancellationToken::new();
    upstream_client
        .call(
            req,
            TimeoutProfile::Quota,
            cancel,
        )
        .await
        .map_err(|e| {
            CoreError::UpstreamConnection(format!(
                "{upstream_url}: {e:?}"
            ))
        })
}

pub struct AudioUsageArgs<'a> {
    pub db_pool: &'a DbPool,
    pub request_id: RequestId,
    pub api_key_id: Option<ApiKeyId>,
    pub provider_id: &'a ProviderId,
    pub account_id: Option<AccountId>,
    pub combo_id: Option<ComboId>,
    pub model_row_id: Option<ModelRowId>,
    pub upstream_model_id: &'a str,
    pub status_code: u16,
    pub error_msg: Option<String>,
    pub total_ms: u64,
}

pub fn record_audio_usage_row(args: AudioUsageArgs<'_>) {
    record_unary_usage(
        args.db_pool,
        &crate::unary::UnaryUsageArgs {
            request_id: args.request_id,
            api_key_id: args.api_key_id,
            provider_id: args.provider_id,
            account_id: args.account_id,
            combo_id: args.combo_id,
            combo_target_id: None,
            model_row_id: args.model_row_id,
            upstream_model_id: args.upstream_model_id,
            prompt_tokens: None,
            completion_tokens: None,
            status_code: args.status_code,
            error_msg: args.error_msg,
            total_ms: args.total_ms,
            endpoint_kind: openproxy_types::EndpointKind::Audio,
        },
    );
}

pub async fn execute_transcribe(
    db_pool: &DbPool,
    adapters: &[ProviderAdapterEnum],
    upstream_client: &Arc<UpstreamClient>,
    circuit_breaker: &CircuitBreakerRegistry,
    master_key: &MasterKey,
    parsed_body: ParsedAudioBody,
    api_key_id: Option<ApiKeyId>,
) -> Result<AudioTranscriptionResponse> {
    let started = Instant::now();

    // 1. Resolve routing.
    let routing_plan = {
        let r = db_pool.reader();
        routing::resolve(&r, &parsed_body.model_name)?
    };

    // 2. Resolve audio targets.
    let targets = resolve_audio_targets(db_pool, routing_plan, api_key_id, started)?;

    let mut last_error = None;
    let mut attempt = 0;

    // 3. Multi-target dispatch loop
    for target in targets {
        attempt += 1;
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
        let upstream_url = adapter.build_transcription_url();

        let api_key = match resolve_api_key(db_pool, master_key, target.account_id, &target.provider) {
            Ok(k) => k,
            Err(e) => {
                last_error = Some(e);
                continue;
            }
        };

        let body_clone = parsed_body.clone();

        let response = match dispatch_audio_request(
            upstream_client,
            ProviderAdapterEnum::clone(&adapter),
            &upstream_url,
            &api_key,
            &target.upstream_model,
            body_clone,
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                if let Some(account_id) = target.account_id {
                    circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
                }
                tracing::warn!(
                    "Audio target failed (connection error): provider={}, error={:?}",
                    target.provider,
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
                let err = CoreError::UpstreamConnection(format!("read body: {e:?}"));
                if let Some(account_id) = target.account_id {
                    circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
                }
                tracing::warn!(
                    "Audio target body read failed: provider={}, error={:?}",
                    target.provider,
                    err
                );
                last_error = Some(err);
                continue;
            }
        };

        if status_code.as_u16() >= 400 {
            if let Some(account_id) = target.account_id {
                circuit_breaker.record_failure(CircuitBreakerKey::Account(account_id));
            }
            tracing::warn!(
                "Audio target returned error status: provider={}, status={}",
                target.provider,
                status_code
            );
            last_error = Some(CoreError::UpstreamConnection(format!(
                "upstream status {status_code}"
            )));
            continue;
        }

        // Success! Record usage and return.
        let total_ms = started.elapsed().as_millis() as u64;
        record_audio_usage_row(AudioUsageArgs {
            db_pool,
            request_id: RequestId::new(),
            api_key_id,
            provider_id: &target.provider,
            account_id: target.account_id,
            combo_id: target.combo_id,
            model_row_id: target.model_row_id,
            upstream_model_id: &target.upstream_model,
            status_code: status_code.as_u16(),
            error_msg: None,
            total_ms,
        });

        tracing::info!("Audio request succeeded after {} attempts", attempt);
        return Ok(AudioTranscriptionResponse {
            status_code: status_code.as_u16(),
            content_type,
            body_bytes,
        });
    }

    Err(last_error.unwrap_or_else(|| CoreError::Internal("No valid targets found".into())))
}
