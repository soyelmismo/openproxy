//! Devin `GetChatMessage` response-frame parsing.
//!
//! The Connect envelope deframer hands decompressed protobuf payloads here:
//! [`parse_devin_frame`] extracts text/thinking deltas, tool calls,
//! signatures and token usage, and [`parse_devin_trailer_error`] maps the
//! end-of-stream trailer to HTTP-shaped errors.

use bytes::Bytes;

use super::protoc::{self, ProtoReader};

pub use super::protoc::DevinWireError;

/// A streamed tool-call delta (response field 6).
#[derive(Debug, Clone, Default)]
pub struct DevinToolCallDelta {
    pub id: String,
    pub name: String,
    pub arguments: String,
    pub invalid_json_str: String,
    pub invalid_json_err: String,
    pub is_custom: bool,
}

/// Usage counters from response field 7. `prompt_tokens` and
/// `cache_write_tokens` accumulate across frames (`+=`); the rest last-wins.
#[derive(Debug, Clone, Default)]
pub struct DevinUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cache_write_tokens: i64,
    pub cached_tokens: i64,
    pub status_code: u64,
    pub request_id: String,
    pub model_name: String,
}

impl DevinUsage {
    fn merge(&mut self, other: DevinUsage) {
        self.prompt_tokens += other.prompt_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
        if other.completion_tokens != 0 {
            self.completion_tokens = other.completion_tokens;
        }
        if other.cached_tokens != 0 {
            self.cached_tokens = other.cached_tokens;
        }
        if other.status_code != 0 {
            self.status_code = other.status_code;
        }
        if !other.request_id.is_empty() {
            self.request_id = other.request_id;
        }
        if !other.model_name.is_empty() {
            self.model_name = other.model_name;
        }
    }
}

/// One parsed `GetChatMessage` stream frame.
#[derive(Debug, Clone, Default)]
pub struct DevinFrame {
    pub output_id: String,
    pub timestamp_secs: u64,
    pub text: String,
    pub delta_tokens: u64,
    pub stop_reason: u64,
    pub tool_call_deltas: Vec<DevinToolCallDelta>,
    pub thinking_text: String,
    pub signature: Bytes,
    pub signature_type: String,
    pub latency_ms: f64,
    pub message_id: String,
    pub usage: Option<DevinUsage>,
    pub dim_prompt: i64,
    pub dim_completion: i64,
    pub dim_cached: i64,
    pub dim_found: bool,
}

fn reader_str(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn parse_tool_call_delta(reader: &mut ProtoReader<'_>) -> Option<DevinToolCallDelta> {
    let mut delta = DevinToolCallDelta::default();
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (1, protoc::WIRE_LEN) => delta.id = reader_str(reader.bytes_value()?),
            (2, protoc::WIRE_LEN) => delta.name = reader_str(reader.bytes_value()?),
            (3, protoc::WIRE_LEN) => delta.arguments = reader_str(reader.bytes_value()?),
            (4, protoc::WIRE_LEN) => {
                delta.invalid_json_str = reader_str(reader.bytes_value()?);
            }
            (5, protoc::WIRE_LEN) => {
                delta.invalid_json_err = reader_str(reader.bytes_value()?);
            }
            (6, protoc::WIRE_VARINT) => delta.is_custom = reader.varint()? != 0,
            _ => reader.skip(wire)?,
        }
    }
    Some(delta)
}

fn parse_usage(reader: &mut ProtoReader<'_>) -> Option<DevinUsage> {
    let mut usage = DevinUsage::default();
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (2, protoc::WIRE_VARINT) => usage.prompt_tokens += reader.varint()? as i64,
            (3, protoc::WIRE_VARINT) => usage.completion_tokens = reader.varint()? as i64,
            (4, protoc::WIRE_VARINT) => usage.cache_write_tokens += reader.varint()? as i64,
            (5, protoc::WIRE_VARINT) => usage.cached_tokens = reader.varint()? as i64,
            (6, protoc::WIRE_VARINT) => usage.status_code = reader.varint()?,
            (8, protoc::WIRE_LEN) => {
                let hdr = reader.bytes_value()?;
                parse_usage_header(hdr, &mut usage);
            }
            (9, protoc::WIRE_LEN) => usage.model_name = reader_str(reader.bytes_value()?),
            _ => reader.skip(wire)?,
        }
    }
    Some(usage)
}

