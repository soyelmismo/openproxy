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
use openproxy_pipeline::circuit_breaker::CircuitBreakerRegistry;
use openproxy_types::{
    CoreError, Result,
    ids::{ApiKeyId, RequestId},
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
    UnaryTarget as AudioTargets, UnaryTarget, UnaryUsageArgs, apply_adapter_headers,
    is_target_available, map_upstream_status_error, record_unary_usage, resolve_api_key,
    resolve_unary_targets,
};

pub type AudioUsageArgs<'a> = UnaryUsageArgs<'a>;

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
    if adapter.build_auth_header(api_key).is_none() {
        return Err(CoreError::Validation("Invalid API Key".into()));
    }

    let boundary = format!("----WebKitFormBoundary{}", uuid::Uuid::new_v4().simple());
    let mut payload = Vec::new();

    // model field
    payload.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    payload.extend_from_slice(b"Content-Disposition: form-data; name=\"model\"\r\n\r\n");
    payload.extend_from_slice(upstream_model_id.as_bytes());
    payload.extend_from_slice(b"\r\n");

    // form fields
    for (k, v) in &body.form_fields {
        if v.trim().is_empty() {
            continue;
        }
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
    let mut req =
        UpstreamRequest::post_multipart(upstream_url, &content_type, Bytes::from(payload));

    apply_adapter_headers(&mut req, &adapter, api_key, upstream_model_id, true);

    let cancel = CancellationToken::new();
    upstream_client
        .call(req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("{upstream_url}: {e:?}")))
}

async fn dispatch_single_transcribe(
    db_pool: &DbPool,
    upstream_client: &Arc<UpstreamClient>,
    circuit_breaker: &CircuitBreakerRegistry,
    master_key: &MasterKey,
    adapter: ProviderAdapterEnum,
    target: &crate::unary::AudioTarget,
    parsed_body: &ParsedAudioBody,
) -> Result<AudioTranscriptionResponse> {
    let upstream_url = adapter.build_transcription_url();
    let api_key = resolve_api_key(db_pool, master_key, target.account_id, &target.provider)?;
    let body_clone = parsed_body.clone();

    let response = dispatch_audio_request(
        upstream_client,
        adapter,
        &upstream_url,
        &api_key,
        &target.upstream_model,
        body_clone,
    )
    .await
    .map_err(|e| {
        crate::guarded_unary_target!(record_failure: circuit_breaker, target);
        tracing::warn!(
            "Audio target failed (connection error): provider={}, error={:?}",
            target.provider,
            e
        );
        e
    })?;

    let status_code = response.status;
    let content_type = response
        .headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_string();

    let body_bytes = response.collect().await.map_err(|e| {
        let err = CoreError::UpstreamConnection(format!("read body: {e:?}"));
        crate::guarded_unary_target!(record_failure: circuit_breaker, target);
        tracing::warn!(
            "Audio target body read failed: provider={}, error={:?}",
            target.provider,
            err
        );
        err
    })?;

    let code_u16 = status_code.as_u16();
    if code_u16 >= 400 {
        crate::guarded_unary_target!(record_failure: circuit_breaker, target);
        let err_text = String::from_utf8_lossy(&body_bytes);
        tracing::warn!(
            "Audio target returned error status: provider={}, model={}, status={}, body={}",
            target.provider,
            target.upstream_model,
            status_code,
            err_text
        );
        return Err(map_upstream_status_error(
            code_u16,
            target.provider.as_str(),
            &target.upstream_model,
            &err_text,
        ));
    }

    Ok(AudioTranscriptionResponse {
        status_code: code_u16,
        content_type,
        body_bytes,
    })
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

        crate::guarded_unary_target!(check: db_pool, circuit_breaker, target);

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

        match dispatch_single_transcribe(
            db_pool,
            upstream_client,
            circuit_breaker,
            master_key,
            adapter,
            &target,
            &parsed_body,
        )
        .await
        {
            Ok(resp) => {
                let total_ms = started.elapsed().as_millis() as u64;
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
                        prompt_tokens: None,
                        completion_tokens: None,
                        status_code: resp.status_code,
                        error_msg: None,
                        total_ms,
                        endpoint_kind: openproxy_types::EndpointKind::Audio,
                    },
                );

                tracing::info!("Audio request succeeded after {} attempts", attempt);
                return Ok(resp);
            }
            Err(e) => {
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| CoreError::Internal("No valid targets found".into())))
}

