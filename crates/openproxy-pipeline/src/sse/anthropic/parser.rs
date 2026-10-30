//! Byte and line parser for Anthropic SSE stream lines.

use super::super::MAX_SSE_EVENT_TYPE_BYTES;
use openproxy_types::error::Result;

/// Parse one Anthropic SSE stream line, tracking the current event type
/// across calls.
///
/// Returns `Ok(Some("event_type\ndata_payload"))` on a `data:` line and
/// `Ok(None)` for every other line, including `event:` lines.
pub fn parse_anthropic_sse_stream_line(
    line: &str,
    current_event: &mut Option<String>,
) -> Result<Option<String>> {
    let line = line.trim_end_matches('\r');

    if line.is_empty() {
        *current_event = None;
        return Ok(None);
    }

    if let Some(event_type) = line.strip_prefix("event: ") {
        let event_type = event_type.trim();
        if event_type.len() > MAX_SSE_EVENT_TYPE_BYTES {
            tracing::warn!(
                actual_len = event_type.len(),
                max = MAX_SSE_EVENT_TYPE_BYTES,
                "SSE event type exceeds maximum length — truncating"
            );
            // Truncating keeps the stream alive.
            *current_event = None;
            return Ok(None);
        }
        *current_event = Some(event_type.to_string());
        return Ok(None);
    }

    if let Some(data) = line.strip_prefix("data: ") {
        // MiniMax (and some Anthropic-compatible gateways) emit `data:` lines
        // without a preceding `event:` line; the event type then lives inside
        // the payload's own `type` field. Inferring it keeps the same dispatch
        // downstream instead of dropping every event as `unknown`.
        let event_type = current_event.as_deref().unwrap_or_else(|| {
            serde_json::from_str::<serde_json::Value>(data)
                .ok()
                .map_or("unknown", |v| {
                    let by_type = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    // The inferred type must not outlive `data`; leak a small
                    // fixed set via match so we can keep returning &str.
                    match by_type {
                        "message_start" => "message_start",
                        "message_delta" => "message_delta",
                        "message_stop" => "message_stop",
                        "content_block_start" => "content_block_start",
                        "content_block_delta" => "content_block_delta",
                        "content_block_stop" => "content_block_stop",
                        "message" => "message",
                        "error" => "error",
                        "ping" => "ping",
                        // MiniMax's whole-message envelope carries `content`
                        // (and `base_resp`) but no `type` tag — sniff it.
                        _ if v.get("content").is_some_and(|c| c.is_array()) => "message",
                        _ => "unknown",
                    }
                })
        });
        return Ok(Some(format!("{event_type}\n{data}")));
    }

    // id:, retry: and comment lines carry no event.
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MiniMax streams `data:` lines without an `event:` prefix; the payload's
    /// own `type` field must drive dispatch, not the (absent) event line.
    #[test]
    fn data_only_infers_event_type_from_payload() {
        let mut event: Option<String> = None;
        let out = parse_anthropic_sse_stream_line(
            r#"data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            &mut event,
        )
        .expect("must parse");
        assert_eq!(
            out.as_deref(),
            Some(
                r#"message_delta
{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#
            )
        );
    }

    /// Whole-message MiniMax envelope without `type` tag: `content` array +
    /// `base_resp` still resolves to a `message` event instead of `unknown`.
    #[test]
    fn data_only_sniffs_untagged_message_envelope() {
        let mut event: Option<String> = None;
        let out = parse_anthropic_sse_stream_line(
            r#"data: {"base_resp":{"status_code":0},"role":"assistant","content":[{"type":"text","text":"h"}]}"#,
            &mut event,
        )
        .expect("must parse");
        assert!(out.is_some_and(|p| p.starts_with("message\n")));
    }

    /// An explicit `event:` line always wins over payload inference.
    #[test]
    fn explicit_event_wins_over_payload_type() {
        let mut event: Option<String> = Some("content_block_delta".to_string());
        let out =
            parse_anthropic_sse_stream_line(r#"data: {"type":"message","content":[]}"#, &mut event)
                .expect("must parse");
        assert!(out.is_some_and(|p| p.starts_with("content_block_delta\n")));
    }

    /// Unrelated `data:` JSON (no type, no content array) keeps the previous
    /// `unknown` behaviour: dropped downstream, never an error.
    #[test]
    fn data_only_without_type_falls_back_to_unknown() {
        let mut event: Option<String> = None;
        let out =
            parse_anthropic_sse_stream_line(r#"data: {"foo":1}"#, &mut event).expect("must parse");
        assert!(out.is_some_and(|p| p.starts_with("unknown\n")));
    }
}
