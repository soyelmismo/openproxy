//! Devin Connect-RPC wire layer.
//!
//! Connect envelope framing (`[1B flag][4B BE len][payload]`), the
//! `GetChatMessageRequest` protobuf builder, the streaming response-frame
//! parser, Connect trailer error mapping and the request-identity helpers
//! (sentry trace, device fingerprint, Basic auth) — all reverse-engineered
//! from CLIProxyAPI's Devin executor.

use bytes::{Bytes, BytesMut};
use openproxy_types::{CoreError, OpenAIRequest};

use super::protoc;
use super::{models, tools, turn};

pub use super::protoc::DevinWireError;

/// Connect frame flag: plain data frame.
pub const CONNECT_FLAG_DATA: u8 = 0x00;
/// Connect frame flag bit: payload is gzip-compressed.
pub const CONNECT_FLAG_COMPRESSED: u8 = 0x01;
/// Connect frame flag bit: end-of-stream trailer frame.
pub const CONNECT_FLAG_END_STREAM: u8 = 0x02;

/// Maximum accepted *compressed* frame payload (16 MiB).
pub const MAX_COMPRESSED_FRAME: usize = 16 * 1024 * 1024;
/// Maximum accepted *decompressed* frame payload (64 MiB).
pub const MAX_DECOMPRESSED_FRAME: usize = 64 * 1024 * 1024;

/// Length of the sentry device fingerprint sent in ClientMetadata.31.
pub const DEVICE_FINGERPRINT_LEN: usize = 732;

/// Lowest temperature the upstream Devin sampler tolerates. The backend
/// divides by temperature when sampling, so a verbatim `0` or a negative
/// value makes it return `invalid_argument` on every model. `0.0001` keeps
/// the output deterministic in practice while staying inside the accepted
/// range (any value `> 0`, however small, passes).
pub const DEVIN_MIN_TEMPERATURE: f32 = 0.0001;

// ---------------------------------------------------------------------------
// Identity helpers
// ---------------------------------------------------------------------------

/// `Authorization: Basic <token>-<token>` — Devin duplicates the session
/// token under the (non-standard) Basic scheme.
pub fn basic_auth(token: &str) -> String {
    format!("Basic {token}-{token}")
}

/// `<32 lowercase hex>-<16 lowercase hex>-1` sentry trace header.
pub fn sentry_trace_header() -> String {
    let trace = uuid::Uuid::new_v4().simple().to_string();
    let span_full = uuid::Uuid::new_v4().simple().to_string();
    let span: String = span_full.chars().take(16).collect();
    format!("{trace}-{span}-1")
}

/// Lowercase hex encoding without pulling in another dependency.
fn hex_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 732 lowercase hex characters — the Devin device fingerprint is a fixed
/// length, format-free random hex blob.
///
/// A non-empty `seed` pins a deterministic fingerprint
/// (`sha256("{seed}-{counter}")` hex blocks concatenated) for callers that
/// must keep one device identity across requests; an empty seed yields a
/// fresh random fingerprint per call.
pub fn device_fingerprint(seed: &str) -> String {
    let seed = seed.trim();
    if seed.is_empty() {
        // 23 UUIDs * 32 hex = 736 chars, truncated to the required 732.
        let mut out = String::with_capacity(DEVICE_FINGERPRINT_LEN);
        while out.len() < DEVICE_FINGERPRINT_LEN {
            out.push_str(&uuid::Uuid::new_v4().simple().to_string());
        }
        return out.chars().take(DEVICE_FINGERPRINT_LEN).collect();
    }
    let mut out = String::with_capacity(DEVICE_FINGERPRINT_LEN);
    let mut counter = 0u64;
    while out.len() < DEVICE_FINGERPRINT_LEN {
        use sha2::Digest;
        let h = sha2::Sha256::digest(format!("{seed}-{counter}"));
        out.push_str(&hex_lower(&h));
        counter += 1;
    }
    out.chars().take(DEVICE_FINGERPRINT_LEN).collect()
}

// ---------------------------------------------------------------------------
// gzip container handling
// ---------------------------------------------------------------------------

