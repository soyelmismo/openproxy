//! OpenAI message history → Devin `ChatMessage` prompt sequence.
//!
//! Devin replays the whole conversation on every `GetChatMessage` call, so the
//! mapper keeps upstream ordering and its source vocabulary: 1=user,
//! 2=assistant, 4=matched tool result. Orphaned tool results are re-emitted as
//! source-1 user prompts (`is_orphaned`), mirroring CLIProxyAPI's
//! `OriginalToolCallID`/`IsOrphanedTool` handling — the wire only carries the
//! result text, the flag exists for tests and downstream inspection.

use openproxy_types::{CoreError, OpenAIMessage};
use serde_json::Value;

use super::protoc;
use super::signature::parse_signature_value;

/// Prompt source emitted for user turns (and orphaned tool results).
pub const DEVIN_SOURCE_USER: u64 = 1;
/// Prompt source emitted for assistant turns.
pub const DEVIN_SOURCE_ASSISTANT: u64 = 2;
/// Prompt source emitted for tool results matched to a pending tool call.
pub const DEVIN_SOURCE_TOOL_RESULT: u64 = 4;

/// Part `type`s Devin cannot receive; the first one seen is recorded as
/// [`DevinPrompt::dropped_part`] and drives the user-turn validation.
const UNSENDABLE_PART_TYPES: &[&str] = &[
    "image",
    "input_image",
    "image_url",
    "audio",
    "input_audio",
    "audio_url",
    "video",
    "input_video",
    "video_url",
    "document",
    "file",
    "input_file",
];

const EMPTY_TOOL_RESULT_PLACEHOLDER: &str = "{}";

/// A tool call embedded in an assistant prompt (field 6).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DevinToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// An inline image embedded in a prompt (field 10).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DevinImage {
    /// Base64 payload, without any `data:` URI prefix.
    pub data_base64: String,
    pub mime: String,
}

/// One `ChatMessage` prompt (request field 3).
#[derive(Debug, Clone, Default)]
pub struct DevinPrompt {
    pub message_id: String,
    pub source: u64,
    pub content: String,
    pub tool_calls: Vec<DevinToolCall>,
    pub tool_call_id: String,
    pub images: Vec<DevinImage>,
    pub thinking: String,
    pub signature: Vec<u8>,
    pub signature_type: String,
    /// Tool result re-emitted as a user prompt because no pending tool call
    /// matched. Never set on genuine user turns.
    pub is_orphaned: bool,
    /// First unsendable media `type` seen in this turn's parts, if any.
    pub dropped_part: Option<String>,
}

impl DevinPrompt {
    /// Encode the `ChatMessage` submessage.
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        protoc::append_string(&mut buf, 1, &self.message_id);
        protoc::append_varint_field(&mut buf, 2, self.source);
        protoc::append_string(&mut buf, 3, &self.content);
        for tc in &self.tool_calls {
            let mut sub = Vec::new();
            protoc::append_string(&mut sub, 1, &tc.id);
            protoc::append_string(&mut sub, 2, &tc.name);
            protoc::append_string(&mut sub, 3, &tc.arguments);
            protoc::append_message(&mut buf, 6, &sub);
        }
        if !self.tool_call_id.is_empty() {
            protoc::append_string(&mut buf, 7, &self.tool_call_id);
        }
        for img in &self.images {
            let mut sub = Vec::new();
            protoc::append_string(&mut sub, 1, &img.data_base64);
            protoc::append_string(&mut sub, 2, &img.mime);
            protoc::append_message(&mut buf, 10, &sub);
        }
        if !self.thinking.is_empty() {
            protoc::append_string(&mut buf, 11, &self.thinking);
        }
        if !self.signature.is_empty() {
            protoc::append_bytes(&mut buf, 12, &self.signature);
        }
        if !self.signature_type.is_empty() {
            protoc::append_string(&mut buf, 18, &self.signature_type);
        }
        buf
    }

    /// A user-source prompt that carried only unsendable media: no text, no
    /// decodable images, and at least one dropped part recorded.
    fn is_media_only_user_turn(&self) -> bool {
        self.source == DEVIN_SOURCE_USER
            && self.content.trim().is_empty()
            && self.images.is_empty()
            && self.dropped_part.is_some()
    }
}