/// usage.8 header entry `{1: key, 2: value}` → `x-request-id`. Some upstreams
/// ship a bare printable-ASCII request id instead of a key/value pair.
fn parse_usage_header(hdr: &[u8], usage: &mut DevinUsage) {
    let mut sub = ProtoReader::new(hdr);
    let mut key = String::new();
    let mut value = String::new();
    let mut fields_seen = 0usize;
    while let Some((field, wire)) = sub.tag() {
        match (field, wire) {
            (1, protoc::WIRE_LEN) => {
                let Some(bytes) = sub.bytes_value() else {
                    return;
                };
                key = reader_str(bytes);
                fields_seen += 1;
            }
            (2, protoc::WIRE_LEN) => {
                let Some(bytes) = sub.bytes_value() else {
                    return;
                };
                value = reader_str(bytes);
                fields_seen += 1;
            }
            _ => {
                if sub.skip(wire).is_none() {
                    return;
                }
            }
        }
    }
    if (key.eq_ignore_ascii_case("x-request-id") || key.eq_ignore_ascii_case("request-id"))
        && !value.is_empty()
    {
        usage.request_id = value;
    } else if fields_seen == 0
        && !hdr.is_empty()
        && hdr.iter().all(|b| b.is_ascii_graphic() || *b == b' ')
    {
        usage.request_id = reader_str(hdr);
    }
}

/// dimGroup (field 28): `{1: title, 2: repeated metric{5: key, 4: {2: fixed32
/// val}}}`. Only the `"Token Usage"` group counts; first group with a metric
/// wins.
fn parse_dim_group(payload: &[u8], frame: &mut DevinFrame) {
    if frame.dim_found {
        return;
    }
    let mut reader = ProtoReader::new(payload);
    let mut title = String::new();
    let mut metrics: Vec<(String, f32)> = Vec::new();
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (1, protoc::WIRE_LEN) => {
                let Some(bytes) = reader.bytes_value() else {
                    return;
                };
                title = reader_str(bytes);
            }
            (2, protoc::WIRE_LEN) => {
                let Some(metric) = reader.bytes_value() else {
                    return;
                };
                if let Some(m) = parse_dim_metric(metric) {
                    metrics.push(m);
                }
            }
            _ => {
                if reader.skip(wire).is_none() {
                    return;
                }
            }
        }
    }
    if title != "Token Usage" {
        return;
    }
    let mut found = false;
    for (key, val) in metrics {
        let v = val as i64;
        match key.as_str() {
            "input_tokens" => {
                frame.dim_prompt = v;
                found = true;
            }
            "output_tokens" => {
                frame.dim_completion = v;
                found = true;
            }
            "cached_input_tokens" => {
                frame.dim_cached = v;
                found = true;
            }
            _ => {}
        }
    }
    if found {
        frame.dim_found = true;
    }
}

/// metric `{5: key string, 4: message{2: fixed32 value}}` → (key, f32).
fn parse_dim_metric(payload: &[u8]) -> Option<(String, f32)> {
    let mut reader = ProtoReader::new(payload);
    let mut key = String::new();
    let mut value = 0.0f32;
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (5, protoc::WIRE_LEN) => key = reader_str(reader.bytes_value()?),
            (4, protoc::WIRE_LEN) => {
                let inner = reader.bytes_value()?;
                let mut sub = ProtoReader::new(inner);
                while let Some((f, w)) = sub.tag() {
                    if f == 2 && w == protoc::WIRE_32BIT {
                        value = f32::from_bits(sub.fixed32_value()?);
                    } else {
                        sub.skip(w)?;
                    }
                }
            }
            _ => reader.skip(wire)?,
        }
    }
    Some((key, value))
}