const GZIP_MAGIC: [u8; 2] = [0x1f, 0x8b];
const GZIP_CM_DEFLATE: u8 = 8;
const GZIP_FHCRC: u8 = 0x02;
const GZIP_FEXTRA: u8 = 0x04;
const GZIP_FNAME: u8 = 0x08;
const GZIP_FCOMMENT: u8 = 0x10;
const GZIP_FIXED_HEADER_LEN: usize = 10;

/// Decompress a gzip container: validate the header, skip optional fields and
/// raw-inflate the body. The trailing CRC32+ISIZE trailer is left unread —
/// `decompress_to_vec_with_limit` stops at the deflate stream end.
pub fn gunzip(input: &[u8]) -> Result<Vec<u8>, DevinWireError> {
    if input.len() < GZIP_FIXED_HEADER_LEN {
        return Err(DevinWireError::Gzip("truncated header".into()));
    }
    if input[0..2] != GZIP_MAGIC {
        return Err(DevinWireError::Gzip("bad magic".into()));
    }
    if input[2] != GZIP_CM_DEFLATE {
        return Err(DevinWireError::Gzip(format!(
            "unsupported compression method {}",
            input[2]
        )));
    }
    let flags = input[3];
    if flags & 0xe0 != 0 {
        return Err(DevinWireError::Gzip(format!(
            "reserved FLG bits set: {flags:#04x}"
        )));
    }

    let mut pos = GZIP_FIXED_HEADER_LEN;
    if flags & GZIP_FEXTRA != 0 {
        let Some(extra_len_bytes) = input.get(pos..pos + 2) else {
            return Err(DevinWireError::Gzip("truncated FEXTRA length".into()));
        };
        let Ok(extra_len_bytes) = <[u8; 2]>::try_from(extra_len_bytes) else {
            return Err(DevinWireError::Gzip("truncated FEXTRA length".into()));
        };
        let extra_len = usize::from(u16::from_le_bytes(extra_len_bytes));
        pos = pos
            .checked_add(2)
            .and_then(|p| p.checked_add(extra_len))
            .ok_or_else(|| DevinWireError::Gzip("FEXTRA length overflow".into()))?;
    }
    for flag in [GZIP_FNAME, GZIP_FCOMMENT] {
        if flags & flag != 0 {
            let Some(tail) = input.get(pos..) else {
                return Err(DevinWireError::Gzip("truncated name/comment".into()));
            };
            let Some(nul) = tail.iter().position(|b| *b == 0) else {
                return Err(DevinWireError::Gzip("unterminated name/comment".into()));
            };
            pos += nul + 1;
        }
    }
    if flags & GZIP_FHCRC != 0 {
        pos = pos
            .checked_add(2)
            .ok_or_else(|| DevinWireError::Gzip("header overflow".into()))?;
    }
    if pos > input.len() {
        return Err(DevinWireError::Gzip("header exceeds input".into()));
    }

    miniz_oxide::inflate::decompress_to_vec_with_limit(
        input.get(pos..).unwrap_or(&[]),
        MAX_DECOMPRESSED_FRAME,
    )
    .map_err(|e| {
        if e.status == miniz_oxide::inflate::TINFLStatus::HasMoreOutput {
            DevinWireError::DecompressedTooLarge(MAX_DECOMPRESSED_FRAME)
        } else {
            DevinWireError::Gzip(e.to_string())
        }
    })
}

// ---------------------------------------------------------------------------
// Connect envelope framing
// ---------------------------------------------------------------------------

