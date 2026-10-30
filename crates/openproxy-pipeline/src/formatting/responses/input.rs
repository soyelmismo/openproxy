use openproxy_types::OpenAIMessage;
use openproxy_types::error::CoreError;
use serde_json::{Value, json};

use crate::formatting::extract_reasoning_content;

pub(crate) fn convert_image_url_part(item: &Value) -> Option<Value> {
    let (url, detail) = if let Some(url_obj) = item.get("image_url").and_then(Value::as_object) {
        let u = url_obj.get("url").and_then(Value::as_str)?;
        let d = url_obj
            .get("detail")
            .and_then(Value::as_str)
            .or_else(|| item.get("detail").and_then(Value::as_str));
        (u.to_string(), d)
    } else if let Some(u) = item
        .get("image_url")
        .and_then(Value::as_str)
        .or_else(|| item.get("url").and_then(Value::as_str))
    {
        (u.to_string(), item.get("detail").and_then(Value::as_str))
    } else if let Some(fid) = item.get("file_id").and_then(Value::as_str) {
        return Some(json!({ "type": "input_image", "file_id": fid }));
    } else {
        let data = item.get("image").and_then(Value::as_str)?;
        let mime = item
            .get("mime_type")
            .and_then(Value::as_str)
            .unwrap_or("image/jpeg");
        let u = media_data_uri(data, mime);
        (u, item.get("detail").and_then(Value::as_str))
    };
    let mut out = json!({ "type": "input_image", "image_url": url });
    if let Some(d) = detail {
        out["detail"] = Value::String(d.to_string());
    }
    Some(out)
}

pub(crate) fn convert_image_source_part(item: &Value) -> Option<Value> {
    if let Some(source) = item.get("source").and_then(Value::as_object)
        && let Some(data) = source
            .get("data")
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
    {
        let mime = source
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or("image/jpeg");
        let u = media_data_uri(data, mime);
        return Some(json!({ "type": "input_image", "image_url": u }));
    }
    convert_image_url_part(item)
}

/// Rebuilds the `data:<mime>;base64,<payload>` URI that the Responses file
/// parts want, from whichever shape the part arrived in.
fn media_data_uri(raw: &str, mime: &str) -> String {
    if raw.starts_with("data:") {
        raw.to_string()
    } else {
        format!("data:{mime};base64,{raw}")
    }
}

/// Maps a document part onto a Responses `input_file`.
///
/// The Responses API takes documents as `input_file` with either
/// `file_data` (a data URI or plain base64) or `file_id`. Anything else the
/// upstream rejects, so a part that cannot be expressed this way is dropped
/// rather than forwarded as an unknown type.
fn convert_file_part(item: &Value) -> Option<Value> {
    if let Some(file_id) = item
        .get("file_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .or_else(|| {
            item.get("file")
                .and_then(|f| f.get("file_id"))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        })
    {
        return Some(json!({ "type": "input_file", "file_id": file_id }));
    }

    let data = item
        .get("file_data")
        .and_then(Value::as_str)
        .or_else(|| {
            item.get("file")
                .and_then(|f| f.get("file_data"))
                .and_then(Value::as_str)
        })
        .or_else(|| {
            item.get("source")
                .and_then(|s| s.get("data"))
                .and_then(Value::as_str)
        })
        .filter(|s| !s.is_empty())?;
    let mime = item
        .get("media_type")
        .and_then(Value::as_str)
        .or_else(|| {
            item.get("source")
                .and_then(|s| s.get("media_type"))
                .and_then(Value::as_str)
        })
        .unwrap_or("application/pdf");
    let filename = item
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or("document.pdf");
    let mut out = json!({
        "type": "input_file",
        "filename": filename,
        "file_data": media_data_uri(data, mime),
    });
    if let Some(detail) = item.get("detail").and_then(Value::as_str) {
        out["detail"] = Value::String(detail.to_string());
    }
    Some(out)
}

