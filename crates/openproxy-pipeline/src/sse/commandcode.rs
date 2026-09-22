//! Command Code SSE parser.
//!
//! Translates Command Code CLI SSE streams (`POST /alpha/generate`)
//! into OpenAI-format SSE chunks and supports unary SSE accumulation.

use super::{
    UpstreamSseChunk, make_text_delta, make_tool_call_delta, make_tool_call_start,
    parse_provider_json,
};
use openproxy_types::error::{CoreError, Result};
use openproxy_types::{OpenAIChoice, OpenAIMessage, OpenAIResponse, OpenAIUsage};
use serde_json::{Value, json};

pub(crate) const MAX_COMMANDCODE_TOOL_CALLS: usize = 128;

#[derive(Default, Debug)]
pub struct CommandCodeSseState {
    pub tool_call_ids: Vec<String>,
    pub tool_calls_streamed: Vec<bool>,
    pub last_finish_step_reason: Option<String>,
    pub last_finish_step_usage: Option<OpenAIUsage>,
}

fn map_commandcode_finish_reason(reason: &str) -> String {
    match reason {
        "end_turn" | "stop" => "stop".to_string(),
        "tool_use" | "tool_call" | "tool_calls" => "tool_calls".to_string(),
        "max_tokens" | "length" => "length".to_string(),
        "content_filter" => "content_filter".to_string(),
        other if !other.is_empty() => other.to_string(),
        _ => "stop".to_string(),
    }
}

