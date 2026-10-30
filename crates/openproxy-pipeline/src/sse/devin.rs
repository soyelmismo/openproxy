//! Devin Connect-RPC frame → OpenAI chunk translation.
//!
//! Devin upstream frames are length-prefixed protobuf envelopes, not SSE
//! lines. The stream pipeline hands each decoded frame here through
//! `ChunkEvent::Data(flag || payload)`; this module owns the pure
//! `DevinFrame` → [`UpstreamSseChunk`] mapping plus the unary fold used when
//! the client asked for a non-streaming response.

use crate::sse::UpstreamSseChunk;
use crate::translation::OpenAIUsage;
use openproxy_adapters::adapters::devin::wire;
use openproxy_types::error::CoreError;
use openproxy_types::{OpenAIChoice, OpenAIMessage, OpenAIResponse};
use serde_json::{Value, json};

/// Connect end-of-stream trailer flag. Compressed frames keep bit 0 set, so
/// EOS detection is a mask test, not an equality check.
const CONNECT_END_STREAM_MASK: u8 = 0x02;

/// Assigns (or reuses) the OpenAI tool-call `index` for a Devin tool-call id.
/// Returns `(index, first_seen)` — first-seen deltas must carry `id`/`name`
/// per the OpenAI streaming convention.
fn devin_tool_index(state: &mut DevinStreamState, id: &str) -> (u32, bool) {
    if let Some(&index) = state.tool_index_by_id.get(id) {
        return (index, false);
    }
    let index = state.next_tool_index;
    state.next_tool_index += 1;
    state.tool_index_by_id.insert(id.to_string(), index);
    (index, true)
}

/// Per-request Devin stream state. Tool-call deltas reference the call by id
/// and OpenAI deltas need a stable `index`, so ids get sequential indices in
/// arrival order. `last_stop_reason`/`saw_tool_calls` feed the terminal
/// chunk's `finish_reason` because the EOS trailer itself carries no stop
/// reason.
#[derive(Default)]
pub(crate) struct DevinStreamState {
    pub tool_index_by_id: std::collections::HashMap<String, u32>,
    pub next_tool_index: u32,
    pub last_stop_reason: u64,
    pub saw_tool_calls: bool,
}

/// Maps Devin stop reasons (upstream enum ints) to OpenAI finish reasons:
/// 1 (INCOMPLETE) and 3 (MAX_TOKENS) → `length`, 11 (CONTENT_FILTER) →
/// `content_filter`, otherwise `tool_calls` when the stream emitted calls or
/// `stop`.
pub(crate) fn devin_finish_reason(stop_reason: u64, saw_tool_calls: bool) -> &'static str {
    match stop_reason {
        1 | 3 => "length",
        11 => "content_filter",
        _ if saw_tool_calls => "tool_calls",
        _ => "stop",
    }
}

fn devin_usage_to_openai(u: &wire::DevinUsage) -> OpenAIUsage {
    let cached = u.cached_tokens.clamp(0, i64::from(u32::MAX)) as u32;
    let prompt = (u.prompt_tokens + u.cached_tokens).clamp(0, i64::from(u32::MAX)) as u32;
    let completion = u.completion_tokens.clamp(0, i64::from(u32::MAX)) as u32;
    OpenAIUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: prompt.saturating_add(completion),
        prompt_tokens_details: (cached > 0).then_some(openproxy_types::PromptTokensDetails {
            cached_tokens: Some(cached),
        }),
    }
}

fn devin_frame_usage(frame: &wire::DevinFrame) -> Option<OpenAIUsage> {
    if let Some(u) = &frame.usage {
        return Some(devin_usage_to_openai(u));
    }
    if frame.dim_found {
        let cached = frame.dim_cached.clamp(0, i64::from(u32::MAX)) as u32;
        let prompt = (frame.dim_prompt + frame.dim_cached).clamp(0, i64::from(u32::MAX)) as u32;
        let completion = frame.dim_completion.clamp(0, i64::from(u32::MAX)) as u32;
        return Some(OpenAIUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt.saturating_add(completion),
            prompt_tokens_details: (cached > 0).then_some(openproxy_types::PromptTokensDetails {
                cached_tokens: Some(cached),
            }),
        });
    }
    None
}

