use crate::PipelineRequest;
use crate::translation::OpenAIResponse;
use openproxy_adapters::ProviderAdapter;
use openproxy_types::error::CoreError;

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
    OpenAIResponse {
        id: format!("chatcmpl_{}", uuid::Uuid::new_v4()),
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

pub(crate) fn translate_non_streaming_body(
    target_format: openproxy_types::TargetFormat,
    response_body_raw: &serde_json::Value,
    req: &PipelineRequest,
) -> Result<OpenAIResponse, CoreError> {
    match target_format {
        openproxy_types::TargetFormat::Responses => {
            unreachable!("Responses format is handled natively before dispatcher")
        }
        openproxy_types::TargetFormat::Openai => {
            <OpenAIResponse as serde::Deserialize>::deserialize(response_body_raw)
                .map_err(|e| CoreError::Parse(format!("parse openai response: {e}")))
        }
        openproxy_types::TargetFormat::Anthropic => {
            let anthropic_resp: crate::translation::AnthropicResponse =
                <crate::translation::AnthropicResponse as serde::Deserialize>::deserialize(
                    response_body_raw,
                )
                .map_err(|e| CoreError::Parse(format!("parse anthropic response: {e}")))?;
            Ok(crate::translation::anthropic_to_openai(&anthropic_resp))
        }
        openproxy_types::TargetFormat::Gemini => {
            let adapter = openproxy_adapters::GeminiAdapter::new();
            adapter.translate_non_streaming_response(target_format, response_body_raw.clone())
        }
        openproxy_types::TargetFormat::Atomesus | openproxy_types::TargetFormat::Fx => Ok(
            translate_simple_text_response(response_body_raw, req.openai_request.model.clone()),
        ),
    }
}

pub(crate) fn is_empty_response(resp: &OpenAIResponse) -> bool {
    resp.choices.first().is_some_and(|c| {
        let msg = &c.message;
        let content_empty = msg
            .content
            .as_ref()
            .is_none_or(|v| v.as_str().is_none_or(str::is_empty));
        let no_tool_calls = msg.tool_calls.as_ref().is_none_or(std::vec::Vec::is_empty);
        let no_reasoning = !msg.extra.contains_key("reasoning_content");
        let no_finish = c
            .finish_reason
            .as_ref()
            .is_none_or(|f| f == "null" || f.is_empty());
        content_empty && no_tool_calls && no_reasoning && no_finish
    })
}