/// Wrap a protobuf body in a Connect data envelope (flag 0x00).
pub fn wrap_connect_envelope(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(CONNECT_FLAG_DATA);
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Heuristic: does `body` look like exactly one Connect envelope? Used to let
/// `wrap_request_body` pass through bodies a formatter already encoded.
pub fn looks_like_connect_envelope(body: &[u8]) -> bool {
    if body.len() < 5 || body[0] > (CONNECT_FLAG_COMPRESSED | CONNECT_FLAG_END_STREAM) {
        return false;
    }
    let Ok(len_bytes) = <[u8; 4]>::try_from(&body[1..5]) else {
        return false;
    };
    u32::from_be_bytes(len_bytes) as usize == body.len() - 5
}

/// One deframed Connect envelope. `payload` is already gunzipped when the
/// compressed bit was set; `flag` is preserved for end-stream checks.
#[derive(Debug, Clone)]
pub struct DevinFrameEvent {
    pub flag: u8,
    pub payload: Bytes,
}

impl DevinFrameEvent {
    /// Whether the end-stream bit is set (trailer frame).
    pub fn is_end_stream(&self) -> bool {
        self.flag & CONNECT_FLAG_END_STREAM != 0
    }
}

/// Incremental Connect envelope deframer for streamed responses.
#[derive(Debug, Default)]
pub struct DevinDeframer {
    buf: BytesMut,
}

impl DevinDeframer {
    pub fn new() -> Self {
        Self {
            buf: BytesMut::new(),
        }
    }

    /// Buffer another chunk of the response body.
    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
    }

    /// Whether the internal buffer holds no bytes at all.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Whether a partial frame is buffered at stream end (a protocol error
    /// for callers that saw the upstream close cleanly).
    pub fn has_partial(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Pop the next complete frame, if one is buffered.
    pub fn next_frame(&mut self) -> Result<Option<DevinFrameEvent>, DevinWireError> {
        if self.buf.len() < 5 {
            return Ok(None);
        }
        let flag = self.buf[0];
        if flag > (CONNECT_FLAG_COMPRESSED | CONNECT_FLAG_END_STREAM) {
            return Err(DevinWireError::BadFlag(flag));
        }
        let Ok(len_bytes) = <[u8; 4]>::try_from(&self.buf[1..5]) else {
            return Ok(None);
        };
        let len = u32::from_be_bytes(len_bytes) as usize;
        if len > MAX_COMPRESSED_FRAME {
            return Err(DevinWireError::FrameTooLarge(len));
        }
        if self.buf.len() < 5 + len {
            return Ok(None);
        }
        let _header = self.buf.split_to(5);
        let payload = self.buf.split_to(len).freeze();
        let payload = if flag & CONNECT_FLAG_COMPRESSED != 0 {
            Bytes::from(gunzip(&payload)?)
        } else {
            payload
        };
        Ok(Some(DevinFrameEvent { flag, payload }))
    }
}

// ---------------------------------------------------------------------------
// GetChatMessageRequest builder
// ---------------------------------------------------------------------------

/// ClientMetadata submessage (request field 1 / GetUserStatus field 1).
/// `device_seed` derives the field-31 fingerprint deterministically when the
/// account supplies one; empty means a fresh random fingerprint per request.
fn encode_client_metadata(token: &str, device_seed: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(768);
    protoc::append_string(&mut buf, 1, super::DEVIN_CLIENT_NAME);
    protoc::append_string(&mut buf, 2, super::DEVIN_CLIENT_VERSION);
    protoc::append_string(&mut buf, 3, token);
    protoc::append_string(&mut buf, 4, super::DEVIN_LOCALE);
    // Field 5 is runtime.GOOS upstream; map Rust's OS names onto Go's.
    let goos = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        std::env::consts::OS
    };
    protoc::append_string(&mut buf, 5, goos);
    protoc::append_string(&mut buf, 7, super::DEVIN_CLIENT_VERSION);
    protoc::append_string(&mut buf, 12, super::DEVIN_CLIENT_NAME);
    protoc::append_string(&mut buf, 31, &device_fingerprint(device_seed));
    buf
}

/// Session id from request extras (first non-empty, trimmed) — the same
/// keys upstream reads. `user` is deliberately not a fallback: upstream
/// ignores it and a user identifier is not a conversation id.
fn resolve_session_id(req: &OpenAIRequest) -> Option<String> {
    for key in [
        "session_id",
        "sessionId",
        "conversation_id",
        "previous_interaction_id",
    ] {
        if let Some(v) = req
            .extra
            .get(key)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            return Some(v.to_string());
        }
    }
    None
}

