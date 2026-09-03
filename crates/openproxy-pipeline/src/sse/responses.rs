//! OpenAI Responses API SSE parser.

use super::{
    UpstreamSseChunk, make_text_delta, parse_provider_json, parse_sse_data_or_done,
};
use crate::translation::OpenAIUsage;
use openproxy_types::error::{CoreError, Result};
use serde_json::Value;

/// Maximum allowed tool calls accumulated in ResponsesSseState.
/// Prevents unbounded vector growth from a malicious upstream.
pub(crate) const MAX_RESPONSES_TOOL_CALLS: usize = 128;
/// Maximum allowed bytes for accumulated tool call arguments in the Responses API path.
/// Prevents unbounded string growth per tool call from accumulated delta fragments.
pub(crate) const MAX_RESPONSES_TOOL_CALL_ARGS_BYTES: usize = 1_048_576; // 1 MiB

#[derive(Default, Debug)]
pub struct ResponsesSseState {
    pub tool_calls: Vec<serde_json::Value>,
}

pub fn parse_responses_sse_stream_line(
    line: &str,
    chunk_id: &str,
    created: u64,
    model_name: &str,
    state: &mut ResponsesSseState,
) -> Result<Option<UpstreamSseChunk>> {
    let data = match parse_sse_data_or_done(line) {
        super::SseDataOrDone::Payload(p) => p,
        super::SseDataOrDone::Done => return Ok(Some(UpstreamSseChunk::done())),
        super::SseDataOrDone::Skip => return Ok(None),
    };

    let value: Value = parse_provider_json(data, "responses")?;

    if let Some(error) = value.get("error") {
        return Err(CoreError::upstream_error(
            500,
            "responses",
            model_name,
            error.to_string(),
            false,
        ));
    }

    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let mut usage = None;
    if let Some(u) = value
        .get("usage")
        .or_else(|| value.get("response").and_then(|r| r.get("usage")))
        && let Ok(mut u_parsed) = <OpenAIUsage as serde::Deserialize>::deserialize(u)
    {
        if let Some(val) = u.get("input_tokens").and_then(serde_json::Value::as_u64) {
            u_parsed.prompt_tokens = val.try_into().unwrap_or(u32::MAX);
        }
        if let Some(val) = u.get("output_tokens").and_then(serde_json::Value::as_u64) {
            u_parsed.completion_tokens = val.try_into().unwrap_or(u32::MAX);
        }
        if u_parsed.total_tokens == 0 {
            u_parsed.total_tokens = u_parsed.prompt_tokens + u_parsed.completion_tokens;
        }
        usage = Some(u_parsed);
    }

    if event_type == "response.output_item.added"
        && let Some(item) = value.get("item")
    {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if item_type == "function_call" {
            // Guard: prevent unbounded tool_calls vector growth
            if state.tool_calls.len() >= MAX_RESPONSES_TOOL_CALLS {
                tracing::warn!(
                    count = state.tool_calls.len(),
                    max = MAX_RESPONSES_TOOL_CALLS,
                    "ResponsesSseState: tool_calls limit reached — dropping new call"
                );
                return Ok(None);
            }
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("call_xyz")
                .to_string();
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();

            state.tool_calls.push(serde_json::json!({
                "id": &call_id,
                "type": "function",
                "function": { "name": &name, "arguments": "" }
            }));

            let tc_index = state.tool_calls.len() - 1;
            let chunk = super::make_tool_call_start(
                chunk_id,
                created,
                model_name,
                tc_index as u32,
                &call_id,
                &name,
            );
            return Ok(Some(chunk));
        }
    }

    if event_type == "response.function_call_arguments.delta"
        && let Some(delta) = value.get("delta").and_then(|v| v.as_str())
    {
        let call_id = value
            .get("call_id")
            .or_else(|| value.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if state.tool_calls.is_empty() {
            return Ok(None);
        }

        let mut index = state.tool_calls.len().saturating_sub(1);

        for (i, tc) in state.tool_calls.iter_mut().enumerate().rev() {
            if let Some(id) = tc.get("id").and_then(|v| v.as_str())
                && (id == call_id || call_id.is_empty())
            {
                if let Some(func) = tc.get_mut("function").and_then(|v| v.as_object_mut())
                    && let Some(args) = func.get_mut("arguments")
                    && let Some(args_str) = args.as_str()
                {
                    // Guard: prevent unbounded arguments accumulation
                    if args_str.len() + delta.len() > MAX_RESPONSES_TOOL_CALL_ARGS_BYTES {
                        tracing::warn!(
                            current_len = args_str.len(),
                            delta_len = delta.len(),
                            max = MAX_RESPONSES_TOOL_CALL_ARGS_BYTES,
                            "ResponsesSseState: tool call arguments limit reached — dropping delta"
                        );
                    } else {
                        let mut new_args = args_str.to_string();
                        new_args.push_str(delta);
                        *args = serde_json::Value::String(new_args);
                    }
                }
                index = i;
                break;
            }
        }

        let chunk = super::make_tool_call_delta(
            chunk_id,
            created,
            model_name,
            index as u32,
            delta,
        );
        return Ok(Some(chunk));
    }

    if event_type == "response.content_part.added"
        && let Some(part) = value.get("part")
    {
        let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if !text.is_empty() {
            return Ok(Some(make_text_delta(
                chunk_id,
                created,
                model_name,
                text,
                false,
            )));
        }
    }

    if matches!(
        event_type,
        "response.output_text.delta" | "response.text.delta" | "response.audio.delta"
    ) {
        let delta = value.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if !delta.is_empty() {
            return Ok(Some(make_text_delta(
                chunk_id,
                created,
                model_name,
                delta,
                false,
            )));
        }
    }

    if event_type == "response.done" || event_type == "response.completed" {
        let mut stop_reason = Some("stop".to_string());
        if !state.tool_calls.is_empty() {
            stop_reason = Some("tool_calls".to_string());
        }
        return Ok(Some(UpstreamSseChunk {
            raw_payload: None,
            payload: serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model_name,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": stop_reason
                }],
                "usage": usage
            }),
            done: false,
            usage,
            stop_reason,
            delta_reasoning: None,
            delta_tool_calls: Vec::new(),
            has_content: false,
        }));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responses_output_text_delta_translates_to_chat_delta() {
        let mut state = ResponsesSseState::default();
        let line = r#"data: {"type":"response.output_text.delta","delta":"pong"}"#;

        let chunk =
            parse_responses_sse_stream_line(line, "chatcmpl_1", 123, "gpt-test", &mut state)
                .expect("parse")
                .expect("chunk");

        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"].as_str(),
            Some("pong")
        );
        assert!(chunk.has_content);
    }

    #[test]
    fn responses_completed_uses_nested_usage() {
        let mut state = ResponsesSseState::default();
        let line = r#"data: {"type":"response.completed","response":{"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}}"#;

        let chunk =
            parse_responses_sse_stream_line(line, "chatcmpl_1", 123, "gpt-test", &mut state)
                .expect("parse")
                .expect("chunk");

        assert_eq!(
            chunk.payload["choices"][0]["finish_reason"].as_str(),
            Some("stop")
        );
        assert_eq!(chunk.usage.as_ref().map(|u| u.total_tokens), Some(5));
    }
}