/// Generate a clean synthetic 16kHz 16-bit mono PCM WAV buffer of a spoken "hello"
/// for testing Speech-to-Text (STT/Whisper) models without external file dependencies.
pub fn generate_test_speech_wav() -> Vec<u8> {
    const SAMPLE_RATE: u32 = 16000;
    const DURATION_SECS: f32 = 1.0;
    let num_samples = (SAMPLE_RATE as f32 * DURATION_SECS) as usize;
    let mut pcm_bytes = Vec::with_capacity(num_samples * 2);

    let mut phase0: f32 = 0.0;
    for i in 0..num_samples {
        let t = i as f32 / SAMPLE_RATE as f32;
        let val = if t < 0.15 {
            // /h/ aspiration noise
            let pseudo_rand =
                (((i.wrapping_mul(1103515245)).wrapping_add(12345)) % 65536) as f32 / 65536.0 - 0.5;
            pseudo_rand * 0.25 * (t / 0.15 * std::f32::consts::PI).sin()
        } else if t < 0.50 {
            // /e/ formant
            let p = (t - 0.15) / 0.35;
            let f0 = 140.0 - 15.0 * p;
            phase0 += 2.0 * std::f32::consts::PI * f0 / SAMPLE_RATE as f32;
            let env = (p * std::f32::consts::PI).sin();
            let glot = phase0.sin() + 0.5 * (2.0 * phase0).sin();
            let f1 = 530.0;
            let f2 = 1840.0;
            env * (0.6 * (2.0 * std::f32::consts::PI * f1 * t).sin()
                + 0.3 * (2.0 * std::f32::consts::PI * f2 * t).sin()
                + 0.2 * glot)
        } else if t < 0.70 {
            // /l/
            let p = (t - 0.50) / 0.20;
            let f0 = 125.0;
            phase0 += 2.0 * std::f32::consts::PI * f0 / SAMPLE_RATE as f32;
            let env = 0.6 * (p * std::f32::consts::PI).sin();
            let f1 = 380.0;
            let f2 = 1200.0;
            env * (0.5 * (2.0 * std::f32::consts::PI * f1 * t).sin()
                + 0.2 * (2.0 * std::f32::consts::PI * f2 * t).sin())
        } else if t < 0.95 {
            // /o/ formant
            let p = (t - 0.70) / 0.25;
            let f0 = 120.0 - 20.0 * p;
            phase0 += 2.0 * std::f32::consts::PI * f0 / SAMPLE_RATE as f32;
            let env = (p * std::f32::consts::PI).sin();
            let glot = phase0.sin() + 0.4 * (2.0 * phase0).sin();
            let f1 = 480.0;
            let f2 = 950.0;
            env * (0.6 * (2.0 * std::f32::consts::PI * f1 * t).sin()
                + 0.3 * (2.0 * std::f32::consts::PI * f2 * t).sin()
                + 0.2 * glot)
        } else {
            0.0
        };

        let sample = (val.clamp(-1.0, 1.0) * 22000.0) as i16;
        pcm_bytes.extend_from_slice(&sample.to_le_bytes());
    }

    let pcm_len = pcm_bytes.len() as u32;
    let byte_rate = SAMPLE_RATE * 2;
    let mut wav = Vec::with_capacity(44 + pcm_bytes.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + pcm_len).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // subchunk1 size (16 for PCM)
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM format = 1
    wav.extend_from_slice(&1u16.to_le_bytes()); // Mono (1 channel)
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes()); // Block align (2 bytes)
    wav.extend_from_slice(&16u16.to_le_bytes()); // Bits per sample (16)
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&pcm_len.to_le_bytes());
    wav.extend_from_slice(&pcm_bytes);
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_test_speech_wav_valid_riff_pcm() {
        let wav = generate_test_speech_wav();
        assert!(wav.len() > 44);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");

        let channels = u16::from_le_bytes([wav[22], wav[23]]);
        let sample_rate = u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]);
        let bits_per_sample = u16::from_le_bytes([wav[34], wav[35]]);

        assert_eq!(channels, 1);
        assert_eq!(sample_rate, 16000);
        assert_eq!(bits_per_sample, 16);
        assert_eq!(&wav[36..40], b"data");
    }
}
