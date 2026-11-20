//! Accumulates an upstream SSE text response into an `OpenAIResponse`.
//!
//! Used when an upstream provider (such as OpenCode on its free tier)
//! requires `stream: true` even when the downstream client requested a unary
//! (non-streaming) completion, or when an upstream answers an HTTP 200 stream.

use crate::sse_accumulator::ResponseAccumulator;
use crate::translation::OpenAIResponse;
use openproxy_types::ResultExt;
use openproxy_types::TargetFormat;
use openproxy_types::error::{CoreError, Result};

fn feed_chunk_to_acc(
    acc: &mut ResponseAccumulator,
    mut chunk: crate::sse::UpstreamSseChunk,
) -> bool {
    if chunk.is_liveness() {
        // A usage-only frame (Gemini reports `usageMetadata` on metadata-only
        // frames) is not forwarded, but its counters must still reach the fold.
        if let Some(u) = chunk.usage.take() {
            acc.set_usage_with_presence(u, chunk.usage_presence);
        }
        return false;
    }
    let done = chunk.done;
    let usage = chunk.usage.take();
    let usage_presence = chunk.usage_presence;
    let stop_reason = chunk.stop_reason.take();
    let delta_reasoning = chunk.delta_reasoning.take();
    let json_str = chunk.into_json_string();

    if let Some(u) = usage {
        acc.set_usage_with_presence(u, usage_presence);
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
    parse_sse_stream_to_openai_response_with_presence(target_format, body_str, model_name)
        .map(|(response, _)| response)
}

/// Parse an SSE stream buffer into a unary `OpenAIResponse` together with the
/// per-field presence observed by the parsers while folding the frames.
///
/// Presence travels with the chunk and is merged by the accumulator; it is
/// never re-derived from the serialized response, which would mark every
/// zero-filled field as an upstream observation.
pub fn parse_sse_stream_to_openai_response_with_presence(
    target_format: TargetFormat,
    body_str: &str,
    model_name: &str,
) -> Result<(OpenAIResponse, openproxy_types::UsagePresence)> {
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
        TargetFormat::CommandCodeGo => {
            let mut state = crate::sse::CommandCodeSseState::default();
            let mut saw_any_chunk = false;
            for line in body_str.lines() {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(chunk) = crate::sse::parse_commandcode_sse_line(
                    trimmed, chunk_id, created, model_name, &mut state,
                )? {
                    saw_any_chunk = true;
                    if feed_chunk_to_acc(&mut acc, chunk) {
                        break;
                    }
                }
            }
            if let Some(r) = state.last_finish_step_reason.take() {
                acc.set_stop_reason(&r);
            }
            if let Some(u) = state.last_finish_step_usage.take() {
                acc.set_usage_with_presence(u, state.last_finish_step_usage_presence);
            }
            if !saw_any_chunk && acc.content_text().is_empty() {
                // If stream was completely empty of chunks, leave stop_reason None
            } else if !acc.has_stop_reason() && (!acc.content_text().is_empty() || saw_any_chunk) {
                acc.set_stop_reason("stop");
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
            let mut tool_call_index_counter = 0u32;
            for line in body_str.lines() {
                let trimmed = line.trim_end_matches(['\r', '\n']);
                if trimmed.is_empty() {
                    continue;
                }
                if let Some(chunk) = crate::sse::parse_gemini_sse_line(
                    trimmed,
                    chunk_id,
                    created,
                    model_name,
                    &mut tool_call_index_counter,
                )? && feed_chunk_to_acc(&mut acc, chunk)
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
        TargetFormat::SystemOne => {
            return Err(CoreError::Parse(format!(
                "SSE streaming is not supported for target format {target_format}"
            )));
        }
        TargetFormat::Devin => {
            return Err(CoreError::Parse(
                "devin bodies are Connect-RPC frames; use fold_devin_body_to_openai_response"
                    .to_string(),
            ));
        }
    }

    let val = acc.finish(chunk_id, created, model_name);
    let response = serde_json::from_value(val)
        .ctx_parse("failed to parse accumulated sse into OpenAIResponse")?;
    Ok((response, acc.usage_presence()))
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
    fn test_parse_sse_stream_unsupported_formats() {
        let res = parse_sse_stream_to_openai_response(
            TargetFormat::SystemOne,
            "data: test\n\n",
            "systemone-model",
        );
        assert!(
            matches!(res, Err(CoreError::Parse(ref msg)) if msg.contains("SSE streaming is not supported for target format systemone"))
        );
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
        let usage = resp.usage.expect("usage must be accumulated");
        assert_eq!(usage.prompt_tokens, 15);
        assert_eq!(usage.completion_tokens, 5);
    }

    #[test]
    fn test_parse_gemini_sse_stream_with_liveness_chunks() {
        let stream_text = "data: {\"candidates\":[{\"content\":{\"parts\":[{\"thought\":true,\"text\":\"\"}]}}]}\n\ndata: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello from Gemini!\"}]}}]}\n\ndata: {\"candidates\":[{\"content\":{\"parts\":[]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":12,\"candidatesTokenCount\":4,\"totalTokenCount\":16}}\n\ndata: [DONE]\n\n";
        let resp = parse_sse_stream_to_openai_response(
            TargetFormat::Gemini,
            stream_text,
            "gemini-2.5-flash",
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
            "Hello from Gemini!"
        );
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 12);
        assert_eq!(usage.completion_tokens, 4);
    }

    #[test]
    fn test_openai_fold_presence_keeps_partial_usage_and_reports_only_what_arrived() {
        // The final OpenAI frame reports prompt+total but no completion. The
        // fold must keep the counters in the value while the presence says the
        // completion is absent, so the caller estimates it instead of billing
        // a zero as an observation.
        let stream_text = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.4\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.4\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":17,\"total_tokens\":17}}\n\ndata: [DONE]\n\n";
        let (resp, presence) = parse_sse_stream_to_openai_response_with_presence(
            TargetFormat::Openai,
            stream_text,
            "gpt-5.4",
        )
        .unwrap();
        let usage = resp.usage.expect("folded usage");
        assert_eq!(usage.prompt_tokens, 17);
        assert_eq!(usage.total_tokens, 17);
        assert_eq!(
            presence,
            openproxy_types::UsagePresence {
                prompt: true,
                completion: false,
                total: true,
                cached: false,
            },
            "the absent completion must not be reported as an observation"
        );

        // The legacy wrapper keeps working and yields the same response.
        let legacy =
            parse_sse_stream_to_openai_response(TargetFormat::Openai, stream_text, "gpt-5.4")
                .unwrap();
        assert_eq!(
            legacy.usage.expect("legacy usage").prompt_tokens,
            17,
            "the public wrapper must keep its signature and behaviour"
        );
    }

    #[test]
    fn test_openai_fold_presence_reports_none_without_a_usage_block() {
        let stream_text = "data: {\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hi\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let (resp, presence) = parse_sse_stream_to_openai_response_with_presence(
            TargetFormat::Openai,
            stream_text,
            "m",
        )
        .unwrap();
        assert!(resp.usage.is_none());
        assert_eq!(presence, openproxy_types::UsagePresence::none());
    }

    /// Fold a first frame that reports `prompt_tokens: 17` and no total with a
    /// second frame whose usage block is exactly `final_usage`.
    fn fold_two_frames_with_final_usage(
        final_usage: &str,
    ) -> (OpenAIResponse, openproxy_types::UsagePresence) {
        let stream_text = format!(
            "data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.4\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"hi\"}},\"finish_reason\":null}}],\"usage\":{{\"prompt_tokens\":17}}}}\n\n\
             data: {{\"id\":\"c1\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"gpt-5.4\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}],\"usage\":{final_usage}}}\n\n\
             data: [DONE]\n\n"
        );
        parse_sse_stream_to_openai_response_with_presence(
            TargetFormat::Openai,
            &stream_text,
            "gpt-5.4",
        )
        .unwrap()
    }

    #[test]
    fn test_openai_fold_total_absent_keeps_presence_false() {
        // The parent probe: first frame prompt 17, second frame completion 0
        // with no total. The merge must not mark a total as measured; the
        // materialized body may derive it from the observed components.
        let (resp, presence) = fold_two_frames_with_final_usage("{\"completion_tokens\":0}");
        let usage = resp.usage.expect("folded usage");
        assert_eq!(usage.prompt_tokens, 17);
        assert_eq!(usage.completion_tokens, 0);
        assert_eq!(
            usage.total_tokens, 17,
            "the materialized body derives the total from observed components"
        );
        assert!(
            !presence.total,
            "a total that never arrived must not be marked as measured"
        );
        assert!(presence.prompt && presence.completion);
    }

    #[test]
    fn test_openai_fold_explicit_zero_total_is_kept() {
        // The parent probe: the second frame explicitly reports total 0. It
        // must survive as an observed zero, not be replaced by prompt+completion.
        let (resp, presence) =
            fold_two_frames_with_final_usage("{\"completion_tokens\":0,\"total_tokens\":0}");
        let usage = resp.usage.expect("folded usage");
        assert_eq!(usage.prompt_tokens, 17);
        assert_eq!(usage.completion_tokens, 0);
        assert_eq!(
            usage.total_tokens, 0,
            "an explicit zero total is an observation and must not be rewritten"
        );
        assert!(
            presence.total,
            "the explicit zero came from the upstream, so presence is true"
        );
    }

    #[test]
    fn test_openai_fold_positive_total_is_kept() {
        let (resp, presence) =
            fold_two_frames_with_final_usage("{\"completion_tokens\":0,\"total_tokens\":17}");
        let usage = resp.usage.expect("folded usage");
        assert_eq!(usage.prompt_tokens, 17);
        assert_eq!(usage.total_tokens, 17);
        assert!(presence.total);
    }
}