/// Parse a single Command Code SSE or NDJSON line into an OpenAI-format chunk.
pub fn parse_commandcode_sse_line(
    line: &str,
    chunk_id: &str,
    created: u64,
    model_name: &str,
    state: &mut CommandCodeSseState,
) -> Result<Option<UpstreamSseChunk>> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with(':') {
        return Ok(None);
    }

    let data_str = if let Some(rest) = trimmed.strip_prefix("data:") {
        let p = rest.trim_start();
        if p == "[DONE]" {
            return Ok(Some(UpstreamSseChunk::done()));
        }
        p
    } else if trimmed == "[DONE]" {
        return Ok(Some(UpstreamSseChunk::done()));
    } else if trimmed.starts_with('{') {
        trimmed
    } else {
        return Ok(None);
    };

    let val: Value = parse_provider_json(data_str, "commandcodego")?;

    let event_type = val.get("type").and_then(Value::as_str).unwrap_or("");

    if event_type == "error" || val.get("error").is_some() {
        let err_obj = val.get("error");
        let msg = err_obj
            .and_then(|e| {
                e.get("message")
                    .and_then(Value::as_str)
                    .or_else(|| e.as_str())
            })
            .or_else(|| val.get("message").and_then(Value::as_str))
            .unwrap_or("commandcode error");
        return Err(CoreError::upstream_error(
            500,
            "commandcodego",
            model_name,
            msg.to_string(),
            false,
        ));
    }

    match event_type {
        "text-delta" | "text" => {
            let text = val
                .get("text")
                .or_else(|| val.get("textDelta"))
                .or_else(|| val.get("delta"))
                .or_else(|| val.get("content"))
                .and_then(Value::as_str)
                .unwrap_or("");
            Ok(Some(make_text_delta(
                chunk_id, created, model_name, text, false,
            )))
        }
        "reasoning-delta" | "reasoning" => {
            let reasoning = val
                .get("text")
                .or_else(|| val.get("reasoningDelta"))
                .or_else(|| val.get("delta"))
                .or_else(|| val.get("textDelta"))
                .and_then(Value::as_str)
                .unwrap_or("");
            Ok(Some(make_text_delta(
                chunk_id, created, model_name, reasoning, true,
            )))
        }
        "tool-input-start" | "tool-call-start" => {
            let id = val
                .get("toolCallId")
                .or_else(|| val.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("call_cc");
            let name = val
                .get("toolName")
                .or_else(|| val.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");

            if let Some(pos) = state.tool_call_ids.iter().position(|i| i == id) {
                return Ok(Some(make_tool_call_start(
                    chunk_id, created, model_name, pos as u32, id, name,
                )));
            }

            if state.tool_call_ids.len() >= MAX_COMMANDCODE_TOOL_CALLS {
                tracing::warn!("CommandCodeSseState: tool_calls limit reached");
                return Ok(None);
            }

            state.tool_call_ids.push(id.to_string());
            state.tool_calls_streamed.push(false);
            let index = (state.tool_call_ids.len() - 1) as u32;

            Ok(Some(make_tool_call_start(
                chunk_id, created, model_name, index, id, name,
            )))
        }
        "tool-input-delta" => {
            let id = val
                .get("id")
                .or_else(|| val.get("toolCallId"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let delta = val
                .get("delta")
                .or_else(|| val.get("argsTextDelta"))
                .or_else(|| val.get("textDelta"))
                .and_then(Value::as_str)
                .unwrap_or("");

            let index = state
                .tool_call_ids
                .iter()
                .position(|i| i == id)
                .unwrap_or_else(|| {
                    if state.tool_call_ids.len() < MAX_COMMANDCODE_TOOL_CALLS {
                        state.tool_call_ids.push(id.to_string());
                        state.tool_calls_streamed.push(true);
                        state.tool_call_ids.len() - 1
                    } else {
                        0
                    }
                });

            if index < state.tool_calls_streamed.len() {
                state.tool_calls_streamed[index] = true;
            }

            Ok(Some(make_tool_call_delta(
                chunk_id,
                created,
                model_name,
                index as u32,
                delta,
            )))
        }
        "tool-call" => {
            let id = val
                .get("toolCallId")
                .or_else(|| val.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("call_cc");
            let name = val
                .get("toolName")
                .or_else(|| val.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let args = val
                .get("input")
                .or_else(|| val.get("args"))
                .map(|a| {
                    if a.is_string() {
                        a.as_str().unwrap_or("").to_string()
                    } else {
                        serde_json::to_string(a).unwrap_or_default()
                    }
                })
                .unwrap_or_default();

            if let Some(index) = state.tool_call_ids.iter().position(|i| i == id) {
                if state
                    .tool_calls_streamed
                    .get(index)
                    .copied()
                    .unwrap_or(false)
                {
                    // Tool call arguments were already streamed via tool-input-delta.
                    // Do not emit another delta chunk with the full arguments to avoid
                    // duplicating arguments downstream in SSE client accumulators.
                    return Ok(None);
                }
                if index < state.tool_calls_streamed.len() {
                    state.tool_calls_streamed[index] = true;
                }
                return Ok(Some(make_tool_call_delta(
                    chunk_id,
                    created,
                    model_name,
                    index as u32,
                    &args,
                )));
            }

            if state.tool_call_ids.len() >= MAX_COMMANDCODE_TOOL_CALLS {
                tracing::warn!("CommandCodeSseState: tool_calls limit reached");
                return Ok(None);
            }

            state.tool_call_ids.push(id.to_string());
            state.tool_calls_streamed.push(true);
            let index = (state.tool_call_ids.len() - 1) as u32;

            let tool_call = json!({
                "index": index,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": args }
            });

            let payload = json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model_name,
                "choices": [{
                    "index": 0,
                    "delta": { "tool_calls": [&tool_call] },
                    "finish_reason": null,
                }]
            });

            Ok(Some(UpstreamSseChunk {
                raw_payload: None,
                payload,
                done: false,
                usage: None,
                stop_reason: None,
                delta_reasoning: None,
                delta_tool_calls: vec![tool_call],
                has_content: true,
            }))
        }
        "finish" => {
            let mut finish_reason = val
                .get("rawFinishReason")
                .or_else(|| val.get("finishReason"))
                .or_else(|| val.get("finish_reason"))
                .and_then(Value::as_str)
                .map(map_commandcode_finish_reason);

            let usage = val
                .get("totalUsage")
                .or_else(|| val.get("usage"))
                .map(|u| {
                    let prompt = u
                        .get("promptTokens")
                        .or_else(|| u.get("prompt_tokens"))
                        .or_else(|| u.get("inputTokens"))
                        .and_then(Value::as_u64);
                    let completion = u
                        .get("completionTokens")
                        .or_else(|| u.get("completion_tokens"))
                        .or_else(|| u.get("outputTokens"))
                        .and_then(Value::as_u64);
                    let total = u
                        .get("totalTokens")
                        .or_else(|| u.get("total_tokens"))
                        .and_then(Value::as_u64)
                        .or_else(|| match (prompt, completion) {
                            (Some(p), Some(c)) => Some(p + c),
                            (Some(p), None) => Some(p),
                            (None, Some(c)) => Some(c),
                            _ => None,
                        });
                    let cached = u
                        .get("cachedInputTokens")
                        .or_else(|| {
                            u.get("inputTokenDetails")
                                .and_then(|d| d.get("cacheReadTokens"))
                        })
                        .and_then(Value::as_u64)
                        .and_then(|c| u32::try_from(c).ok());
                    let prompt_details =
                        cached.map(|c| openproxy_types::message::PromptTokensDetails {
                            cached_tokens: Some(c),
                        });
                    super::build_openai_usage(prompt, completion, total, prompt_details)
                })
                .or_else(|| state.last_finish_step_usage.take());

            if let Some(step_reason) = state.last_finish_step_reason.take()
                && (finish_reason.is_none()
                    || (finish_reason.as_deref() == Some("stop") && step_reason != "stop"))
            {
                finish_reason = Some(step_reason);
            }
            let finish_reason = finish_reason.unwrap_or_else(|| "stop".to_string());

            let payload = json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model_name,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": &finish_reason,
                }],
                "usage": usage.as_ref().map(|u| json!({
                    "prompt_tokens": u.prompt_tokens,
                    "completion_tokens": u.completion_tokens,
                    "total_tokens": u.total_tokens,
                })),
            });

            Ok(Some(UpstreamSseChunk {
                raw_payload: None,
                payload,
                done: true,
                usage,
                stop_reason: Some(finish_reason),
                delta_reasoning: None,
                delta_tool_calls: Vec::new(),
                has_content: false,
            }))
        }
        "finish-step" | "step-finish" => {
            let finish_reason = val
                .get("rawFinishReason")
                .or_else(|| val.get("finishReason"))
                .or_else(|| val.get("finish_reason"))
                .and_then(Value::as_str)
                .map(map_commandcode_finish_reason);

            let usage = val.get("totalUsage").or_else(|| val.get("usage")).map(|u| {
                let prompt = u
                    .get("promptTokens")
                    .or_else(|| u.get("prompt_tokens"))
                    .or_else(|| u.get("inputTokens"))
                    .and_then(Value::as_u64);
                let completion = u
                    .get("completionTokens")
                    .or_else(|| u.get("completion_tokens"))
                    .or_else(|| u.get("outputTokens"))
                    .and_then(Value::as_u64);
                let total = u
                    .get("totalTokens")
                    .or_else(|| u.get("total_tokens"))
                    .and_then(Value::as_u64)
                    .or_else(|| match (prompt, completion) {
                        (Some(p), Some(c)) => Some(p + c),
                        (Some(p), None) => Some(p),
                        (None, Some(c)) => Some(c),
                        _ => None,
                    });
                let cached = u
                    .get("cachedInputTokens")
                    .or_else(|| {
                        u.get("inputTokenDetails")
                            .and_then(|d| d.get("cacheReadTokens"))
                    })
                    .and_then(Value::as_u64)
                    .and_then(|c| u32::try_from(c).ok());
                let prompt_details =
                    cached.map(|c| openproxy_types::message::PromptTokensDetails {
                        cached_tokens: Some(c),
                    });
                super::build_openai_usage(prompt, completion, total, prompt_details)
            });

            if let Some(r) = finish_reason {
                state.last_finish_step_reason = Some(r);
            }
            if let Some(u) = usage {
                state.last_finish_step_usage = Some(u);
            }

            Ok(None)
        }
        "ping" | "heartbeat" | "start" | "start-step" | "step-start" | "text-start"
        | "text-end" | "reasoning-start" | "reasoning-end" | "provider-metadata"
        | "tool-result" => Ok(None),
        _ => Ok(None),
    }
}

