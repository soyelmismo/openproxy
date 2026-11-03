//! Translation and response extraction helpers for non-streaming upstream responses.

use openproxy_adapters::ProviderAdapter;
use openproxy_types::OpenAIResponse;
use openproxy_types::error::CoreError;

/// `OpenAIResponse` minimalista desde `choices[0].message.content` del body
/// crudo, para adapters que no producen OpenAI nativo.
pub(crate) fn translate_simple_text_response(
    response_body_raw: &serde_json::Value,
    model_name: String,
) -> OpenAIResponse {
    let text = response_body_raw
        .get("choices")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|s| s.as_str())
        .or_else(|| response_body_raw.get("content").and_then(|s| s.as_str()))
        .unwrap_or("");

    let mut id = String::with_capacity(48);
    use std::fmt::Write;
    let _ = write!(&mut id, "chatcmpl_{}", uuid::Uuid::new_v4());

    OpenAIResponse {
        id,
        object: "chat.completion".to_string(),
        created: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs()),
        model: model_name,
        choices: vec![openproxy_types::OpenAIChoice {
            index: 0,
            message: openproxy_types::OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::String(text.to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: None,
    }
}

/// Traduce el body según `target_format`. Gemini delega en su propia
/// traducción; Atomesus cae al fallback simple.
pub(crate) fn translate_non_streaming_body(
    target_format: openproxy_types::TargetFormat,
    response_body_raw: &serde_json::Value,
    req: &crate::PipelineRequest,
) -> Result<OpenAIResponse, CoreError> {
    use openproxy_types::ResultExt;
    match target_format {
        openproxy_types::TargetFormat::Responses => {
            crate::translation::responses_to_openai(response_body_raw, &req.openai_request.model)
        }
        openproxy_types::TargetFormat::Openai => {
            <OpenAIResponse as serde::Deserialize>::deserialize(response_body_raw)
                .ctx_parse("parse openai response")
        }
        openproxy_types::TargetFormat::Anthropic => {
            let anthropic_resp: crate::translation::AnthropicResponse =
                <crate::translation::AnthropicResponse as serde::Deserialize>::deserialize(
                    response_body_raw,
                )
                .ctx_parse("parse anthropic response")?;
            Ok(crate::translation::anthropic_to_openai(&anthropic_resp))
        }
        openproxy_types::TargetFormat::Gemini => {
            let adapter = openproxy_adapters::GeminiAdapter::new();
            adapter.translate_non_streaming_response(target_format, response_body_raw.clone())
        }
        openproxy_types::TargetFormat::Atomesus => Ok(translate_simple_text_response(
            response_body_raw,
            req.openai_request.model.clone(),
        )),
        openproxy_types::TargetFormat::CommandCodeGo => {
            if let Ok(resp) = <OpenAIResponse as serde::Deserialize>::deserialize(response_body_raw)
            {
                Ok(resp)
            } else {
                let body_str = serde_json::to_string(response_body_raw).unwrap_or_default();
                crate::sse::parse_sse_stream_to_openai_response(
                    target_format,
                    &body_str,
                    &req.openai_request.model,
                )
            }
        }
        openproxy_types::TargetFormat::SystemOne => {
            <OpenAIResponse as serde::Deserialize>::deserialize(response_body_raw)
                .ctx_parse("parse systemone response")
        }
        // Unreachable in practice: Devin bodies are Connect-RPC protobuf and
        // `dispatch_upstream_non_streaming` folds them before JSON parse.
        openproxy_types::TargetFormat::Devin => {
            Err(CoreError::Parse("devin unary body is not JSON".to_string()))
        }
    }
}
