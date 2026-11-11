use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::modality::EmbeddingModality;

/// One content part of a multimodal embedding item.
///
/// This is the canonical, provider-agnostic form. Media references normalize to
/// a URL — either a `data:` URL or an `https://` one — so a client may speak its
/// own dialect while everything downstream sees a single shape.
///
/// `Deserialize` accepts every dialect the chat path already recognizes:
/// OpenAI `image_url` (nested or flattened), Anthropic `source` (url or
/// base64), Gemini `inline_data`, and a bare string as text. `Serialize` emits
/// the canonical form; translating to a specific upstream wire format is the
/// per-provider layer's job, not this one's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EmbeddingPart {
    /// Plain text. The only part that carries tokens.
    Text { text: String },
    /// Image reference: a `data:` URL or an `https://` URL.
    Image { url: String },
    /// Audio reference: a `data:` URL or an `https://` URL.
    Audio { url: String },
    /// Video reference: a `data:` URL or an `https://` URL.
    Video { url: String },
    /// Document reference (PDF and friends): a `data:` URL or an `https://` URL.
    Document { url: String },
}

impl EmbeddingPart {
    /// Normalizes any recognized content-part dialect into the canonical form.
    ///
    /// # Errors
    /// Returns a message when the value is neither a string nor an object, or
    /// when the modality or the media reference cannot be recovered from it.
    pub fn from_value(value: &Value) -> std::result::Result<Self, String> {
        match value {
            Value::String(text) => Ok(Self::Text { text: text.clone() }),
            Value::Object(map) => Self::from_object(map),
            other => Err(format!(
                "embedding part must be a string or an object, got {other}"
            )),
        }
    }

    fn from_object(map: &serde_json::Map<String, Value>) -> std::result::Result<Self, String> {
        // Text first: `text` and `content` are unambiguous across dialects, and
        // this is the common case.
        for key in ["text", "content"] {
            if let Some(text) = map.get(key).and_then(Value::as_str) {
                return Ok(Self::Text {
                    text: text.to_string(),
                });
            }
        }

        let modality = part_modality(map)?;
        let url = part_media_url(map, modality)?;
        Ok(modality.part_with_url(url))
    }

    /// The modality this part carries.
    #[must_use]
    pub const fn modality(&self) -> EmbeddingModality {
        match self {
            Self::Text { .. } => EmbeddingModality::Text,
            Self::Image { .. } => EmbeddingModality::Image,
            Self::Audio { .. } => EmbeddingModality::Audio,
            Self::Video { .. } => EmbeddingModality::Video,
            Self::Document { .. } => EmbeddingModality::Document,
        }
    }

    /// Whether the part carries no usable payload.
    ///
    /// Media parts are never empty: a reference is either present or the part
    /// failed to parse.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Text { text } => text.is_empty(),
            Self::Image { .. }
            | Self::Audio { .. }
            | Self::Video { .. }
            | Self::Document { .. } => false,
        }
    }
}

impl EmbeddingModality {
    /// Top-level MIME type of this modality, used to complete a bare format
    /// name like `wav` into `audio/wav`.
    const fn mime_prefix(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Document => "application",
        }
    }

    /// Builds the canonical part carrying `url` for this modality.
    fn part_with_url(self, url: String) -> EmbeddingPart {
        match self {
            Self::Text => EmbeddingPart::Text { text: url },
            Self::Image => EmbeddingPart::Image { url },
            Self::Audio => EmbeddingPart::Audio { url },
            Self::Video => EmbeddingPart::Video { url },
            Self::Document => EmbeddingPart::Document { url },
        }
    }
}

/// One item of a multimodal batch.
///
/// A bare string is a single text part; an array is the ordered list of parts.
/// Accepting both is what lets a batch mix plain items with multimodal ones,
/// which is the normal shape when a corpus holds both: pages of a scanned
/// document beside paragraphs of plain text, indexed in one call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddingBatchItem(Box<[EmbeddingPart]>);

impl EmbeddingBatchItem {
    /// The parts of this item, in order.
    #[must_use]
    pub fn parts(&self) -> &[EmbeddingPart] {
        &self.0
    }

    /// An item holding one text part.
    #[must_use]
    pub fn from_str_item(text: impl Into<String>) -> Self {
        Self(Box::new([EmbeddingPart::Text { text: text.into() }]))
    }

    /// An item holding the given parts, in order.
    #[must_use]
    pub fn from_parts(parts: Vec<EmbeddingPart>) -> Self {
        Self(parts.into_boxed_slice())
    }
}

impl<'de> Deserialize<'de> for EmbeddingBatchItem {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        match &value {
            // A bare string is one text part, never a one-element array of
            // characters: the string is the payload.
            Value::String(text) => Ok(Self(Box::new([EmbeddingPart::Text { text: text.clone() }]))),
            Value::Array(items) => {
                let mut parts = Vec::with_capacity(items.len());
                for item in items {
                    parts.push(EmbeddingPart::from_value(item).map_err(serde::de::Error::custom)?);
                }
                Ok(Self(parts.into_boxed_slice()))
            }
            other => Err(serde::de::Error::custom(format!(
                "embedding batch item must be a string or an array of parts, got {other}"
            ))),
        }
    }
}

impl Serialize for EmbeddingBatchItem {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EmbeddingPart {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        Self::from_value(&value).map_err(serde::de::Error::custom)
    }
}

