//! Caso especial aislado: dispatch para `horde` vision (interrogación de
//! imagen al endpoint público de AI Horde). Bifurca ANTES de la decisión
//! streaming/non-streaming del dispatcher principal.

use super::types::{DispatchContext, DispatchParams};
use super::UpstreamDispatcher;
use crate::translation::OpenAIResponse;
use crate::PipelineResult;
use openproxy_adapters::upstream::{CancellationToken, UpstreamRequest};
use openproxy_types::combos::ComboTarget;
use openproxy_types::error::CoreError;
use openproxy_types::Model;
use std::time::Instant;

/// `true` cuando el target es `horde` y el modelo es de visión (o el
/// request trae una imagen embebida en mensajes). Esta rama se evalúa
/// en `mod.rs::dispatch_upstream` antes de poblar headers.
pub(super) fn is_horde_vision_request(
    target: &ComboTarget,
    model: &Model,
    req: &crate::PipelineRequest,
) -> bool {
    target.provider_id.as_str() == "horde"
        && (openproxy_adapters::HordeAdapter::is_vision_model(model.model_id.as_str())
            || openproxy_adapters::HordeAdapter::extract_image_from_messages(
                &req.openai_request.messages,
            )
            .is_some())
}

impl UpstreamDispatcher {
    /// Bifurcación ortogonal: si es Horde vision, ejecuta la interrogación
    /// y serializa la respuesta como `OpenAIResponse` (también opcionalmente
    /// como stream SSE de 2 chunks).
    pub(super) async fn dispatch_horde_vision(
        &self,
        params: DispatchParams<'_>,
        dctx: DispatchContext<'_>,
        upstream_request: UpstreamRequest,
    ) -> PipelineResult {
        let DispatchParams {
            target,
            combo,
            req,
            model,
            headers,
            started,
            attempt,
            race_size,
            trace_id,
            ..
        } = params;

        let Some(source_image) = openproxy_adapters::HordeAdapter::extract_image_from_messages(
            &req.openai_request.messages,
        ) else {
            let err = CoreError::Validation(
                "No image found in request messages for Horde vision interrogation".into(),
            );
            return self.record_and_fail(
                req,
                combo,
                target,
                dctx.fail_ctx_code(&err, None, None, 400),
            );
        };

        let send_start = Instant::now();
        let cancel_token = CancellationToken::from_watch(tokio::sync::watch::Receiver::clone(
            &req.client_disconnected,
        ));
        let api_key = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("apikey"))
            .map_or("", |(_, v)| v.as_str());
        let base_url = "https://aihorde.net/api/v2";

        let caption_res = openproxy_adapters::HordeAdapter::execute_interrogate(
            &self.config.upstream_client,
            base_url,
            api_key,
            &source_image,
            cancel_token,
        )
        .await;

        let connect_and_send_ms = send_start.elapsed().as_millis() as u64;

        let caption = match caption_res {
            Ok(c) => c,
            Err(e) => {
                return self.record_and_fail(
                    req,
                    combo,
                    target,
                    dctx.fail_ctx_code(&e, Some(connect_and_send_ms), None, e.http_status()),
                );
            }
        };

        let mut response_id = String::with_capacity(48);
        use std::fmt::Write;
        let _ = write!(
            &mut response_id,
            "chatcmpl_{}",
            uuid::Uuid::new_v4().simple()
        );

        let created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let completion_tokens = (caption.len() / 4).max(1) as u32;
        let prompt_tokens = 10u32;
        let openai_response = OpenAIResponse {
            id: response_id.clone(),
            object: "chat.completion".to_string(),
            created,
            model: req.openai_request.model.clone(),
            choices: vec![openproxy_types::OpenAIChoice {
                index: 0,
                message: openproxy_types::OpenAIMessage {
                    role: "assistant".to_string(),
                    content: Some(serde_json::Value::String(caption.clone())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: Default::default(),
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(openproxy_types::OpenAIUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens: prompt_tokens + completion_tokens,
                prompt_tokens_details: None,
            }),
        };

        if let Some(sink) = req.stream_sink.as_ref() {
            let chunk1 = serde_json::json!({
                "id": response_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": req.openai_request.model,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "role": "assistant",
                        "content": caption
                    },
                    "finish_reason": null
                }]
            });
            let chunk2 = serde_json::json!({
                "id": response_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": req.openai_request.model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
            });
            let chunk1_str = serde_json::to_string(&chunk1).unwrap_or_default();
            let mut buf1 = String::with_capacity(chunk1_str.len() + 8);
            use std::fmt::Write;
            let _ = write!(&mut buf1, "data: {chunk1_str}\n\n");
            let _ = sink.send(bytes::Bytes::from(buf1)).await;

            let chunk2_str = serde_json::to_string(&chunk2).unwrap_or_default();
            let mut buf2 = String::with_capacity(chunk2_str.len() + 8);
            let _ = write!(&mut buf2, "data: {chunk2_str}\n\n");
            let _ = sink.send(bytes::Bytes::from(buf2)).await;
            let _ = sink.send(crate::pipeline::SSE_DONE_BYTES).await;
        }

        let total_ms_now = started.elapsed().as_millis() as u64;
        let request_headers_btm = if self.tracker.is_recording() {
            Some(crate::redact::redact_btreemap_sensitive(
                headers
                    .iter()
                    .map(|(k, v)| (k.to_owned(), v.to_owned()))
                    .collect::<std::collections::BTreeMap<String, String>>(),
            ))
        } else {
            None
        };
        let resp_json = serde_json::to_value(&openai_response).unwrap_or_default();
        let usage_tuple =
            match crate::usage_tracker::UsageRecordBuilder::new(&self.tracker, req, combo, target)
                .proxy_url(upstream_request.proxy)
                .proxy_status(upstream_request.proxy_status)
                .model_opt(Some(model))
                .err_opt(None)
                .connect_ms_opt(Some(connect_and_send_ms))
                .ttft_ms_opt(Some(connect_and_send_ms))
                .total_ms(total_ms_now)
                .status_code(200)
                .attempt(attempt)
                .race_size(race_size)
                .trace_id(trace_id.clone())
                .prompt_tokens_opt(Some(prompt_tokens))
                .completion_tokens_opt(Some(completion_tokens))
                .cached_tokens(None)
                .response_body_json(Some(resp_json))
                .request_headers(request_headers_btm)
                .response_headers(None)
                .is_streaming(false)
                .stream_complete(true)
                .stop_reason(None)
                .record()
            {
                Ok(id) => id,
                Err(e) => {
                    tracing::warn!(error = %e, "UsageRecordBuilder failed; non-fatal");
                    None
                }
            };

        PipelineResult {
            status_code: 200,
            error: None,
            final_response: Some(openai_response),
            attempts: attempt,
            usage_tuple,
        }
    }
}