/// Upstream `normalizeDevinUUID`: valid UUIDs pass through, any other string
/// maps deterministically to an RFC-4122 UUID (sha256-based, v5-style layout —
/// the concrete hash is an opaque client-side session key, so a different
/// digest than upstream's SHA1 loses nothing), empty yields a random v4.
pub fn normalize_devin_uuid(raw: &str) -> String {
    let raw = raw.trim();
    if raw.is_empty() {
        return uuid::Uuid::new_v4().to_string();
    }
    if uuid::Uuid::parse_str(raw).is_ok() {
        return raw.to_string();
    }
    use sha2::Digest;
    let digest = sha2::Sha256::digest(raw.as_bytes());
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // version 5 layout: stable across runs for the same input string.
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

/// Per-session request ordinal (request field 15.2): first request in a
/// session emits 0 (omitted on the wire), subsequent ones 1, 2, 3… Process
/// scoped like devin-cli, capped like upstream's 5000-entry LRU.
const MAX_SESSION_TURN_COUNTERS: usize = 5000;

type SessionTurnCounters = (
    std::collections::HashMap<String, u64>,
    std::collections::VecDeque<String>,
);

static SESSION_TURN_COUNTERS: std::sync::LazyLock<std::sync::Mutex<SessionTurnCounters>> =
    std::sync::LazyLock::new(|| {
        std::sync::Mutex::new((
            std::collections::HashMap::new(),
            std::collections::VecDeque::new(),
        ))
    });

pub fn next_devin_session_turn_index(session_id: &str) -> u64 {
    let clean = session_id.trim();
    if clean.is_empty() {
        return 0;
    }
    let mut guard = SESSION_TURN_COUNTERS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (map, order) = &mut *guard;
    if let std::collections::hash_map::Entry::Vacant(e) = map.entry(clean.to_string()) {
        e.insert(0);
        order.push_back(clean.to_string());
        while map.len() > MAX_SESSION_TURN_COUNTERS
            && let Some(old) = order.pop_front()
        {
            map.remove(&old);
        }
        return 0;
    }
    let counter = map.get_mut(clean).map(|c| {
        *c += 1;
        *c
    });
    counter.unwrap_or(0)
}

/// Drop a session's turn counter (tests / explicit session resets).
#[cfg(test)]
pub fn reset_devin_session_turn_index(session_id: &str) {
    let mut guard = SESSION_TURN_COUNTERS
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let (map, order) = &mut *guard;
    map.remove(session_id.trim());
    order.retain(|s| s != session_id.trim());
}

/// Port of upstream `SanitizeDevinSystemPrompt`: strip Claude Code
/// attribution/identity lines before the system prompt hits the wire so
/// Devin's own server-side prompt (~390 tokens) isn't polluted by a second
/// CLI identity.
pub fn sanitize_devin_system_prompt(prompt: &str) -> String {
    const ATTRIBUTION_PREFIX: &str = "x-anthropic-billing-header:";
    const STRIPPED_NEEDLES: &[&str] = &[
        "You are Claude Code",
        "authorized security testing",
        "destructive techniques, DoS attacks",
        "Claude Code is available as a CLI",
        "Fast mode for Claude Code",
        "Codex refers to the open-source agentic coding interface",
        "- Don’t output ANSI escape codes directly — the CLI renderer applies them.",
    ];
    let normalized = prompt.replace("\r\n", "\n");
    let kept: Vec<&str> = normalized
        .split('\n')
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with(ATTRIBUTION_PREFIX)
                && !STRIPPED_NEEDLES.iter().any(|n| trimmed.contains(n))
        })
        .collect();
    kept.join("\n").trim().to_string()
}

// ---------------------------------------------------------------------------
// Tool-call ID namespace (upstream `cpa_tid_v1_` encoding)
// ---------------------------------------------------------------------------

const DEVIN_TOOL_ID_PREFIX: &str = "cpa_tid_v1_";