/// Result of mapping an OpenAI message list for one upstream request.
#[derive(Debug, Clone, Default)]
pub struct DevinTurn {
    pub system_prompt: String,
    pub prompts: Vec<DevinPrompt>,
}

/// Accumulator for one message's content parts.
#[derive(Default)]
struct PartAccum {
    texts: Vec<String>,
    thinkings: Vec<String>,
    images: Vec<DevinImage>,
    dropped: Option<String>,
    /// First non-empty signature found on a thinking/reasoning part —
    /// upstream attaches per-part signatures with a first-wins rule.
    signature: Vec<u8>,
    signature_type: String,
}

/// Interpret one string field that may hold a data URI or raw base64.
/// Remote URIs return `None` — Devin cannot fetch them; the caller's
/// `dropped` accounting records the part as unsendable.
fn image_from_str(raw: &str) -> Option<DevinImage> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains("://") {
        return None;
    }
    if let Some(rest) = trimmed.strip_prefix("data:")
        && let Some((mime, b64)) = rest.split_once(";base64,")
    {
        let mime = if mime.is_empty() { "image/png" } else { mime };
        return Some(DevinImage {
            data_base64: b64.to_string(),
            mime: mime.to_string(),
        });
    }
    Some(DevinImage {
        data_base64: trimmed.to_string(),
        mime: "image/png".to_string(),
    })
}

/// Pull an inline image out of a content part, across the several shapes
/// OpenAI/Anthropic-style clients use (`image_url`, `source`, `image`,
/// `data` + `mime_type`, `type: "input_image"`).
fn extract_image(part: &Value) -> Option<DevinImage> {
    let obj = part.as_object()?;

    // {"image_url": "<url|b64|data:...>"} or {"image_url": {"url": "..."}}
    if let Some(image_url) = obj.get("image_url") {
        let raw = image_url
            .as_str()
            .or_else(|| image_url.get("url").and_then(Value::as_str));
        if let Some(img) = raw.and_then(image_from_str) {
            return Some(img);
        }
    }

    // Anthropic-style {"source": {"type": "base64", "media_type", "data"}}
    // and {"image": <b64|{"data","mime_type"}>}.
    for key in ["source", "image"] {
        if let Some(media) = obj.get(key) {
            if let Some(img) = media.as_str().and_then(image_from_str) {
                return Some(img);
            }
            let data = media.get("data").and_then(Value::as_str);
            let mime = media
                .get("media_type")
                .or_else(|| media.get("mime_type"))
                .or_else(|| media.get("mime"))
                .and_then(Value::as_str)
                .unwrap_or("image/png");
            if let Some(data) = data
                && let Some(img) = image_from_str(data)
            {
                return Some(DevinImage {
                    data_base64: img.data_base64,
                    mime: mime.to_string(),
                });
            }
        }
    }

    // {"data": "<b64>", "mime_type": "image/..."} on the part itself.
    if let Some(data) = obj.get("data").and_then(Value::as_str) {
        let mime = obj
            .get("mime_type")
            .or_else(|| obj.get("media_type"))
            .or_else(|| obj.get("mime"))
            .and_then(Value::as_str)
            .unwrap_or("image/png");
        if let Some(img) = image_from_str(data) {
            return Some(DevinImage {
                data_base64: img.data_base64,
                mime: mime.to_string(),
            });
        }
    }

    None
}

fn mime_to_ext(mime: &str) -> &str {
    match mime.rsplit('/').next().unwrap_or("png") {
        "jpeg" | "jpg" => "jpg",
        "png" => "png",
        "webp" => "webp",
        "gif" => "gif",
        _ => "png",
    }
}