/// Accumulates a full Command Code SSE text payload into an `OpenAIResponse`.
pub fn parse_commandcode_sse_to_unary(body_str: &str, model_name: &str) -> Result<OpenAIResponse> {
    let mut state = CommandCodeSseState::default();
    let mut content = String::new();
    let mut reasoning_content: Option<String> = None;
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut finish_reason = None;
    let mut usage = None;

    for line in body_str.lines() {
        let Some(chunk) = parse_commandcode_sse_line(line, "cc_unary", 0, model_name, &mut state)?
        else {
            continue;
        };

        if let Some(r) = chunk.delta_reasoning {
            reasoning_content
                .get_or_insert_with(String::new)
                .push_str(&r);
        }

        for tc in chunk.delta_tool_calls {
            let tc_idx = tc.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            if tc_idx >= tool_calls.len() {
                tool_calls.resize(tc_idx + 1, Value::Null);
            }
            if tool_calls[tc_idx].is_null() {
                tool_calls[tc_idx] = tc;
            } else if let Some(new_args) = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(Value::as_str)
                && let Some(existing_args) = tool_calls[tc_idx]
                    .get_mut("function")
                    .and_then(|f| f.get_mut("arguments"))
                && let Some(s) = existing_args.as_str()
            {
                *existing_args = json!(format!("{s}{new_args}"));
            }
        }

        if let Some(choices) = chunk.payload.get("choices").and_then(Value::as_array)
            && let Some(choice) = choices.first()
        {
            if let Some(c) = choice
                .get("delta")
                .and_then(|d| d.get("content"))
                .and_then(Value::as_str)
            {
                content.push_str(c);
            }
            if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
                finish_reason = Some(fr.to_string());
            }
        }

        if let Some(u) = chunk.usage {
            usage = Some(u);
        }

        if chunk.done {
            break;
        }
    }

    if finish_reason.is_none() {
        finish_reason = state.last_finish_step_reason.take();
    }
    if usage.is_none() {
        usage = state.last_finish_step_usage.take();
    }

    let has_any_data = !content.is_empty() || !tool_calls.is_empty() || reasoning_content.is_some();
    let resolved_finish_reason = finish_reason.or_else(|| {
        if has_any_data {
            Some("stop".to_string())
        } else {
            None
        }
    });

    let mut extra = serde_json::Map::new();
    if let Some(r) = reasoning_content {
        extra.insert("reasoning_content".to_string(), json!(r));
    }

    let message = OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(Value::String(content)),
        name: None,
        tool_call_id: None,
        tool_calls: if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls.into_iter().filter(|v| !v.is_null()).collect())
        },
        extra,
    };

    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    Ok(OpenAIResponse {
        id: "chatcmpl_cc".to_string(),
        object: "chat.completion".to_string(),
        created,
        model: model_name.to_string(),
        choices: vec![OpenAIChoice {
            index: 0,
            message,
            finish_reason: resolved_finish_reason,
        }],
        usage,
    })
}