/// Maps an audio part onto a Responses `input_audio`.
fn convert_audio_part(item: &Value) -> Option<Value> {
    if let Some(audio) = item.get("input_audio").and_then(Value::as_object) {
        let data = audio.get("data").and_then(Value::as_str)?;
        let format = audio.get("format").and_then(Value::as_str).unwrap_or("wav");
        return Some(json!({
            "type": "input_audio",
            "input_audio": { "data": data, "format": format },
        }));
    }
    let url = item
        .get("audio_url")
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.get("url").and_then(Value::as_str).map(str::to_string))
        })
        .or_else(|| {
            item.get("source").and_then(|s| {
                let data = s.get("data").and_then(Value::as_str)?;
                let mime = s
                    .get("media_type")
                    .and_then(Value::as_str)
                    .unwrap_or("audio/wav");
                Some(media_data_uri(data, mime))
            })
        })?;
    Some(json!({ "type": "input_audio", "audio_url": url }))
}

pub(crate) fn convert_content_item_to_part(
    item: &Value,
    text_type: &str,
) -> Result<Option<Value>, CoreError> {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("text");
    let part = match item_type {
        "text" | "input_text" | "output_text" => {
            let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({ "type": text_type, "text": text }))
        }
        "image_url" | "input_image" => convert_image_url_part(item),
        "image" => convert_image_source_part(item),
        "file" | "input_file" | "document" => convert_file_part(item),
        // The Responses API has no video content type: its accepted values are
        // input_text, input_image, input_audio, input_file, output_text,
        // refusal, computer_screenshot, summary_text and encrypted_content.
        // Forwarding a made-up `input_video` only moves the failure upstream
        // behind a 400 that reads like a proxy bug, so reject it here.
        "input_video" | "video_url" | "video" => {
            return Err(CoreError::Validation(
                "the Responses API does not accept video input; \
                 use a model exposed through /v1/chat/completions instead"
                    .to_string(),
            ));
        }
        "input_audio" | "audio" => convert_audio_part(item),
        _ => None,
    };
    Ok(part)
}

