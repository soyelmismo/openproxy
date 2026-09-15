//! OpenAI Responses API SSE parser.

use super::{UpstreamSseChunk, make_text_delta, parse_provider_json, parse_sse_data_or_done};
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
    pub usage: Option<OpenAIUsage>,
}

fn find_tool_call_index(tool_calls: &[Value], item_id: &str, call_id: &str) -> Option<usize> {
    for (i, tc) in tool_calls.iter().enumerate().rev() {
        if !item_id.is_empty() && tc.get("item_id").and_then(Value::as_str) == Some(item_id) {
            return Some(i);
        }
        if !call_id.is_empty() && tc.get("id").and_then(Value::as_str) == Some(call_id) {
            return Some(i);
        }
    }
    None
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
        super::SseDataOrDone::Done => {
            let mut chunk = UpstreamSseChunk::done();
            chunk.usage = state.usage.clone();
            return Ok(Some(chunk));
        }
        super::SseDataOrDone::Skip => return Ok(None),
    };

    let value: Value = parse_provider_json(data, "responses")?;

    if let Some(error) = value
        .get("error")
        .or_else(|| value.get("response").and_then(|r| r.get("error")))
        .filter(|e| !e.is_null())
    {
        let msg = if let Some(m) = error.get("message").and_then(Value::as_str) {
            if let Some(code) = error.get("code").and_then(Value::as_str) {
                format!("{code}: {m}")
            } else {
                m.to_string()
            }
        } else {
            error.to_string()
        };
        return Err(CoreError::upstream_error(
            500,
            "responses",
            model_name,
            msg,
            false,
        ));
    }

    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let usage = value
        .get("usage")
        .or_else(|| value.get("response").and_then(|r| r.get("usage")))
        .map(|u| {
            let prompt_tokens = u
                .get("input_tokens")
                .or_else(|| u.get("prompt_tokens"))
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0);
            let completion_tokens = u
                .get("output_tokens")
                .or_else(|| u.get("completion_tokens"))
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0);
            let total_tokens = u
                .get("total_tokens")
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or_else(|| prompt_tokens.saturating_add(completion_tokens));

            let cached_tokens = u
                .get("input_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .or_else(|| {
                    u.get("prompt_tokens_details")
                        .and_then(|d| d.get("cached_tokens"))
                })
                .and_then(Value::as_u64)
                .and_then(|v| u32::try_from(v).ok());

            let prompt_tokens_details =
                cached_tokens.map(|cached| openproxy_types::PromptTokensDetails {
                    cached_tokens: Some(cached),
                });

            OpenAIUsage {
                prompt_tokens,
                completion_tokens,
                total_tokens,
                prompt_tokens_details,
            }
        });

    if let Some(ref u) = usage {
        state.usage = Some(u.clone());
    }

    if event_type == "response.output_item.added"
        && let Some(item) = value.get("item")
    {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if item_type == "function_call" {
            let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let call_id =
                item.get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(if item_id.is_empty() {
                        "call_xyz"
                    } else {
                        item_id
                    });
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");

            let existing_idx = find_tool_call_index(&state.tool_calls, item_id, call_id);
            let tc_index = match existing_idx {
                Some(idx) => idx,
                None => {
                    // Guard: prevent unbounded tool_calls vector growth
                    if state.tool_calls.len() >= MAX_RESPONSES_TOOL_CALLS {
                        tracing::warn!(
                            count = state.tool_calls.len(),
                            max = MAX_RESPONSES_TOOL_CALLS,
                            "ResponsesSseState: tool_calls limit reached — dropping new call"
                        );
                        return Ok(None);
                    }
                    state.tool_calls.push(serde_json::json!({
                        "id": call_id,
                        "item_id": item_id,
                        "type": "function",
                        "function": { "name": name, "arguments": "" }
                    }));
                    state.tool_calls.len() - 1
                }
            };

            let chunk = super::make_tool_call_start(
                chunk_id,
                created,
                model_name,
                tc_index as u32,
                call_id,
                name,
            );
            return Ok(Some(chunk));
        }
    }

    if event_type == "response.function_call_arguments.delta"
        && let Some(delta) = value.get("delta").and_then(|v| v.as_str())
    {
        let item_id = value.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
        let call_id = value
            .get("call_id")
            .or_else(|| value.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if state.tool_calls.is_empty() {
            return Ok(None);
        }

        let index = find_tool_call_index(&state.tool_calls, item_id, call_id)
            .unwrap_or_else(|| state.tool_calls.len().saturating_sub(1));

        if let Some(tc) = state.tool_calls.get_mut(index)
            && let Some(func) = tc.get_mut("function").and_then(|v| v.as_object_mut())
            && let Some(args) = func.get_mut("arguments")
            && let Some(args_str) = args.as_str()
        {
            // Guard: prevent unbounded arguments accumulation
            if args_str.len().saturating_add(delta.len()) > MAX_RESPONSES_TOOL_CALL_ARGS_BYTES {
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

        let chunk = super::make_tool_call_delta(chunk_id, created, model_name, index as u32, delta);
        return Ok(Some(chunk));
    }

    if event_type == "response.function_call_arguments.done" {
        let item_id = value.get("item_id").and_then(|v| v.as_str()).unwrap_or("");
        let call_id = value
            .get("call_id")
            .or_else(|| value.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let authoritative_args = value
            .get("arguments")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        if let Some(index) = find_tool_call_index(&state.tool_calls, item_id, call_id)
            && let Some(tc) = state.tool_calls.get_mut(index)
            && let Some(func) = tc.get_mut("function").and_then(|v| v.as_object_mut())
            && let Some(args) = func.get_mut("arguments")
            && let Some(current_args) = args.as_str()
        {
            let missing_delta = if authoritative_args.len() > current_args.len()
                && authoritative_args.starts_with(current_args)
            {
                authoritative_args.get(current_args.len()..).unwrap_or("")
            } else if current_args.is_empty() && !authoritative_args.is_empty() {
                authoritative_args
            } else {
                ""
            };

            *args = serde_json::Value::String(authoritative_args.to_string());

            if !missing_delta.is_empty() {
                let chunk = super::make_tool_call_delta(
                    chunk_id,
                    created,
                    model_name,
                    index as u32,
                    missing_delta,
                );
                return Ok(Some(chunk));
            }
        }
        return Ok(None);
    }

    if event_type == "response.output_item.done"
        && let Some(item) = value.get("item")
    {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if item_type == "function_call" {
            let item_id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let call_id =
                item.get("call_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or(if item_id.is_empty() {
                        "call_xyz"
                    } else {
                        item_id
                    });
            let authoritative_args = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("");

            if let Some(index) = find_tool_call_index(&state.tool_calls, item_id, call_id)
                && let Some(tc) = state.tool_calls.get_mut(index)
                && let Some(func) = tc.get_mut("function").and_then(|v| v.as_object_mut())
                && let Some(args) = func.get_mut("arguments")
                && let Some(current_args) = args.as_str()
            {
                let missing_delta = if authoritative_args.len() > current_args.len()
                    && authoritative_args.starts_with(current_args)
                {
                    authoritative_args.get(current_args.len()..).unwrap_or("")
                } else if current_args.is_empty() && !authoritative_args.is_empty() {
                    authoritative_args
                } else {
                    ""
                };

                *args = serde_json::Value::String(authoritative_args.to_string());

                if !missing_delta.is_empty() {
                    let chunk = super::make_tool_call_delta(
                        chunk_id,
                        created,
                        model_name,
                        index as u32,
                        missing_delta,
                    );
                    return Ok(Some(chunk));
                }
            }
        }
        return Ok(None);
    }

    if event_type == "response.content_part.added"
        && let Some(part) = value.get("part")
    {
        let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if !text.is_empty() {
            return Ok(Some(make_text_delta(
                chunk_id, created, model_name, text, false,
            )));
        }
    }

    if matches!(
        event_type,
        "response.reasoning_text.delta"
            | "response.reasoning_summary.delta"
            | "response.reasoning_summary_text.delta"
    ) {
        let delta = value.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        if !delta.is_empty() {
            return Ok(Some(make_text_delta(
                chunk_id, created, model_name, delta, true,
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
                chunk_id, created, model_name, delta, false,
            )));
        }
    }

    if matches!(
        event_type,
        "response.done" | "response.completed" | "response.incomplete"
    ) {
        let incomplete_reason = value
            .get("response")
            .and_then(|r| r.get("incomplete_details"))
            .and_then(|d| d.get("reason"))
            .and_then(|s| s.as_str());

        let stop_reason = match incomplete_reason {
            Some("max_output_tokens") => Some("length".to_string()),
            Some("content_filter") => Some("content_filter".to_string()),
            _ if !state.tool_calls.is_empty() => Some("tool_calls".to_string()),
            _ => Some("stop".to_string()),
        };

        let final_usage = usage.or_else(|| state.usage.clone());
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
                "usage": final_usage
            }),
            done: false,
            usage: final_usage,
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

    #[test]
    fn responses_completed_with_input_output_tokens() {
        let mut state = ResponsesSseState::default();
        let line = r#"data: {"type":"response.completed","response":{"usage":{"input_tokens":61,"output_tokens":18,"total_tokens":79,"input_tokens_details":{"cached_tokens":12}}}}"#;

        let chunk =
            parse_responses_sse_stream_line(line, "chatcmpl_1", 123, "muse-spark", &mut state)
                .expect("parse")
                .expect("chunk");

        let usage = chunk.usage.expect("usage present");
        assert_eq!(usage.prompt_tokens, 61);
        assert_eq!(usage.completion_tokens, 18);
        assert_eq!(usage.total_tokens, 79);
        assert_eq!(
            usage
                .prompt_tokens_details
                .as_ref()
                .and_then(|d| d.cached_tokens),
            Some(12)
        );
        assert_eq!(state.usage.as_ref().map(|u| u.completion_tokens), Some(18));
    }

    #[test]
    fn responses_done_carries_persisted_usage_from_prior_event() {
        let mut state = ResponsesSseState::default();
        // First event carries usage
        let line1 = r#"data: {"type":"response.output_item.done","usage":{"input_tokens":100,"output_tokens":25}}"#;
        let _ = parse_responses_sse_stream_line(line1, "chatcmpl_1", 123, "muse-spark", &mut state)
            .expect("parse");

        assert_eq!(state.usage.as_ref().map(|u| u.completion_tokens), Some(25));

        // Terminal done sentinel carries persisted usage
        let line_done = "data: [DONE]";
        let chunk =
            parse_responses_sse_stream_line(line_done, "chatcmpl_1", 123, "muse-spark", &mut state)
                .expect("parse")
                .expect("done chunk");

        assert!(chunk.done);
        assert_eq!(chunk.usage.as_ref().map(|u| u.completion_tokens), Some(25));
    }

    #[test]
    fn responses_tool_call_start_and_delta_by_item_id() {
        let mut state = ResponsesSseState::default();
        let line_add = r#"data: {"type":"response.output_item.added","item":{"id":"fc_001","call_id":"call_abc","type":"function_call","name":"get_weather","arguments":""}}"#;
        let chunk_start =
            parse_responses_sse_stream_line(line_add, "c1", 123, "muse-spark", &mut state)
                .expect("parse")
                .expect("chunk");

        assert_eq!(
            chunk_start.payload["choices"][0]["delta"]["tool_calls"][0]["id"].as_str(),
            Some("call_abc")
        );
        assert_eq!(
            chunk_start.payload["choices"][0]["delta"]["tool_calls"][0]["function"]["name"]
                .as_str(),
            Some("get_weather")
        );

        // Delta event only contains item_id, not call_id
        let line_delta = r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_001","delta":"{\"city\":\"Madrid\"}"}"#;
        let chunk_delta =
            parse_responses_sse_stream_line(line_delta, "c1", 123, "muse-spark", &mut state)
                .expect("parse")
                .expect("chunk");

        assert_eq!(
            chunk_delta.payload["choices"][0]["delta"]["tool_calls"][0]["index"].as_u64(),
            Some(0)
        );
        assert_eq!(
            chunk_delta.payload["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"]
                .as_str(),
            Some("{\"city\":\"Madrid\"}")
        );
    }

    #[test]
    fn responses_parallel_tool_calls_interleaved() {
        let mut state = ResponsesSseState::default();
        // Add tool 0
        let l1 = r#"data: {"type":"response.output_item.added","item":{"id":"fc_1","call_id":"call_1","type":"function_call","name":"tool_a","arguments":""}}"#;
        let _ = parse_responses_sse_stream_line(l1, "c1", 123, "muse-spark", &mut state)
            .expect("parse");

        // Add tool 1
        let l2 = r#"data: {"type":"response.output_item.added","item":{"id":"fc_2","call_id":"call_2","type":"function_call","name":"tool_b","arguments":""}}"#;
        let _ = parse_responses_sse_stream_line(l2, "c1", 123, "muse-spark", &mut state)
            .expect("parse");

        // Delta for tool 1 arrives first
        let l3 = r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_2","delta":"arg_b"}"#;
        let chunk_b = parse_responses_sse_stream_line(l3, "c1", 123, "muse-spark", &mut state)
            .expect("parse")
            .expect("chunk");
        assert_eq!(
            chunk_b.payload["choices"][0]["delta"]["tool_calls"][0]["index"].as_u64(),
            Some(1)
        );

        // Delta for tool 0 arrives second
        let l4 = r#"data: {"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"arg_a"}"#;
        let chunk_a = parse_responses_sse_stream_line(l4, "c1", 123, "muse-spark", &mut state)
            .expect("parse")
            .expect("chunk");
        assert_eq!(
            chunk_a.payload["choices"][0]["delta"]["tool_calls"][0]["index"].as_u64(),
            Some(0)
        );
    }

    #[test]
    fn responses_function_call_arguments_done_fills_missing_delta() {
        let mut state = ResponsesSseState::default();
        let l1 = r#"data: {"type":"response.output_item.added","item":{"id":"fc_1","call_id":"call_1","type":"function_call","name":"tool_a","arguments":""}}"#;
        let _ = parse_responses_sse_stream_line(l1, "c1", 123, "muse-spark", &mut state)
            .expect("parse");

        // Upstream sends arguments.done directly without prior deltas
        let l2 = r#"data: {"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"param\":\"val\"}"}"#;
        let chunk = parse_responses_sse_stream_line(l2, "c1", 123, "muse-spark", &mut state)
            .expect("parse")
            .expect("chunk");

        assert_eq!(
            chunk.payload["choices"][0]["delta"]["tool_calls"][0]["index"].as_u64(),
            Some(0)
        );
        assert_eq!(
            chunk.payload["choices"][0]["delta"]["tool_calls"][0]["function"]["arguments"].as_str(),
            Some("{\"param\":\"val\"}")
        );

        // Subsequent done with same arguments returns None (no duplicate delta)
        let l3 = r#"data: {"type":"response.output_item.done","item":{"id":"fc_1","call_id":"call_1","type":"function_call","arguments":"{\"param\":\"val\"}"}}"#;
        let chunk_done = parse_responses_sse_stream_line(l3, "c1", 123, "muse-spark", &mut state)
            .expect("parse");
        assert!(chunk_done.is_none());
    }

    #[test]
    fn responses_reasoning_delta_emits_reasoning_content() {
        let mut state = ResponsesSseState::default();
        let line = r#"data: {"type":"response.reasoning_text.delta","delta":"thinking step"}"#;
        let chunk = parse_responses_sse_stream_line(line, "c1", 123, "muse-spark", &mut state)
            .expect("parse")
            .expect("chunk");

        assert_eq!(
            chunk.payload["choices"][0]["delta"]["reasoning_content"].as_str(),
            Some("thinking step")
        );
        assert_eq!(chunk.delta_reasoning.as_deref(), Some("thinking step"));
    }

    #[test]
    fn responses_incomplete_max_tokens_sets_length_stop_reason() {
        let mut state = ResponsesSseState::default();
        let line = r#"data: {"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#;
        let chunk = parse_responses_sse_stream_line(line, "c1", 123, "muse-spark", &mut state)
            .expect("parse")
            .expect("chunk");

        assert_eq!(
            chunk.payload["choices"][0]["finish_reason"].as_str(),
            Some("length")
        );
    }

    #[test]
    fn responses_response_created_with_null_error_is_not_error() {
        let mut state = ResponsesSseState::default();
        let line = r#"data: {"type":"response.created","response":{"id":"resp_123","error":null}}"#;
        let chunk = parse_responses_sse_stream_line(line, "c1", 123, "muse-spark", &mut state)
            .expect("should not error on null response.error");
        assert!(chunk.is_none());
    }
}