/// Translates one Devin data frame (flag bits cleared) into the chunk to
/// forward. Returns `None` when the frame carries no client-visible output
/// (timestamp/metadata-only frames keep liveness semantics upstream).
pub(crate) fn devin_frame_to_chunk(
    state: &mut DevinStreamState,
    frame: &wire::DevinFrame,
    chunk_id: &str,
    created: u64,
    model_name: &str,
) -> Option<UpstreamSseChunk> {
    if frame.stop_reason != 0 {
        state.last_stop_reason = frame.stop_reason;
    }

    let usage = devin_frame_usage(frame);
    let mut delta = serde_json::Map::new();
    let mut has_content = false;

    if !frame.text.is_empty() {
        delta.insert("content".into(), Value::String(frame.text.clone()));
        has_content = true;
    }
    if !frame.thinking_text.is_empty() {
        delta.insert(
            "reasoning_content".into(),
            Value::String(frame.thinking_text.clone()),
        );
        has_content = true;
    }

    let mut delta_tool_calls: Vec<Value> = Vec::new();
    if !frame.tool_call_deltas.is_empty() {
        let mut calls = Vec::with_capacity(frame.tool_call_deltas.len());
        for tc in &frame.tool_call_deltas {
            // Non-portable upstream ids are namespaced before the client sees
            // them (upstream `cpa_tid_v1_` encoding); request-side matching
            // decodes them back in `restore_devin_tool_call_ids`.
            let client_id = wire::encode_devin_tool_call_id(&tc.id);
            let (index, first_seen) = devin_tool_index(state, &client_id);
            state.saw_tool_calls = true;
            has_content = true;
            let mut function = serde_json::Map::new();
            if first_seen && !tc.name.is_empty() {
                function.insert("name".into(), Value::String(tc.name.clone()));
            }
            if !tc.arguments.is_empty() {
                function.insert("arguments".into(), Value::String(tc.arguments.clone()));
                has_content = true;
            }
            let mut call = serde_json::Map::new();
            call.insert("index".into(), json!(index));
            if first_seen {
                call.insert("id".into(), Value::String(client_id));
                call.insert("type".into(), json!("function"));
            }
            call.insert("function".into(), Value::Object(function));
            calls.push(Value::Object(call.clone()));
            delta_tool_calls.push(Value::Object(call));
        }
        delta.insert("tool_calls".into(), Value::Array(calls));
    }

    if delta.is_empty() && usage.is_none() && frame.stop_reason == 0 {
        return None;
    }

    let payload = json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model_name,
        "choices": [{
            "index": 0,
            "delta": Value::Object(delta),
            "finish_reason": null
        }]
    });

    Some(UpstreamSseChunk {
        raw_payload: None,
        payload,
        done: false,
        usage,
        stop_reason: None,
        delta_reasoning: (!frame.thinking_text.is_empty()).then(|| frame.thinking_text.clone()),
        delta_tool_calls,
        has_content,
    })
}

/// Builds the terminal chunk emitted when the EOS trailer arrives cleanly.
/// `finish_reason` comes from accumulated stream state because the trailer
/// carries only status metadata.
pub(crate) fn devin_done_chunk(
    state: &DevinStreamState,
    chunk_id: &str,
    created: u64,
    model_name: &str,
) -> UpstreamSseChunk {
    let finish_reason = devin_finish_reason(state.last_stop_reason, state.saw_tool_calls);
    let payload = json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model_name,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": finish_reason
        }]
    });
    UpstreamSseChunk {
        raw_payload: None,
        payload,
        done: true,
        usage: None,
        stop_reason: Some(finish_reason.to_string()),
        delta_reasoning: None,
        delta_tool_calls: Vec::new(),
        has_content: false,
    }
}

