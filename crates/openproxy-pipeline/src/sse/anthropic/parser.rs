//! Byte and line parser for Anthropic SSE stream lines.

use super::super::MAX_SSE_EVENT_TYPE_BYTES;
use openproxy_types::error::Result;

/// Parse a single line from an Anthropic SSE stream.
/// Anthropic SSE uses `event:` lines to set the event type, then `data:` lines
/// with the payload. This function tracks state across calls.
///
/// Returns `Ok(Some(payload))` when a complete data payload is found,
/// `Ok(None)` for non-data lines, and `Err` for parse failures.
pub fn parse_anthropic_sse_stream_line(
    line: &str,
    current_event: &mut Option<String>,
) -> Result<Option<String>> {
    let line = line.trim_end_matches('\r');

    if line.is_empty() {
        // Empty line = end of event, reset
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
            // Truncate instead of erroring to keep the stream alive.
            *current_event = None;
            return Ok(None);
        }
        *current_event = Some(event_type.to_string());
        return Ok(None);
    }

    if let Some(data) = line.strip_prefix("data: ") {
        let event_type = current_event.as_deref().unwrap_or("unknown");
        // Return the event type alongside the data so the caller can translate
        // Format: "event_type\ndata_payload"
        return Ok(Some(format!("{event_type}\n{data}")));
    }

    // Ignore id:, retry:, comments, etc.
    Ok(None)
}
