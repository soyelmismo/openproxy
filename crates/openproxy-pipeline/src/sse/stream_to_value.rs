//! Accumulates an upstream SSE text response into an `OpenAIResponse`.
//!
//! Used when an upstream provider (such as OpenCode on its free tier)
//! requires `stream: true` even when the downstream client requested a unary
//! (non-streaming) completion, or when an upstream answers an HTTP 200 stream.

use crate::sse_accumulator::ResponseAccumulator;
use crate::translation::OpenAIResponse;
use openproxy_types::TargetFormat;
use openproxy_types::error::{CoreError, Result};

fn feed_chunk_to_acc(
    acc: &mut ResponseAccumulator,
    mut chunk: crate::sse::UpstreamSseChunk,
) -> bool {
    let done = chunk.done;
    let usage = chunk.usage.take();
    let stop_reason = chunk.stop_reason.take();
    let delta_reasoning = chunk.delta_reasoning.take();
    let json_str = chunk.into_json_string();

    if let Some(u) = usage {
        acc.set_usage(u);
    }
    if let Some(sr) = stop_reason {
        acc.set_stop_reason(&sr);
    }
    if let Some(dr) = delta_reasoning {
        acc.append_reasoning(&dr);
    }
    acc.append_openai_raw(&json_str);
    done
}

/// Parse an SSE stream buffer into a unary `OpenAIResponse`.
pub fn parse_sse_stream_to_openai_response(
    target_format: TargetFormat,
    body_str: &str,
    model_name: &str,
) -> Result<OpenAIResponse> {
    if target_format == TargetFormat::CommandCodeGo {
        return crate::sse::parse_commandcode_sse_to_unary(body_str, model_name);
    }

    let mut acc = ResponseAccumulator::new();
    let chunk_id = "chatcmpl_unary";
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());

    match target_format {
        TargetFormat::Openai => {
            for line in body_str.lines() {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(chunk) = crate::sse::parse_openai_sse_line(trimmed)?
                    && feed_chunk_to_acc(&mut acc, chunk)
                {
                    break;
                }
            }
        }
        TargetFormat::Anthropic => {
            let mut current_event_type: Option<String> = None;
            let mut tool_use_acc: Option<crate::sse::AnthropicToolUseAccumulator> = None;
            let mut tool_call_index_counter = 0u32;

            for line in body_str.lines() {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    continue;
                }
                let Some(payload) =
                    crate::sse::parse_anthropic_sse_stream_line(trimmed, &mut current_event_type)?
                else {
                    continue;
                };

                if let Some(chunk) = crate::sse::translate_anthropic_sse_event(
                    &payload,
                    chunk_id,
                    created,
                    model_name,
                    &mut tool_use_acc,
                    &mut tool_call_index_counter,
                )? && feed_chunk_to_acc(&mut acc, chunk)
                {
                    break;
                }
            }
        }
        TargetFormat::Responses => {
            let mut state = crate::sse::ResponsesSseState::default();
            for line in body_str.lines() {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(chunk) = crate::sse::parse_responses_sse_stream_line(
                    trimmed, chunk_id, created, model_name, &mut state,
                )? && feed_chunk_to_acc(&mut acc, chunk)
                {
                    break;
                }
            }
        }
        TargetFormat::Gemini => {
            for line in body_str.lines() {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(chunk) =
                    crate::sse::parse_gemini_sse_line(trimmed, chunk_id, created, model_name)?
                    && feed_chunk_to_acc(&mut acc, chunk)
                {
                    break;
                }
            }
        }
        TargetFormat::Atomesus => {
            for line in body_str.lines() {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(chunk) =
                    crate::sse::parse_atomesus_sse_line(trimmed, chunk_id, created, model_name)?
                    && feed_chunk_to_acc(&mut acc, chunk)
                {
                    break;
                }
            }
        }
        TargetFormat::CommandCodeGo | TargetFormat::SystemOne => unreachable!(),
    }

    let val = acc.finish(chunk_id, created, model_name);
    serde_json::from_value(val).map_err(|e| {
        CoreError::Parse(format!(
            "failed to parse accumulated sse into OpenAIResponse: {e}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_openai_sse_stream() {
        let stream_text = "data: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1694268190,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1694268190,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\" world!\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"chatcmpl-123\",\"object\":\"chat.completion.chunk\",\"created\":1694268190,\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\ndata: [DONE]\n\n";
        let resp = parse_sse_stream_to_openai_response(TargetFormat::Openai, stream_text, "gpt-4")
            .unwrap();

        assert_eq!(
            resp.choices[0]
                .message
                .content
                .as_ref()
                .unwrap()
                .as_str()
                .unwrap(),
            "Hello world!"
        );
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 2);
    }

    #[test]
    fn test_parse_anthropic_sse_stream() {
        let stream_text = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_123\",\"type\":\"message\",\"role\":\"assistant\",\"model\":\"claude-3-5-sonnet\",\"usage\":{\"input_tokens\":15,\"output_tokens\":1}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hi from Claude!\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":5}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let resp = parse_sse_stream_to_openai_response(
            TargetFormat::Anthropic,
            stream_text,
            "claude-3-5-sonnet",
        )
        .unwrap();

        assert_eq!(
            resp.choices[0]
                .message
                .content
                .as_ref()
                .unwrap()
                .as_str()
                .unwrap(),
            "Hi from Claude!"
        );
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("end_turn"));
    }
}