/// Resolves the modality of a content part from its shape.
///
/// The `type` field wins when present; otherwise the part is identified by the
/// keys it carries, which is how Gemini's `inline_data` arrives — it has no
/// `type` at all.
fn part_modality(
    map: &serde_json::Map<String, Value>,
) -> std::result::Result<EmbeddingModality, String> {
    if let Some(declared) = map.get("type").and_then(Value::as_str) {
        return modality_from_type(declared)
            .ok_or_else(|| format!("unsupported embedding part type: {declared}"));
    }

    for (key, modality) in [
        ("image_url", EmbeddingModality::Image),
        ("audio_url", EmbeddingModality::Audio),
        ("video_url", EmbeddingModality::Video),
        ("input_audio", EmbeddingModality::Audio),
        ("input_video", EmbeddingModality::Video),
        ("input_file", EmbeddingModality::Document),
        ("input_document", EmbeddingModality::Document),
        ("file", EmbeddingModality::Document),
    ] {
        if map.contains_key(key) {
            return Ok(modality);
        }
    }

    // `inline_data` carries only a MIME type, which names the modality.
    for key in ["inline_data", "inlinedata", "inlineData"] {
        if let Some(inner) = map.get(key).and_then(Value::as_object)
            && let Some(mime) = inner.get("mime_type").and_then(Value::as_str)
        {
            return modality_from_mime(mime).ok_or_else(|| {
                format!("inline_data MIME type is not an embeddable modality: {mime}")
            });
        }
    }

    Err("embedding part declares no recognizable modality".to_string())
}

/// Maps a `type` value to a modality, accepting the whole vocabulary the chat
/// path already tolerates so a client never has to rewrite its payload.
fn modality_from_type(declared: &str) -> Option<EmbeddingModality> {
    match declared {
        "text" => Some(EmbeddingModality::Text),
        "image_url" | "input_image" | "image" => Some(EmbeddingModality::Image),
        "input_audio" | "audio_url" | "audio" => Some(EmbeddingModality::Audio),
        "input_video" | "video_url" | "video" => Some(EmbeddingModality::Video),
        "input_file" | "input_document" | "file" | "document" => Some(EmbeddingModality::Document),
        _ => None,
    }
}

/// Maps a MIME type to a modality by its top-level type.
///
/// `application` is deliberately not accepted wholesale: the embedding models
/// read PDFs as documents and nothing else under that top-level type, so a zip
/// or a firmware blob is rejected rather than guessed at.
fn modality_from_mime(mime: &str) -> Option<EmbeddingModality> {
    if mime == "application/pdf" {
        return Some(EmbeddingModality::Document);
    }
    match mime.split('/').next()? {
        "image" => Some(EmbeddingModality::Image),
        "audio" => Some(EmbeddingModality::Audio),
        "video" => Some(EmbeddingModality::Video),
        _ => None,
    }
}

/// Extracts the media reference of a part, synthesizing a `data:` URL when the
/// dialect sends raw base64 plus a MIME type instead of a URL.
fn part_media_url(
    map: &serde_json::Map<String, Value>,
    modality: EmbeddingModality,
) -> std::result::Result<String, String> {
    // A plain top-level `url` covers the flattened and the simple dialects.
    if let Some(url) = map.get("url").and_then(Value::as_str) {
        return non_empty_url(url);
    }

    // The nested dialects: OpenAI's `image_url` object, Anthropic's `source`,
    // Gemini's `inline_data`. Each is checked in turn.
    for key in [
        "image_url",
        "audio_url",
        "video_url",
        "input_audio",
        "input_video",
        "input_file",
        "input_document",
        "file",
        "source",
        "inline_data",
        "inlinedata",
        "inlineData",
    ] {
        if let Some(inner) = map.get(key) {
            // Flattened dialect: the key holds the URL itself, not an object.
            if let Some(url) = inner.as_str() {
                return non_empty_url(url);
            }
            if let Some(url) = inner.get("url").and_then(Value::as_str) {
                return non_empty_url(url);
            }
            if let Some(data) = inner.get("data").and_then(Value::as_str) {
                let mime = inner
                    .get("media_type")
                    .or_else(|| inner.get("mime_type"))
                    .or_else(|| inner.get("format"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        "embedding part carries base64 data but no MIME type".to_string()
                    })?;
                return non_empty_url(&synthesize_data_url(modality, mime, data));
            }
        }
    }

    // A flat Anthropic-style part: `data` and `media_type` beside the type.
    if let Some(data) = map.get("data").and_then(Value::as_str) {
        let mime = map
            .get("media_type")
            .or_else(|| map.get("mime_type"))
            .and_then(Value::as_str)
            .ok_or_else(|| "embedding part carries base64 data but no MIME type".to_string())?;
        return non_empty_url(&synthesize_data_url(modality, mime, data));
    }

    Err(format!(
        "embedding part of modality {} carries no media reference",
        modality.as_str()
    ))
}

/// Rejects an empty reference, which would otherwise sail through and fail much
/// later inside a fetch or a decoder.
fn non_empty_url(url: &str) -> std::result::Result<String, String> {
    if url.trim().is_empty() {
        return Err("embedding part media reference is empty".to_string());
    }
    Ok(url.to_string())
}

/// Builds a `data:` URL from raw base64 and a MIME type, so every dialect
/// converges on the same canonical reference.
///
/// Anthropic and Gemini send a full MIME type (`image/png`); OpenAI's
/// `input_audio` sends only a format name (`wav`). A value without a `/` is
/// completed with the modality's own type, so both land on `audio/wav`.
fn synthesize_data_url(modality: EmbeddingModality, mime: &str, data: &str) -> String {
    let mime = if mime.contains('/') {
        mime.to_string()
    } else {
        format!("{}/{}", modality.mime_prefix(), mime)
    };
    format!("data:{mime};base64,{data}")
}
