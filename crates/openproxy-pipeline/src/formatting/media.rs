//! Canonical media parts.
//!
//! Every inbound endpoint (`/v1/chat/completions`, `/v1/messages`,
//! `/v1/responses`) parses its own dialect, but they all funnel into the same
//! `OpenAIRequest` before the pipeline runs. Three dialects name the same four
//! media kinds differently, and a part that keeps the dialect it arrived in is
//! silently dropped by any formatter that only recognises its own vocabulary:
//! an `input_image` from a Responses client reaches a Gemini target as an
//! unknown type, `map_single_content_part` falls back to
//! `extract_content_part_text`, and the model answers about an image it never
//! received.
//!
//! This module rewrites every media part to the canonical OpenAI Chat
//! Completions spelling — `text`, `image_url`, `input_audio`, `file`,
//! `video_url` — so that each target formatter only has to understand one
//! vocabulary. Video stays `video_url` because that is the type vLLM-backed
//! providers (NVIDIA NIM, Hetzner, MiniMax) actually accept; `input_video` is
//! not in the Responses API nor in any Chat Completions schema and is rejected
//! upstream with a 400.

use serde_json::{Map, Value, json};

/// Rewrites a single content part to canonical OpenAI Chat Completions shape.
///
/// Returns `None` for parts that carry no meaning downstream (an empty text
/// part, an unknown type) so callers can drop them instead of forwarding a
/// type the upstream will reject.
pub fn canonical_media_part(part: &Value) -> Option<Value> {
    let obj = part.as_object()?;
    let typ = obj.get("type").and_then(Value::as_str).unwrap_or("text");

    match typ {
        "text" | "input_text" | "output_text" => {
            let text = obj.get("text").and_then(Value::as_str).unwrap_or("");
            if text.is_empty() {
                return None;
            }
            Some(json!({ "type": "text", "text": text }))
        }

        // Image: `input_image` is the Responses spelling; `image` is the
        // Anthropic block name after a reverse translation.
        "image_url" | "input_image" | "image" => canonical_image(obj),

        // Audio: Chat Completions uses `input_audio` with `{data, format}`.
        "input_audio" | "audio" => canonical_audio(obj),

        // Video: vLLM's extension, accepted by NIM, Hetzner and MiniMax.
        "video_url" | "input_video" | "video" => canonical_video(obj),

        // Documents: Chat Completions `file` with `{filename, file_data}`.
        "file" | "input_file" | "document" => canonical_file(obj),

        _ => None,
    }
}

fn data_uri(raw: &str, mime: &str) -> String {
    if raw.starts_with("data:") {
        raw.to_string()
    } else {
        format!("data:{mime};base64,{raw}")
    }
}

/// Pulls the payload out of an Anthropic-style `source` object.
fn source_payload(obj: &Map<String, Value>, default_mime: &str) -> Option<(String, String)> {
    let source = obj.get("source")?.as_object()?;
    if let Some(data) = source.get("data").and_then(Value::as_str) {
        let mime = source
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or(default_mime);
        return Some((data_uri(data, mime), mime.to_string()));
    }
    if let Some(url) = source.get("url").and_then(Value::as_str) {
        return Some((url.to_string(), default_mime.to_string()));
    }
    None
}

fn canonical_image(obj: &Map<String, Value>) -> Option<Value> {
    // `image_url` accepts either a bare string or `{url, detail}`; both survive
    // as an object so `detail` is not lost.
    if let Some(image_url) = obj.get("image_url") {
        let detail = image_url
            .get("detail")
            .or_else(|| obj.get("detail"))
            .cloned();
        let url = image_url
            .as_str()
            .map(str::to_string)
            .or_else(|| {
                image_url
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .or_else(|| source_payload(obj, "image/jpeg").map(|(u, _)| u))?;
        let mut inner = Map::new();
        inner.insert("url".into(), Value::String(url));
        if let Some(d) = detail {
            inner.insert("detail".into(), d);
        }
        return Some(json!({ "type": "image_url", "image_url": Value::Object(inner) }));
    }
    // Anthropic `image` block: `{source: {type: base64|url, media_type, data}}`.
    let (url, _) = source_payload(obj, "image/jpeg")?;
    Some(json!({ "type": "image_url", "image_url": { "url": url } }))
}

fn canonical_audio(obj: &Map<String, Value>) -> Option<Value> {
    if let Some(audio) = obj.get("input_audio").and_then(Value::as_object) {
        let data = audio.get("data").and_then(Value::as_str)?;
        let format = audio
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("wav");
        return Some(json!({
            "type": "input_audio",
            "input_audio": { "data": data, "format": format },
        }));
    }
    let (url, _) = source_payload(obj, "audio/wav")?;
    Some(json!({ "type": "audio_url", "audio_url": { "url": url } }))
}

fn canonical_video(obj: &Map<String, Value>) -> Option<Value> {
    let url = obj
        .get("video_url")
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.get("url").and_then(Value::as_str).map(str::to_string))
        })
        .or_else(|| source_payload(obj, "video/mp4").map(|(u, _)| u))?;
    Some(json!({ "type": "video_url", "video_url": { "url": url } }))
}

fn canonical_file(obj: &Map<String, Value>) -> Option<Value> {
    // Already canonical Chat Completions: `{file: {filename, file_data}}`.
    if let Some(file) = obj.get("file").and_then(Value::as_object) {
        let mut inner = Map::new();
        for key in ["filename", "file_data", "file_id"] {
            if let Some(v) = file.get(key) {
                inner.insert(key.into(), v.clone());
            }
        }
        if inner.is_empty() {
            return None;
        }
        return Some(json!({ "type": "file", "file": Value::Object(inner) }));
    }

    let filename = obj
        .get("filename")
        .and_then(Value::as_str)
        .unwrap_or("document.pdf");
    if let Some(file_id) = obj.get("file_id").and_then(Value::as_str) {
        return Some(json!({
            "type": "file",
            "file": { "filename": filename, "file_id": file_id },
        }));
    }
    if let Some(data) = obj.get("file_data").and_then(Value::as_str) {
        let mime = obj
            .get("media_type")
            .and_then(Value::as_str)
            .unwrap_or("application/pdf");
        return Some(json!({
            "type": "file",
            "file": { "filename": filename, "file_data": data_uri(data, mime) },
        }));
    }
    // Anthropic `document` block.
    let (data, mime) = source_payload(obj, "application/pdf")?;
    let name = if obj.get("filename").is_none() && mime.contains("pdf") {
        "document.pdf".to_string()
    } else {
        filename.to_string()
    };
    Some(json!({
        "type": "file",
        "file": { "filename": name, "file_data": data },
    }))
}

/// Canonicalises every media part in a content array, preserving order and
/// dropping parts that carry nothing downstream.
pub fn canonical_media_parts(parts: &[Value]) -> Vec<Value> {
    parts.iter().filter_map(canonical_media_part).collect()
}

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
