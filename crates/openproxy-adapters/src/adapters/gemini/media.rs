use super::types::GeminiInlineData;

fn map_audio_extension(ext: &str) -> Option<&'static str> {
    match ext {
        "wav" | "x-wav" | "wave" => Some("audio/wav"),
        "mp3" | "mpeg" => Some("audio/mp3"),
        "m4a" | "aac" => Some("audio/m4a"),
        "ogg" | "opus" => Some("audio/ogg"),
        "flac" | "x-flac" => Some("audio/flac"),
        "aiff" | "x-aiff" => Some("audio/aiff"),
        "pcm" => Some("audio/pcm"),
        _ => None,
    }
}

pub fn normalize_audio_mime(format: &str) -> String {
    let ext = format.trim_start_matches('.');
    if let Some(mime) = map_audio_extension(ext) {
        return mime.to_string();
    }
    if ext.bytes().any(|b| b.is_ascii_uppercase()) {
        let lower = ext.to_ascii_lowercase();
        if let Some(mime) = map_audio_extension(&lower) {
            return mime.to_string();
        }
        if lower.starts_with("audio/") {
            return lower;
        }
    } else if ext.starts_with("audio/") {
        return ext.to_string();
    }
    "audio/mp3".to_string()
}

fn parse_data_uri(url: &str) -> Option<GeminiInlineData> {
    let stripped = url.strip_prefix("data:")?;
    let (mime_type, rest) = stripped.split_once(';')?;
    let (_, data) = rest.split_once(',')?;
    Some(GeminiInlineData {
        mime_type: mime_type.to_string(),
        data: data.to_string(),
    })
}

fn parse_image_part_inline(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<GeminiInlineData> {
    let url = obj.get("image_url")?.as_object()?.get("url")?.as_str()?;
    parse_data_uri(url)
}

fn parse_audio_url_part_inline(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<GeminiInlineData> {
    let audio_obj = obj.get("audio_url")?.as_object()?;
    let url = audio_obj.get("url")?.as_str()?;
    if let Some(inline) = parse_data_uri(url) {
        return Some(inline);
    }
    let mime = audio_obj
        .get("mime_type")
        .or_else(|| audio_obj.get("mimeType"))
        .or_else(|| audio_obj.get("format"))
        .and_then(|v| v.as_str())
        .map_or_else(|| "audio/mp3".to_string(), normalize_audio_mime);
    Some(GeminiInlineData {
        mime_type: mime,
        data: url.to_string(),
    })
}

fn parse_input_audio_part_inline(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<GeminiInlineData> {
    let audio_obj = obj
        .get("input_audio")
        .or_else(|| obj.get("audio"))?
        .as_object()?;
    let data_raw = audio_obj.get("data")?.as_str()?;
    let format_raw = audio_obj
        .get("format")
        .or_else(|| audio_obj.get("mime_type"))
        .or_else(|| audio_obj.get("mimeType"))
        .and_then(|v| v.as_str())
        .unwrap_or("mp3");
    let mime_type = normalize_audio_mime(format_raw);
    let data = if let Some(stripped) = data_raw.strip_prefix("data:") {
        let (_, rest) = stripped.split_once(',')?;
        rest.to_string()
    } else {
        data_raw.to_string()
    };
    Some(GeminiInlineData { mime_type, data })
}

pub fn parse_media_part_to_inline_data(part: &serde_json::Value) -> Option<GeminiInlineData> {
    let obj = part.as_object()?;
    let typ = obj.get("type").and_then(|v| v.as_str())?;
    match typ {
        "image_url" | "input_image" => parse_image_part_inline(obj),
        "audio_url" => parse_audio_url_part_inline(obj),
        "input_audio" | "audio" => parse_input_audio_part_inline(obj),
        // vLLM's video extension. Gemini takes video as inline_data with a
        // video/* mime type, so the same path as an image applies.
        "video_url" | "input_video" => parse_video_part_inline(obj),
        // Documents arrive as Chat Completions `file` after canonicalisation.
        // Gemini reads PDFs as inline_data with application/pdf.
        "file" | "input_file" | "document" => parse_file_part_inline(obj),
        _ => None,
    }
}

fn parse_video_part_inline(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<GeminiInlineData> {
    let url = obj
        .get("video_url")
        .and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.get("url").and_then(|u| u.as_str()).map(str::to_string))
        })
        .or_else(|| {
            obj.get("source")
                .and_then(|s| s.get("url"))
                .and_then(|u| u.as_str())
                .map(str::to_string)
        })?;
    if let Some(inline) = parse_data_uri(&url) {
        return Some(inline);
    }
    Some(GeminiInlineData {
        mime_type: "video/mp4".to_string(),
        data: url,
    })
}

fn parse_file_part_inline(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<GeminiInlineData> {
    // Canonical `{type: "file", file: {filename, file_data}}`.
    if let Some(file) = obj.get("file").and_then(|f| f.as_object()) {
        if let Some(data) = file.get("file_data").and_then(|v| v.as_str()) {
            if let Some(inline) = parse_data_uri(data) {
                return Some(inline);
            }
            return Some(GeminiInlineData {
                mime_type: "application/pdf".to_string(),
                data: data.to_string(),
            });
        }
    }
    // Flat `{type: "input_file", file_data: ...}`.
    if let Some(data) = obj.get("file_data").and_then(|v| v.as_str()) {
        if let Some(inline) = parse_data_uri(data) {
            return Some(inline);
        }
        return Some(GeminiInlineData {
            mime_type: "application/pdf".to_string(),
            data: data.to_string(),
        });
    }
    // Anthropic `document` block with a base64 source.
    let source = obj.get("source")?.as_object()?;
    let data = source.get("data").and_then(|v| v.as_str())?;
    let mime = source
        .get("media_type")
        .and_then(|v| v.as_str())
        .unwrap_or("application/pdf");
    Some(GeminiInlineData {
        mime_type: mime.to_string(),
        data: data.to_string(),
    })
}

pub fn parse_image_url_to_inline_data(part: &serde_json::Value) -> Option<GeminiInlineData> {
    parse_media_part_to_inline_data(part)
}