/// IDs the upstream/client round-trip accepts unmodified: `[A-Za-z0-9_-]+`.
fn is_portable_tool_call_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Encode a non-portable tool-call id into the `cpa_tid_v1_` namespace so it
/// cannot collide with native ids. Portable ids pass through.
pub fn encode_devin_tool_call_id(id: &str) -> String {
    use base64::Engine;
    if id.is_empty() || (is_portable_tool_call_id(id) && !id.starts_with(DEVIN_TOOL_ID_PREFIX)) {
        return id.to_string();
    }
    format!(
        "{DEVIN_TOOL_ID_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(id.as_bytes())
    )
}

/// Decode exactly one `cpa_tid_v1_` layer; malformed encodings and ordinary
/// ids pass through unchanged.
pub fn decode_devin_tool_call_id(id: &str) -> String {
    use base64::Engine;
    let Some(rest) = id.strip_prefix(DEVIN_TOOL_ID_PREFIX) else {
        return id.to_string();
    };
    let Ok(decoded) = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(rest) else {
        return id.to_string();
    };
    let Ok(original) = String::from_utf8(decoded) else {
        return id.to_string();
    };
    if original.is_empty() || encode_devin_tool_call_id(&original) != id {
        return id.to_string();
    }
    original
}

/// Decode `cpa_tid_v1_` ids across all prompt tool-call surfaces before the
/// protobuf build (upstream `RestoreDevinToolCallIDs`).
pub fn restore_devin_tool_call_ids(prompts: &mut [turn::DevinPrompt]) {
    for prompt in prompts.iter_mut() {
        for tc in &mut prompt.tool_calls {
            tc.id = decode_devin_tool_call_id(&tc.id);
        }
        prompt.tool_call_id = decode_devin_tool_call_id(&prompt.tool_call_id);
    }
}

/// Catalog models clamp `max_tokens` to their per-model completion limit
/// (upstream `LookupModelInfo` cap); unknown models keep the requested
/// value verbatim, falling back to the 128k default when unset/invalid.
fn resolve_max_tokens(req: &OpenAIRequest, chat_model_uid: &str) -> u64 {
    let requested = req
        .max_tokens
        .map(u64::from)
        .or_else(|| {
            req.extra
                .get("max_completion_tokens")
                .or_else(|| req.extra.get("max_output_tokens"))
                .or_else(|| req.extra.get("max_tokens"))
                .and_then(|v| v.as_u64())
        })
        .unwrap_or(0);
    match models::devin_max_completion_tokens_for(chat_model_uid) {
        Some(cap) if requested == 0 || requested > cap => cap,
        Some(_) => requested,
        None if requested == 0 => super::DEVIN_DEFAULT_MAX_TOKENS,
        None => requested,
    }
}