/// Prepend `[Image {i}: pasted_image_{i}.{ext}]` header lines when a turn has
/// inline images but no `[Image ` marker already — matches the marker protocol
/// Devin's client uses for pasted images.
fn inject_image_headers(text: String, images: &[DevinImage]) -> String {
    if images.is_empty() || text.contains("[Image ") {
        return text;
    }
    let headers = images
        .iter()
        .enumerate()
        .map(|(i, img)| {
            let n = i + 1;
            format!("[Image {n}: pasted_image_{n}.{}]", mime_to_ext(&img.mime))
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("{headers}\n\n{text}")
}

fn first_str_field<'a>(part: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| part.get(*k).and_then(Value::as_str))
}

fn extract_part(part: &Value, for_assistant: bool, acc: &mut PartAccum) {
    let Value::Object(_) = part else {
        if let Value::String(s) = part {
            acc.texts.push(s.clone());
        }
        return;
    };

    if let Some(img) = extract_image(part) {
        acc.images.push(img);
        return;
    }

    let part_type = part.get("type").and_then(Value::as_str).unwrap_or("");

    if for_assistant {
        // Assistant thinking/reasoning lands on `prompt.thinking`, never in
        // the visible content. Its signature fields ride along (upstream
        // model_output/thought steps carry them at step level).
        if part_type == "thinking" || part_type == "reasoning" {
            if let Some(t) = first_str_field(part, &["thinking", "reasoning", "text", "content"]) {
                acc.thinkings.push(t.to_string());
            }
            if acc.signature.is_empty()
                && let Some(raw) = first_str_field(part, &["signature", "thought_signature"])
            {
                let (bytes, kind) = parse_signature_value(raw);
                if !bytes.is_empty() {
                    acc.signature = bytes;
                    acc.signature_type = kind;
                }
            }
            return;
        }
        if let Some(t) = first_str_field(part, &["thinking", "reasoning"]) {
            acc.thinkings.push(t.to_string());
            return;
        }
        if let Some(t) = first_str_field(part, &["text", "content"]) {
            acc.texts.push(t.to_string());
            return;
        }
    } else {
        // User / tool-result turns flatten thinking-ish fields into text.
        if let Some(t) = first_str_field(part, &["text", "content", "thinking", "reasoning"]) {
            acc.texts.push(t.to_string());
            return;
        }
    }

    if UNSENDABLE_PART_TYPES.contains(&part_type) && acc.dropped.is_none() {
        acc.dropped = Some(part_type.to_string());
    }
}

fn extract_content(content: Option<&Value>, for_assistant: bool) -> PartAccum {
    let mut acc = PartAccum::default();
    match content {
        Some(Value::String(s)) => acc.texts.push(s.clone()),
        Some(Value::Array(parts)) => {
            for part in parts {
                extract_part(part, for_assistant, &mut acc);
            }
        }
        Some(value @ Value::Object(_)) => extract_part(value, for_assistant, &mut acc),
        Some(Value::Null) | None => {}
        Some(value) => acc.texts.push(value.to_string()),
    }
    acc
}

fn join_nonempty(fragments: &[String], sep: &str) -> String {
    fragments
        .iter()
        .filter(|f| !f.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join(sep)
}

/// Collect assistant-level thinking/signing extras (`reasoning`, `thinking`,
/// `signature`, `thought_signature`) from `msg.extra`.
fn assistant_extras(msg: &OpenAIMessage, acc: &mut PartAccum) -> (Vec<u8>, String) {
    for key in ["reasoning", "thinking"] {
        match msg.extra.get(key) {
            Some(Value::String(s)) => acc.thinkings.push(s.clone()),
            Some(Value::Array(items)) => {
                for item in items {
                    if let Some(t) = item
                        .as_str()
                        .or_else(|| first_str_field(item, &["thinking", "reasoning", "text"]))
                    {
                        acc.thinkings.push(t.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    let mut signature = Vec::new();
    let mut signature_type = String::new();
    for key in ["signature", "thought_signature"] {
        if let Some(raw) = msg.extra.get(key).and_then(Value::as_str) {
            let (bytes, kind) = parse_signature_value(raw);
            signature = bytes;
            signature_type = kind;
        }
    }
    (signature, signature_type)
}

/// Read `tool_call_id` from the canonical field or `id`/`call_id` extras.
fn tool_result_call_id(msg: &OpenAIMessage) -> String {
    msg.tool_call_id
        .as_deref()
        .or_else(|| {
            ["id", "call_id"]
                .iter()
                .find_map(|k| msg.extra.get(*k).and_then(Value::as_str))
        })
        .unwrap_or("")
        .trim()
        .to_string()
}

fn map_tool_call(tc: &Value) -> Option<DevinToolCall> {
    let obj = tc.as_object()?;
    let function = obj.get("function");
    let name = function
        .and_then(|f| f.get("name"))
        .or_else(|| obj.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let arguments = match function
        .and_then(|f| f.get("arguments"))
        .or_else(|| obj.get("arguments"))
    {
        Some(Value::String(s)) => s.clone(),
        Some(other) => serde_json::to_string(other).unwrap_or_default(),
        None => String::new(),
    };
    let id = obj
        .get("id")
        .or_else(|| obj.get("call_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(DevinToolCall {
        id,
        name,
        arguments,
    })
}

/// Remove emptied media-only user turns; error when a whole contiguous
/// non-assistant run would disappear (upstream's `CheckDevinUserTurns`).
fn apply_user_turn_policy(prompts: &mut Vec<DevinPrompt>) -> Result<(), CoreError> {
    let mut idx = 0;
    while idx < prompts.len() {
        if prompts[idx].source == DEVIN_SOURCE_ASSISTANT {
            idx += 1;
            continue;
        }
        let start = idx;
        while idx < prompts.len() && prompts[idx].source != DEVIN_SOURCE_ASSISTANT {
            idx += 1;
        }
        let run = &prompts[start..idx];
        let mut first_dropped: Option<String> = None;
        let mut has_content_bearing = false;
        for p in run {
            if p.is_media_only_user_turn() {
                if first_dropped.is_none() {
                    first_dropped.clone_from(&p.dropped_part);
                }
            } else {
                has_content_bearing = true;
            }
        }
        if !has_content_bearing && let Some(part_type) = first_dropped {
            return Err(CoreError::Validation(format!(
                "unsupported content part: {part_type}"
            )));
        }
    }
    prompts.retain(|p| !p.is_media_only_user_turn());
    Ok(())
}

/// Map OpenAI messages into the Devin system prompt + ordered prompt list.
///
/// Ordering, merging and tool-result matching mirror CLIProxyAPI's Devin
/// executor so the upstream sees the conversation shape its own client sends.
pub fn map_openai_turn(messages: &[OpenAIMessage]) -> Result<DevinTurn, CoreError> {
    let mut turn = DevinTurn::default();
    let mut pending_tool_call_ids: Vec<String> = Vec::new();

    for msg in messages {
        match msg.role.as_str() {
            "system" | "developer" => {
                // First non-empty system/developer text wins.
                if turn.system_prompt.is_empty() {
                    let acc = extract_content(msg.content.as_ref(), false);
                    let text = join_nonempty(&acc.texts, "\n");
                    let text = text.trim();
                    if !text.is_empty() {
                        turn.system_prompt = text.to_string();
                    }
                }
            }
            "user" => {
                let acc = extract_content(msg.content.as_ref(), false);
                let text = inject_image_headers(join_nonempty(&acc.texts, "\n"), &acc.images);
                turn.prompts.push(DevinPrompt {
                    message_id: uuid::Uuid::new_v4().to_string(),
                    source: DEVIN_SOURCE_USER,
                    content: text,
                    images: acc.images,
                    dropped_part: acc.dropped,
                    ..DevinPrompt::default()
                });
            }
            "assistant" => {
                let mut acc = extract_content(msg.content.as_ref(), true);
                let (extra_sig, extra_sig_type) = assistant_extras(msg, &mut acc);
                // Part-level signatures (thinking blocks) win; message-level
                // extras are the fallback, like upstream's supplement pass.
                let (signature, signature_type) = if acc.signature.is_empty() {
                    (extra_sig, extra_sig_type)
                } else {
                    (acc.signature, acc.signature_type)
                };
                let tool_calls = msg
                    .tool_calls
                    .as_deref()
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(map_tool_call)
                    .collect::<Vec<_>>();
                for tc in &tool_calls {
                    if !tc.id.is_empty() {
                        pending_tool_call_ids.push(tc.id.clone());
                    }
                }
                let text = inject_image_headers(join_nonempty(&acc.texts, "\n"), &acc.images);
                let thinking = join_nonempty(&acc.thinkings, "\n\n");

                // Consecutive assistant messages merge into one prompt.
                if let Some(last) = turn.prompts.last_mut()
                    && last.source == DEVIN_SOURCE_ASSISTANT
                {
                    if !text.is_empty() {
                        if last.content.is_empty() {
                            last.content = text;
                        } else {
                            last.content.push('\n');
                            last.content.push_str(&text);
                        }
                    }
                    last.tool_calls.extend(tool_calls);
                    last.images.extend(acc.images);
                    if !thinking.is_empty() {
                        if last.thinking.is_empty() {
                            last.thinking = thinking;
                        } else {
                            last.thinking.push_str("\n\n");
                            last.thinking.push_str(&thinking);
                        }
                    }
                    if !signature.is_empty() {
                        last.signature = signature;
                        last.signature_type = signature_type;
                    }
                    continue;
                }

                turn.prompts.push(DevinPrompt {
                    message_id: uuid::Uuid::new_v4().to_string(),
                    source: DEVIN_SOURCE_ASSISTANT,
                    content: text,
                    tool_calls,
                    images: acc.images,
                    thinking,
                    signature,
                    signature_type,
                    dropped_part: acc.dropped,
                    ..DevinPrompt::default()
                });
            }
            "tool" | "function" => {
                let provided_id = tool_result_call_id(msg);
                let matched = if provided_id.is_empty() {
                    if pending_tool_call_ids.is_empty() {
                        None
                    } else {
                        Some(pending_tool_call_ids.remove(0))
                    }
                } else {
                    pending_tool_call_ids
                        .iter()
                        .position(|id| id == &provided_id)
                        .map(|pos| pending_tool_call_ids.remove(pos))
                };

                let acc = extract_content(msg.content.as_ref(), false);
                let mut text = inject_image_headers(join_nonempty(&acc.texts, "\n"), &acc.images);
                if text.trim().is_empty() && acc.images.is_empty() {
                    text = EMPTY_TOOL_RESULT_PLACEHOLDER.to_string();
                }

                match matched {
                    Some(call_id) => turn.prompts.push(DevinPrompt {
                        message_id: uuid::Uuid::new_v4().to_string(),
                        source: DEVIN_SOURCE_TOOL_RESULT,
                        content: text,
                        tool_call_id: call_id,
                        images: acc.images,
                        dropped_part: acc.dropped,
                        ..DevinPrompt::default()
                    }),
                    None => turn.prompts.push(DevinPrompt {
                        message_id: uuid::Uuid::new_v4().to_string(),
                        source: DEVIN_SOURCE_USER,
                        content: text,
                        tool_call_id: provided_id,
                        images: acc.images,
                        is_orphaned: true,
                        dropped_part: acc.dropped,
                        ..DevinPrompt::default()
                    }),
                }
            }
            // Unknown roles degrade to user turns so content is never lost.
            _ => {
                let acc = extract_content(msg.content.as_ref(), false);
                let text = inject_image_headers(join_nonempty(&acc.texts, "\n"), &acc.images);
                turn.prompts.push(DevinPrompt {
                    message_id: uuid::Uuid::new_v4().to_string(),
                    source: DEVIN_SOURCE_USER,
                    content: text,
                    images: acc.images,
                    dropped_part: acc.dropped,
                    ..DevinPrompt::default()
                });
            }
        }
    }

    apply_user_turn_policy(&mut turn.prompts)?;
    Ok(turn)
}