/// Accumulates a full Command Code SSE text payload into a JSON `Value` representing an OpenAI response.
pub fn parse_commandcode_sse_to_value(body_str: &str, model_name: &str) -> Result<Value> {
    let resp = parse_commandcode_sse_to_unary(body_str, model_name)?;
    serde_json::to_value(&resp)
        .map_err(|e| CoreError::Parse(format!("serialize commandcode openai response: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_commandcode_text_delta() {
        let mut state = CommandCodeSseState::default();
        let line = r#"data: {"type":"text-delta","textDelta":"Hello world"}"#;
        let chunk = parse_commandcode_sse_line(line, "chunk_1", 1000, "claude", &mut state)
            .unwrap()
            .unwrap();

        assert_eq!(
            chunk.payload["choices"][0]["delta"]["content"].as_str(),
            Some("Hello world")
        );
        assert!(chunk.has_content);
    }

    #[test]
    fn test_parse_commandcode_reasoning_delta() {
        let mut state = CommandCodeSseState::default();
        let line = r#"data: {"type":"reasoning-delta","textDelta":"Thinking..."}"#;
        let chunk = parse_commandcode_sse_line(line, "chunk_1", 1000, "claude", &mut state)
            .unwrap()
            .unwrap();

        assert_eq!(
            chunk.payload["choices"][0]["delta"]["reasoning_content"].as_str(),
            Some("Thinking...")
        );
        assert_eq!(chunk.delta_reasoning.as_deref(), Some("Thinking..."));
    }

    #[test]
    fn test_parse_commandcode_tool_calls_stream() {
        let mut state = CommandCodeSseState::default();
        let l1 = r#"data: {"type":"tool-input-start","id":"call_123","name":"calc"}"#;
        let c1 = parse_commandcode_sse_line(l1, "chunk_1", 1000, "claude", &mut state)
            .unwrap()
            .unwrap();
        assert_eq!(c1.delta_tool_calls.len(), 1);
        assert_eq!(c1.delta_tool_calls[0]["function"]["name"], "calc");

        let l2 = r#"data: {"type":"tool-input-delta","id":"call_123","delta":"{\"a\":1}"}"#;
        let c2 = parse_commandcode_sse_line(l2, "chunk_2", 1000, "claude", &mut state)
            .unwrap()
            .unwrap();
        assert_eq!(c2.delta_tool_calls.len(), 1);
        assert_eq!(c2.delta_tool_calls[0]["function"]["arguments"], "{\"a\":1}");

        // When tool-call event follows tool-input-delta, it must not duplicate arguments
        let l3 = r#"data: {"type":"tool-call","id":"call_123","name":"calc","input":{"a":1}}"#;
        let c3 = parse_commandcode_sse_line(l3, "chunk_3", 1000, "claude", &mut state).unwrap();
        assert!(c3.is_none(), "tool-call after deltas must be deduplicated");
    }

    #[test]
    fn test_parse_commandcode_tool_calls_unary_dedup() {
        let stream = "data: {\"type\":\"tool-input-start\",\"id\":\"call_99\",\"name\":\"delegate_task\"}\n\
                      data: {\"type\":\"tool-input-delta\",\"id\":\"call_99\",\"delta\":\"{\\\"action\\\":\\\"list\\\"}\"}\n\
                      data: {\"type\":\"tool-call\",\"id\":\"call_99\",\"name\":\"delegate_task\",\"input\":{\"action\":\"list\"}}\n\
                      data: {\"type\":\"finish\",\"finishReason\":\"tool_calls\"}\n\
                      data: [DONE]\n";
        let resp = parse_commandcode_sse_to_unary(stream, "muse-spark").unwrap();
        let tc = &resp.choices[0].message.tool_calls.as_ref().unwrap()[0];
        assert_eq!(
            tc["function"]["arguments"].as_str().unwrap(),
            "{\"action\":\"list\"}",
            "arguments must not be duplicated into duplicated action list"
        );
    }

    #[test]
    fn test_parse_commandcode_finish() {
        let mut state = CommandCodeSseState::default();
        let line = r#"data: {"type":"finish","finishReason":"stop","usage":{"promptTokens":12,"completionTokens":34,"totalTokens":46}}"#;
        let chunk = parse_commandcode_sse_line(line, "chunk_1", 1000, "claude", &mut state)
            .unwrap()
            .unwrap();

        assert_eq!(
            chunk.payload["choices"][0]["finish_reason"].as_str(),
            Some("stop")
        );
        assert!(chunk.done);
        let usage = chunk.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 34);
        assert_eq!(usage.total_tokens, 46);
    }

    #[test]
    fn test_parse_commandcode_sse_to_unary() {
        let stream = "data: {\"type\":\"reasoning-delta\",\"textDelta\":\"Hmm\"}\n\
                      data: {\"type\":\"text-delta\",\"textDelta\":\"Answer\"}\n\
                      data: {\"type\":\"finish\",\"finishReason\":\"stop\"}\n\
                      data: [DONE]\n";
        let resp = parse_commandcode_sse_to_unary(stream, "claude-sonnet-5").unwrap();
        assert_eq!(resp.model, "claude-sonnet-5");
        assert_eq!(
            resp.choices[0]
                .message
                .content
                .as_ref()
                .and_then(Value::as_str),
            Some("Answer")
        );
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(
            resp.choices[0]
                .message
                .extra
                .get("reasoning_content")
                .and_then(Value::as_str),
            Some("Hmm")
        );
    }

    #[test]
    fn test_parse_commandcode_raw_ndjson() {
        let stream = "{\"type\":\"text-delta\",\"text\":\"Hello from raw NDJSON\"}\n\
                      {\"type\":\"finish\",\"finishReason\":\"stop\",\"totalUsage\":{\"promptTokens\":10,\"outputTokens\":5}}\n";
        let resp = parse_commandcode_sse_to_unary(stream, "muse").unwrap();
        assert_eq!(
            resp.choices[0]
                .message
                .content
                .as_ref()
                .and_then(Value::as_str),
            Some("Hello from raw NDJSON")
        );
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn test_parse_commandcode_real_world_muse_spark_stream() {
        let stream = "{\"type\":\"start\"}\n\
                      {\"type\":\"start-step\",\"request\":{\"body\":{\"model\":\"meta/muse-spark-1.3-contributor\"}}}\n\
                      {\"type\":\"text-start\"}\n\
                      {\"type\":\"text-delta\",\"text\":\"Hey, I'm here. What\"}\n\
                      {\"type\":\"text-delta\",\"text\":\" are we working on?\"}\n\
                      {\"type\":\"text-end\"}\n\
                      {\"type\":\"finish-step\",\"finishReason\":\"length\",\"usage\":{\"inputTokens\":7312,\"outputTokens\":16,\"totalTokens\":7328}}\n\
                      {\"type\":\"finish\",\"finishReason\":\"length\",\"totalUsage\":{\"inputTokens\":7312,\"outputTokens\":16,\"totalTokens\":7328}}\n\
                      {\"type\":\"provider-metadata\",\"providerMetadata\":{}}\n";
        let resp =
            parse_commandcode_sse_to_unary(stream, "meta/muse-spark-1.3-contributor").unwrap();
        assert_eq!(resp.model, "meta/muse-spark-1.3-contributor");
        assert_eq!(
            resp.choices[0]
                .message
                .content
                .as_ref()
                .and_then(Value::as_str),
            Some("Hey, I'm here. What are we working on?")
        );
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("length"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 7312);
        assert_eq!(usage.completion_tokens, 16);
        assert_eq!(usage.total_tokens, 7328);
    }

    #[test]
    fn test_parse_commandcode_finish_step_only() {
        let stream = "{\"type\":\"start\"}\n\
                      {\"type\":\"text-delta\",\"text\":\"Short text\"}\n\
                      {\"type\":\"finish-step\",\"finishReason\":\"stop\",\"usage\":{\"promptTokens\":5,\"completionTokens\":2,\"totalTokens\":7}}\n";
        let resp = parse_commandcode_sse_to_unary(stream, "claude").unwrap();
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.total_tokens, 7);
    }

    #[test]
    fn test_parse_commandcode_empty_content_with_length_finish() {
        let stream = "{\"type\":\"start\"}\n\
                      {\"type\":\"finish-step\",\"finishReason\":\"length\",\"usage\":{\"inputTokens\":100,\"outputTokens\":16}}\n\
                      {\"type\":\"finish\",\"finishReason\":\"length\",\"totalUsage\":{\"inputTokens\":100,\"outputTokens\":16}}\n";
        let resp = parse_commandcode_sse_to_unary(stream, "muse-spark").unwrap();
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("length"));
        assert_eq!(
            resp.choices[0]
                .message
                .content
                .as_ref()
                .and_then(Value::as_str),
            Some("")
        );
    }

    #[test]
    fn test_parse_commandcode_streaming_finish_step_then_finish_single_terminal() {
        let mut state = CommandCodeSseState::default();
        let step_line = "{\"type\":\"finish-step\",\"finishReason\":\"length\",\"usage\":{\"inputTokens\":100,\"outputTokens\":16}}";
        let chunk_step =
            parse_commandcode_sse_line(step_line, "c1", 1000, "muse-spark", &mut state).unwrap();
        // finish-step must NOT emit a finish frame in streaming mode
        assert!(chunk_step.is_none());
        assert_eq!(state.last_finish_step_reason.as_deref(), Some("length"));

        let finish_line = "{\"type\":\"finish\",\"finishReason\":\"length\",\"totalUsage\":{\"inputTokens\":100,\"outputTokens\":16}}";
        let chunk_finish =
            parse_commandcode_sse_line(finish_line, "c1", 1000, "muse-spark", &mut state)
                .unwrap()
                .unwrap();
        // finish MUST emit the single terminal done frame with finish_reason
        assert!(chunk_finish.done);
        assert_eq!(chunk_finish.stop_reason.as_deref(), Some("length"));
        assert_eq!(
            chunk_finish.payload["choices"][0]["finish_reason"].as_str(),
            Some("length")
        );
    }

    #[test]
    fn test_parse_commandcode_error_event() {
        let mut state = CommandCodeSseState::default();
        let err_line = "{\"type\":\"error\",\"message\":\"Rate limit exceeded\"}";
        let res = parse_commandcode_sse_line(err_line, "c1", 1000, "claude", &mut state);
        let Err(err) = res else {
            panic!("expected error result");
        };
        let err_str = err.to_string();
        assert!(err_str.contains("Rate limit exceeded"));
    }

    #[test]
    fn test_parse_commandcode_truly_empty_stream_has_none_finish_reason() {
        let stream = "{\"type\":\"start\"}\n\
                      {\"type\":\"start-step\"}\n";
        let resp = parse_commandcode_sse_to_unary(stream, "empty-model").unwrap();
        // Without any content or finish event, finish_reason must be None so is_empty_response catches it
        assert_eq!(resp.choices[0].finish_reason, None);
        assert_eq!(
            resp.choices[0]
                .message
                .content
                .as_ref()
                .and_then(Value::as_str),
            Some("")
        );
        assert!(crate::dispatcher::unary::is_empty_response(&resp));
    }
}