/// Parse one (already decompressed) `GetChatMessage` response frame payload.
pub fn parse_devin_frame(payload: &[u8]) -> Result<DevinFrame, DevinWireError> {
    let mut frame = DevinFrame::default();
    let mut reader = ProtoReader::new(payload);
    while let Some((field, wire)) = reader.tag() {
        match (field, wire) {
            (1, protoc::WIRE_LEN) => {
                frame.output_id =
                    reader_str(reader.bytes_value().ok_or(DevinWireError::Truncated)?);
            }
            (2, protoc::WIRE_VARINT) => {
                frame.timestamp_secs = reader.varint().ok_or(DevinWireError::Truncated)?;
            }
            (2, protoc::WIRE_LEN) => {
                let ts = reader.bytes_value().ok_or(DevinWireError::Truncated)?;
                let mut sub = ProtoReader::new(ts);
                while let Some((f, w)) = sub.tag() {
                    if f == 1 && w == protoc::WIRE_VARINT {
                        frame.timestamp_secs = sub.varint().ok_or(DevinWireError::Truncated)?;
                    } else {
                        sub.skip(w).ok_or(DevinWireError::Truncated)?;
                    }
                }
            }
            (3, protoc::WIRE_LEN) => {
                frame.text.push_str(&reader_str(
                    reader.bytes_value().ok_or(DevinWireError::Truncated)?,
                ));
            }
            (4, protoc::WIRE_VARINT) => {
                frame.delta_tokens = reader.varint().ok_or(DevinWireError::Truncated)?;
            }
            (5, protoc::WIRE_VARINT) => {
                frame.stop_reason = reader.varint().ok_or(DevinWireError::Truncated)?;
            }
            (6, protoc::WIRE_LEN) => {
                let raw = reader.bytes_value().ok_or(DevinWireError::Truncated)?;
                let mut sub = ProtoReader::new(raw);
                let delta = parse_tool_call_delta(&mut sub).ok_or(DevinWireError::Truncated)?;
                frame.tool_call_deltas.push(delta);
            }
            (7, protoc::WIRE_LEN) => {
                let raw = reader.bytes_value().ok_or(DevinWireError::Truncated)?;
                let mut sub = ProtoReader::new(raw);
                let usage = parse_usage(&mut sub).ok_or(DevinWireError::Truncated)?;
                match &mut frame.usage {
                    Some(existing) => existing.merge(usage),
                    None => frame.usage = Some(usage),
                }
            }
            (9, protoc::WIRE_LEN) => {
                frame.thinking_text.push_str(&reader_str(
                    reader.bytes_value().ok_or(DevinWireError::Truncated)?,
                ));
            }
            (10, protoc::WIRE_LEN) => {
                frame.signature =
                    Bytes::copy_from_slice(reader.bytes_value().ok_or(DevinWireError::Truncated)?);
            }
            (12, protoc::WIRE_64BIT) => {
                frame.latency_ms =
                    f64::from_bits(reader.fixed64_value().ok_or(DevinWireError::Truncated)?);
            }
            (17, protoc::WIRE_LEN) => {
                frame.message_id =
                    reader_str(reader.bytes_value().ok_or(DevinWireError::Truncated)?);
            }
            (21, protoc::WIRE_LEN) => {
                frame.signature_type =
                    reader_str(reader.bytes_value().ok_or(DevinWireError::Truncated)?);
            }
            (28, protoc::WIRE_LEN) => {
                let raw = reader.bytes_value().ok_or(DevinWireError::Truncated)?;
                parse_dim_group(raw, &mut frame);
            }
            _ => reader.skip(wire).ok_or(DevinWireError::Truncated)?,
        }
    }
    Ok(frame)
}

/// Map a Connect end-stream trailer payload (`{"error":{"code","message"}}`)
/// to `(http_status, message)`. Any present `error.code` maps — unknown codes
/// default to 502 like upstream. `None` for empty/non-JSON/error-free frames.
pub fn parse_devin_trailer_error(payload: &[u8]) -> Option<(u16, String)> {
    let start = payload
        .iter()
        .position(|b| !b.is_ascii_whitespace())
        .unwrap_or(payload.len());
    let end = payload
        .iter()
        .rposition(|b| !b.is_ascii_whitespace())
        .map_or(start, |p| p + 1);
    let trimmed = &payload[start..end.max(start)];
    if trimmed.is_empty() || trimmed == b"{}" {
        return None;
    }
    let json: serde_json::Value = serde_json::from_slice(trimmed).ok()?;
    let error = json.get("error")?;
    let code = error
        .get("code")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let message = error
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let lower_msg = message.to_ascii_lowercase();

    let status = match code.as_str() {
        "invalid_argument" => {
            if lower_msg.contains("internal error") {
                502
            } else {
                400
            }
        }
        "internal" => 502,
        "unauthenticated" => 401,
        "permission_denied" => {
            if lower_msg.contains("high demand") {
                429
            } else {
                403
            }
        }
        "resource_exhausted" => 429,
        "unavailable" => 503,
        "canceled" => 499,
        "deadline_exceeded" => 504,
        "failed_precondition" => {
            if ["quota", "credit", "acu", "exhausted", "limit"]
                .iter()
                .any(|needle| lower_msg.contains(needle))
            {
                429
            } else {
                400
            }
        }
        // Unknown codes default to a bad-gateway like upstream's switch does.
        _ => 502,
    };
    Some((status, format!("devin upstream error ({code}): {message}")))
}

/// Recover the upstream error message from a `body` produced by
/// [`parse_devin_trailer_error`]. Returns `body` unchanged when it does not
/// carry the `devin upstream error (code): ` wrapper (e.g. a raw HTTP error
/// body), so callers can classify on the same text upstream would see.
pub fn devin_error_body_message(body: &str) -> &str {
    body.strip_prefix("devin upstream error (")
        .and_then(|rest| rest.split_once("): "))
        .map_or(body, |(_, msg)| msg)
}