pub(crate) fn convert_msg_content_to_parts(
    content: Option<&Value>,
    text_type: &str,
) -> Result<Vec<Value>, CoreError> {
    let parts = match content {
        Some(Value::String(text)) if !text.is_empty() => {
            vec![json!({ "type": text_type, "text": text })]
        }
        Some(Value::Array(arr)) => {
            let mut parts = Vec::with_capacity(arr.len());
            for item in arr {
                if let Some(part) = convert_content_item_to_part(item, text_type)? {
                    // Only text parts can be empty; a media part carries its
                    // payload in another field, and dropping it here would
                    // silently turn a multimodal message into a text-only one.
                    let empty_text = part
                        .get("text")
                        .and_then(Value::as_str)
                        .is_some_and(str::is_empty);
                    if !empty_text {
                        parts.push(part);
                    }
                }
            }
            parts
        }
        Some(value) if !value.is_null() => {
            let s = if let Some(s) = value.as_str() {
                s.to_string()
            } else {
                value.to_string()
            };
            if !s.is_empty() {
                vec![json!({ "type": text_type, "text": s })]
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    };
    Ok(parts)
}

pub(crate) fn convert_msg_tool_calls(tool_calls: &[Value], input_items: &mut Vec<Value>) {
    for call in tool_calls {
        let call_id = call
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("call_xyz")
            .to_string();
        let func_name = call
            .get("function")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let func_args = call
            .get("function")
            .and_then(|v| v.get("arguments"))
            .and_then(|v| v.as_str())
            .unwrap_or("{}")
            .to_string();

        input_items.push(json!({
            "type": "function_call",
            "call_id": call_id,
            "name": func_name,
            "arguments": func_args
        }));
    }
}

pub(crate) fn sanitize_responses_reasoning_item(item: &mut Value) {
    let Some(map) = item.as_object_mut() else {
        return;
    };
    if map.get("type").and_then(Value::as_str) != Some("reasoning") {
        return;
    }
    let Some(content) = map.remove("content") else {
        return;
    };
    let summary_empty = map
        .get("summary")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty);
    if summary_empty {
        let mut text = String::new();
        if let Some(c_arr) = content.as_array() {
            for part in c_arr {
                if let Some(t) = part
                    .get("text")
                    .and_then(Value::as_str)
                    .or_else(|| part.as_str())
                {
                    text.push_str(t);
                }
            }
        } else if let Some(t) = content.as_str() {
            text.push_str(t);
        }
        if !text.is_empty() {
            map.insert(
                "summary".to_string(),
                json!([{ "type": "summary_text", "text": text }]),
            );
        }
    }
}

pub(crate) fn convert_single_message_to_responses_input(
    msg: &OpenAIMessage,
    input_items: &mut Vec<Value>,
) -> Result<(), CoreError> {
    if msg.role == "tool" {
        let call_id = msg.tool_call_id.as_deref().unwrap_or("call_xyz");
        let content_str = content_to_text(msg.content.as_ref());
        input_items.push(json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": content_str
        }));
        return Ok(());
    }

    if msg.role == "system" || msg.role == "developer" {
        let mut parts = convert_msg_content_to_parts(msg.content.as_ref(), "input_text")?;
        if parts.is_empty() {
            let text = content_to_text(msg.content.as_ref());
            if !text.is_empty() {
                parts.push(json!({ "type": "input_text", "text": text }));
            }
        }
        if !parts.is_empty() {
            input_items.push(json!({
                "role": "developer",
                "content": parts
            }));
        }
        return Ok(());
    }

    let has_tool_calls = msg.tool_calls.as_ref().is_some_and(|tc| !tc.is_empty());

    if msg.role == "assistant" {
        let has_encrypted = msg
            .extra
            .get("reasoning_encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty());
        let reasoning_text = extract_reasoning_content(msg);

        if has_encrypted || reasoning_text.is_some() || msg.extra.contains_key("reasoning_summary")
        {
            let mut r_item = serde_json::Map::new();
            r_item.insert("type".to_string(), Value::String("reasoning".to_string()));
            if let Some(id) = msg.extra.get("reasoning_id").and_then(Value::as_str) {
                r_item.insert("id".to_string(), Value::String(id.to_string()));
            }
            if let Some(enc) = msg
                .extra
                .get("reasoning_encrypted_content")
                .and_then(Value::as_str)
            {
                r_item.insert(
                    "encrypted_content".to_string(),
                    Value::String(enc.to_string()),
                );
            }
            if let Some(sum) = msg.extra.get("reasoning_summary") {
                r_item.insert("summary".to_string(), sum.clone());
            } else if let Some(text) = reasoning_text {
                r_item.insert(
                    "summary".to_string(),
                    json!([{
                        "type": "summary_text",
                        "text": text
                    }]),
                );
            } else if !r_item.contains_key("encrypted_content") {
                r_item.insert("summary".to_string(), Value::Array(Vec::new()));
            }
            input_items.push(Value::Object(r_item));
        }

        let parts = convert_msg_content_to_parts(msg.content.as_ref(), "output_text")?;
        if !parts.is_empty() {
            input_items.push(json!({
                "role": "assistant",
                "content": parts
            }));
        } else if !has_tool_calls {
            input_items.push(json!({
                "role": "assistant",
                "content": [json!({ "type": "output_text", "text": "" })]
            }));
        }
    } else {
        let mut parts = convert_msg_content_to_parts(msg.content.as_ref(), "input_text")?;
        if parts.is_empty() {
            parts.push(json!({ "type": "input_text", "text": "" }));
        }
        input_items.push(json!({
            "role": msg.role,
            "content": parts
        }));
    }

    if let Some(tool_calls) = &msg.tool_calls {
        convert_msg_tool_calls(tool_calls, input_items);
    }
    Ok(())
}

pub(crate) fn messages_to_responses_input<M: crate::context::AsOpenAIMessage>(
    messages: &[M],
) -> Result<Value, CoreError> {
    let mut input_items = Vec::new();
    for msg in messages {
        convert_single_message_to_responses_input(msg.as_message(), &mut input_items)?;
    }
    Ok(Value::Array(input_items))
}

pub(crate) fn content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}