/// Folded unary shape: text is concatenated, thinking lands on
/// `extra.reasoning_content`, tool calls are assembled per-id in index order.
pub(crate) fn fold_devin_body_to_openai_response(
    body: &[u8],
    model_name: &str,
) -> Result<OpenAIResponse, CoreError> {
    let mut deframer = wire::DevinDeframer::new();
    deframer.push(body);

    let mut state = DevinStreamState::default();
    let mut text = String::new();
    let mut thinking = String::new();
    let mut usage: Option<OpenAIUsage> = None;
    let mut created = 0_u64;
    let mut response_id = String::new();
    let mut response_model: Option<String> = None;
    let mut tool_calls: std::collections::BTreeMap<u32, (String, String, String)> =
        std::collections::BTreeMap::new();

    loop {
        let frame_event = deframer
            .next_frame()
            .map_err(|e| CoreError::UpstreamConnection(format!("devin frame: {e}")))?;
        let Some(frame_event) = frame_event else {
            break;
        };
        if frame_event.flag & CONNECT_END_STREAM_MASK != 0 {
            if let Some((status, msg)) = wire::parse_devin_trailer_error(&frame_event.payload) {
                let class = crate::error_classification::classify_upstream_error(status, &msg);
                return Err(CoreError::UpstreamError {
                    status,
                    provider: "devin".into(),
                    model: model_name.to_string(),
                    body: msg,
                    is_proxy_rotated: false,
                    class,
                    is_hard_skip: class.is_hard_skip(),
                });
            }
            continue;
        }
        let frame = wire::parse_devin_frame(&frame_event.payload)
            .map_err(|e| CoreError::UpstreamConnection(format!("devin frame parse: {e}")))?;
        text.push_str(&frame.text);
        thinking.push_str(&frame.thinking_text);
        if frame.timestamp_secs > 0 {
            created = frame.timestamp_secs;
        }
        if response_id.is_empty() {
            response_id = if !frame.message_id.is_empty() {
                frame.message_id.clone()
            } else {
                frame.output_id.clone()
            };
        }
        if let Some(u) = devin_frame_usage(&frame) {
            usage = Some(u);
        }
        if let Some(u) = &frame.usage
            && !u.model_name.is_empty()
        {
            response_model = Some(u.model_name.clone());
        }
        if frame.stop_reason != 0 {
            state.last_stop_reason = frame.stop_reason;
        }
        for tc in &frame.tool_call_deltas {
            let (index, first_seen) = devin_tool_index(&mut state, &tc.id);
            state.saw_tool_calls = true;
            let entry = tool_calls.entry(index).or_default();
            if first_seen {
                // Same namespacing as the streaming path so follow-up
                // requests round-trip through `restore_devin_tool_call_ids`.
                entry.0 = wire::encode_devin_tool_call_id(&tc.id);
                entry.1 = tc.name.clone();
            }
            entry.2.push_str(&tc.arguments);
        }
    }

    if deframer.has_partial() {
        return Err(CoreError::UpstreamConnection(
            "devin stream truncated mid-frame".into(),
        ));
    }

    if response_id.is_empty() {
        response_id = format!("chatcmpl_{}", uuid::Uuid::new_v4());
    }
    if created == 0 {
        created = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
    }

    let mut extra = serde_json::Map::new();
    if !thinking.is_empty() {
        extra.insert("reasoning_content".into(), Value::String(thinking));
    }
    let calls: Vec<Value> = tool_calls
        .into_values()
        .map(|(id, name, arguments)| {
            json!({
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": arguments }
            })
        })
        .collect();

    Ok(OpenAIResponse {
        id: response_id,
        object: "chat.completion".to_string(),
        created,
        model: response_model.unwrap_or_else(|| model_name.to_string()),
        choices: vec![OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(Value::String(text)),
                name: None,
                tool_call_id: None,
                tool_calls: (!calls.is_empty()).then_some(calls),
                extra,
            },
            finish_reason: Some(
                devin_finish_reason(state.last_stop_reason, state.saw_tool_calls).to_string(),
            ),
        }],
        usage,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_adapters::adapters::devin::protoc;

    fn frame_event(payload: &[u8]) -> Vec<u8> {
        wire::wrap_connect_envelope(payload)
    }

    fn text_frame(text: &str) -> Vec<u8> {
        let mut p = Vec::new();
        protoc::append_string(&mut p, 3, text);
        frame_event(&p)
    }

    fn tool_call_frame(id: &str, name: &str, args: &str) -> Vec<u8> {
        let mut tc = Vec::new();
        protoc::append_string(&mut tc, 1, id);
        protoc::append_string(&mut tc, 2, name);
        protoc::append_string(&mut tc, 3, args);
        let mut p = Vec::new();
        protoc::append_message(&mut p, 6, &tc);
        frame_event(&p)
    }

    fn usage_frame(prompt: u64, completion: u64) -> Vec<u8> {
        let mut u = Vec::new();
        protoc::append_varint_field(&mut u, 2, prompt);
        protoc::append_varint_field(&mut u, 3, completion);
        let mut p = Vec::new();
        protoc::append_message(&mut p, 7, &u);
        frame_event(&p)
    }

    fn stop_frame(reason: u64) -> Vec<u8> {
        let mut p = Vec::new();
        protoc::append_varint_field(&mut p, 5, reason);
        frame_event(&p)
    }

    fn eos_trailer(payload: &[u8]) -> Vec<u8> {
        let mut out = vec![0x02];
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn base_frame() -> wire::DevinFrame {
        wire::DevinFrame::default()
    }

    #[test]
    fn chunk_maps_text_thinking_and_usage() {
        let mut state = DevinStreamState::default();
        let frame = wire::DevinFrame {
            text: "hello".into(),
            thinking_text: "hmm".into(),
            usage: Some(wire::DevinUsage {
                prompt_tokens: 10,
                completion_tokens: 4,
                cached_tokens: 3,
                ..Default::default()
            }),
            ..base_frame()
        };
        let chunk = devin_frame_to_chunk(&mut state, &frame, "id1", 7, "m").unwrap();
        assert!(chunk.has_content);
        assert_eq!(chunk.delta_reasoning.as_deref(), Some("hmm"));
        let delta = &chunk.payload["choices"][0]["delta"];
        assert_eq!(delta["content"], "hello");
        assert_eq!(delta["reasoning_content"], "hmm");
        let usage = chunk.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 13); // prompt + cached merged
        assert_eq!(usage.completion_tokens, 4);
        assert_eq!(
            usage.prompt_tokens_details.and_then(|d| d.cached_tokens),
            Some(3)
        );
    }

    #[test]
    fn chunk_metadata_only_frame_is_skipped() {
        let mut state = DevinStreamState::default();
        let frame = wire::DevinFrame {
            timestamp_secs: 123,
            ..base_frame()
        };
        assert!(devin_frame_to_chunk(&mut state, &frame, "id", 0, "m").is_none());
    }

    #[test]
    fn chunk_tool_calls_get_stable_indices_and_encoded_ids() {
        let mut state = DevinStreamState::default();
        let frame = wire::DevinFrame {
            tool_call_deltas: vec![
                wire::DevinToolCallDelta {
                    id: "call_1".into(),
                    name: "exec".into(),
                    arguments: "{\"a\":".into(),
                    ..Default::default()
                },
                wire::DevinToolCallDelta {
                    id: "call_2".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                    ..Default::default()
                },
            ],
            ..base_frame()
        };
        let chunk = devin_frame_to_chunk(&mut state, &frame, "id", 0, "m").unwrap();
        let calls = chunk.payload["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(calls[0]["index"], 0);
        assert_eq!(calls[1]["index"], 1);
        assert_eq!(calls[0]["id"], "call_1");
        assert_eq!(calls[0]["function"]["name"], "exec");
        assert_eq!(calls[0]["type"], "function");

        // Second delta for call_1 reuses index 0 and omits id/type.
        let frame2 = wire::DevinFrame {
            tool_call_deltas: vec![wire::DevinToolCallDelta {
                id: "call_1".into(),
                arguments: "1}".into(),
                ..Default::default()
            }],
            ..base_frame()
        };
        let chunk2 = devin_frame_to_chunk(&mut state, &frame2, "id", 0, "m").unwrap();
        let calls2 = chunk2.payload["choices"][0]["delta"]["tool_calls"]
            .as_array()
            .unwrap();
        assert_eq!(calls2[0]["index"], 0);
        assert!(calls2[0].get("id").is_none());
        assert_eq!(calls2[0]["function"]["arguments"], "1}");
    }

    #[test]
    fn chunk_tool_call_id_gets_encoded_when_not_portable() {
        let mut state = DevinStreamState::default();
        let frame = wire::DevinFrame {
            tool_call_deltas: vec![wire::DevinToolCallDelta {
                id: "weird id with spaces!!".into(),
                name: "f".into(),
                arguments: "{}".into(),
                ..Default::default()
            }],
            ..base_frame()
        };
        let chunk = devin_frame_to_chunk(&mut state, &frame, "id", 0, "m").unwrap();
        let id = chunk.payload["choices"][0]["delta"]["tool_calls"][0]["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(id.starts_with("cpa_tid_v1_"));
        // Round-trip: the request-side restore decodes it back.
        assert_eq!(
            wire::decode_devin_tool_call_id(&id),
            "weird id with spaces!!"
        );
    }

    #[test]
    fn done_chunk_finish_reason_mapping() {
        let mut state = DevinStreamState::default();
        // No stop reason, no tools → stop.
        let c = devin_done_chunk(&state, "id", 0, "m");
        assert_eq!(c.payload["choices"][0]["finish_reason"], "stop");
        assert!(c.done);

        // Stop reason 3 (MAX_TOKENS) → length.
        state.last_stop_reason = 3;
        let c = devin_done_chunk(&state, "id", 0, "m");
        assert_eq!(c.payload["choices"][0]["finish_reason"], "length");

        // Stop reason 11 → content_filter.
        state.last_stop_reason = 11;
        let c = devin_done_chunk(&state, "id", 0, "m");
        assert_eq!(c.payload["choices"][0]["finish_reason"], "content_filter");

        // Tool calls with generic stop → tool_calls.
        state.last_stop_reason = 0;
        state.saw_tool_calls = true;
        let c = devin_done_chunk(&state, "id", 0, "m");
        assert_eq!(c.payload["choices"][0]["finish_reason"], "tool_calls");
    }

    #[test]
    fn unary_fold_text_tools_usage_and_finish() {
        let mut body = Vec::new();
        body.extend_from_slice(&text_frame("partial "));
        body.extend_from_slice(&tool_call_frame("call_9", "exec", "{\"cmd\":\"ls\"}"));
        body.extend_from_slice(&text_frame("done"));
        body.extend_from_slice(&usage_frame(10, 5));
        body.extend_from_slice(&stop_frame(0));
        body.extend_from_slice(&eos_trailer(b"{}"));

        let resp = fold_devin_body_to_openai_response(&body, "devin-model").unwrap();
        assert_eq!(resp.model, "devin-model");
        let choice = &resp.choices[0];
        assert_eq!(choice.message.content, Some(json!("partial done")));
        let calls = choice.message.tool_calls.as_ref().unwrap();
        assert_eq!(calls[0]["function"]["name"], "exec");
        assert_eq!(calls[0]["function"]["arguments"], "{\"cmd\":\"ls\"}");
        assert_eq!(choice.finish_reason.as_deref(), Some("tool_calls"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
    }

    #[test]
    fn unary_fold_thinking_lands_in_reasoning_content() {
        let mut p = Vec::new();
        protoc::append_string(&mut p, 9, "reasoning ");
        protoc::append_string(&mut p, 9, "chunk2");
        let mut body = frame_event(&p);
        body.extend_from_slice(&eos_trailer(b"{}"));

        let resp = fold_devin_body_to_openai_response(&body, "m").unwrap();
        assert_eq!(
            resp.choices[0].message.extra["reasoning_content"],
            json!("reasoning chunk2")
        );
    }

    #[test]
    fn unary_fold_trailer_error_becomes_upstream_error() {
        let mut body = text_frame("x");
        body.extend_from_slice(&eos_trailer(
            br#"{"error":{"code":"resource_exhausted","message":"quota exceeded"}}"#,
        ));
        let err = fold_devin_body_to_openai_response(&body, "m").unwrap_err();
        match err {
            CoreError::UpstreamError { status, .. } => assert_eq!(status, 429),
            other => panic!("expected UpstreamError, got {other:?}"),
        }
    }

    #[test]
    fn unary_fold_truncated_body_errors() {
        let mut body = text_frame("abc");
        body.truncate(body.len() - 2);
        let err = fold_devin_body_to_openai_response(&body, "m").unwrap_err();
        assert!(matches!(err, CoreError::UpstreamConnection(_)));
    }
}
