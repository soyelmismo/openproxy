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
        "image_url" => parse_image_part_inline(obj),
        "audio_url" => parse_audio_url_part_inline(obj),
        "input_audio" | "audio" => parse_input_audio_part_inline(obj),
        _ => None,
    }
}

pub fn parse_image_url_to_inline_data(part: &serde_json::Value) -> Option<GeminiInlineData> {
    parse_media_part_to_inline_data(part)
}