fn thinking_hints(req: &OpenAIRequest) -> (String, i64) {
    let level = req
        .extra
        .get("thinking_level")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let budget = req
        .extra
        .get("thinking_config")
        .and_then(|c| c.get("thinking_budget"))
        .or_else(|| req.extra.get("thinking_budget"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    (level, budget)
}

/// Build a full `GetChatMessageRequest` wrapped in its Connect envelope.
///
/// `api_key` is the Devin session token: it lands both in the Basic auth
/// header (see [`basic_auth`]) and inside ClientMetadata field 3.
/// `device_seed` feeds the deterministic fingerprint when the account
/// carries one; empty keeps the per-request random fingerprint.
pub fn build_devin_request(
    req: &OpenAIRequest,
    api_key: &str,
    upstream_model: &str,
    device_seed: &str,
) -> Result<Bytes, CoreError> {
    let mut mapped = turn::map_openai_turn(&req.messages)?;
    restore_devin_tool_call_ids(&mut mapped.prompts);
    let tools = req
        .tools
        .as_deref()
        .map(tools::collect_devin_tools)
        .unwrap_or_default();
    // Upstream always emits a session: provided ids are normalized, absent
    // ones become a fresh v4 — fields 15/16 are unconditional on the wire.
    let session_id = normalize_devin_uuid(resolve_session_id(req).as_deref().unwrap_or(""));
    let (level, budget) = thinking_hints(req);
    let chat_model_uid = models::resolve_devin_chat_model_uid(upstream_model, &level, budget);

    let mut body = Vec::with_capacity(1024 + mapped.prompts.len() * 64);

    // field 1: ClientMetadata
    protoc::append_message(&mut body, 1, &encode_client_metadata(api_key, device_seed));

    // field 2: system prompt (sanitized; only when non-empty)
    let system_prompt = sanitize_devin_system_prompt(&mapped.system_prompt);
    if !system_prompt.is_empty() {
        protoc::append_string(&mut body, 2, &system_prompt);
    }

    // field 3: repeated ChatMessage prompts
    for prompt in &mapped.prompts {
        protoc::append_message(&mut body, 3, &prompt.encode());
    }

    // field 7: fixed varint 5
    protoc::append_varint_field(&mut body, 7, 5);

    // field 8: GenerationConfig
    {
        let mut gen_cfg = Vec::with_capacity(48);
        protoc::append_varint_field(&mut gen_cfg, 1, 1);
        protoc::append_varint_field(&mut gen_cfg, 2, resolve_max_tokens(req, &chat_model_uid));
        protoc::append_varint_field(&mut gen_cfg, 3, 400);
        // Temperature is unconditional upstream (default 1.0); the 0.95
        // constant is written through the same f32→f64 widening upstream
        // performs, preserving exact wire bits. Clamp `<= 0` to the smallest
        // temperature the upstream sampler accepts: verbatim 0 or negative
        // values make the Codeium backend return `invalid_argument` on every
        // model (it divides by temperature inside the softmax), while
        // 0.0001 is indistinguishable from greedy decoding downstream.
        let temperature = req.temperature.unwrap_or(1.0);
        let temperature = if temperature <= 0.0 {
            DEVIN_MIN_TEMPERATURE
        } else {
            temperature
        };
        protoc::append_fixed64(&mut gen_cfg, 5, f64::from(temperature).to_bits());
        protoc::append_varint_field(&mut gen_cfg, 7, 40);
        protoc::append_fixed64(&mut gen_cfg, 8, f64::from(0.95_f32).to_bits());
        protoc::append_message(&mut body, 8, &gen_cfg);
    }

    // field 10: repeated tools
    for tool in &tools {
        protoc::append_message(&mut body, 10, &tool.encode());
    }

    // fields 15/16: session meta + cascade id (always emitted; cascade id
    // defaults to the session id like upstream's resolve step)
    {
        let turn_index = next_devin_session_turn_index(&session_id);
        let mut meta = Vec::with_capacity(64);
        protoc::append_string(&mut meta, 1, &session_id);
        if turn_index > 0 {
            protoc::append_varint_field(&mut meta, 2, turn_index);
        }
        protoc::append_varint_field(&mut meta, 3, 4);
        // Field 4 = 14 marks a user-turn boundary in devin-cli: first request
        // of the session, or a user turn that does not directly follow
        // another user turn.
        if mapped
            .prompts
            .last()
            .is_some_and(|p| p.source == turn::DEVIN_SOURCE_USER)
            && (turn_index == 0
                || mapped.prompts.len() < 2
                || mapped.prompts[mapped.prompts.len() - 2].source != turn::DEVIN_SOURCE_USER)
        {
            protoc::append_varint_field(&mut meta, 4, 14);
        }
        protoc::append_message(&mut body, 15, &meta);
        protoc::append_string(&mut body, 16, &session_id);
    }

    // field 20: fixed varint 1
    protoc::append_varint_field(&mut body, 20, 1);

    // field 21: chat_model_uid
    protoc::append_string(&mut body, 21, &chat_model_uid);

    Ok(Bytes::from(wrap_connect_envelope(&body)))
}

/// Raw `GetUserStatusRequest` protobuf (no Connect envelope) carrying only
/// ClientMetadata as field 1.
pub fn build_user_status_request(token: &str, device_seed: &str) -> Bytes {
    let mut body = Vec::with_capacity(768);
    protoc::append_message(&mut body, 1, &encode_client_metadata(token, device_seed));
    Bytes::from(body)
}

// Response-side parsing lives in `response.rs`; re-exported here so the
// pipeline keeps a single `wire::` entry point.
pub use super::response::{
    DevinFrame, DevinToolCallDelta, DevinUsage, parse_devin_frame, parse_devin_trailer_error,
};
