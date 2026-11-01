use serde::{Deserialize, Serialize};
use serde_json::Value;

impl_string_enum! {
    /// A modality an embedding request or model can carry.
    ///
    /// Four values, matching the input slots the embedding graph exposes
    /// (`input_ids`, `image_features`, `video_features`, `audio_features`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum EmbeddingModality {
        Text => "text",
        Image => "image",
        Audio => "audio",
        Video => "video",
        Document => "document",
    }
    core_error: "embedding_modality"
}

/// Which modalities a request or a model touches.
///
/// Five modalities fit in a byte, so the set is `Copy` and never allocates.
/// This is what lets a request be gated against what the target model actually
/// accepts, instead of failing deep inside an upstream call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct EmbeddingModalities(u8);

impl EmbeddingModalities {
    const TEXT: u8 = 1 << 0;
    const IMAGE: u8 = 1 << 1;
    const AUDIO: u8 = 1 << 2;
    const VIDEO: u8 = 1 << 3;
    const DOCUMENT: u8 = 1 << 4;

    const fn bit(modality: EmbeddingModality) -> u8 {
        match modality {
            EmbeddingModality::Text => Self::TEXT,
            EmbeddingModality::Image => Self::IMAGE,
            EmbeddingModality::Audio => Self::AUDIO,
            EmbeddingModality::Video => Self::VIDEO,
            EmbeddingModality::Document => Self::DOCUMENT,
        }
    }

    /// The empty set.
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// A set holding exactly one modality.
    #[must_use]
    pub const fn single(modality: EmbeddingModality) -> Self {
        Self(Self::bit(modality))
    }

    /// The set widened by one modality.
    #[must_use]
    pub const fn with(self, modality: EmbeddingModality) -> Self {
        Self(self.0 | Self::bit(modality))
    }

    /// Whether the set holds a modality.
    #[must_use]
    pub const fn contains(self, modality: EmbeddingModality) -> bool {
        self.0 & Self::bit(modality) != 0
    }

    /// Whether the set holds every modality of another set.
    #[must_use]
    pub const fn contains_all(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no modality was ever added.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Every modality in the set, in declaration order.
    pub fn iter(self) -> impl Iterator<Item = EmbeddingModality> {
        [
            EmbeddingModality::Text,
            EmbeddingModality::Image,
            EmbeddingModality::Audio,
            EmbeddingModality::Video,
            EmbeddingModality::Document,
        ]
        .into_iter()
        .filter(move |modality| self.contains(*modality))
    }
}

impl From<EmbeddingModality> for EmbeddingModalities {
    #[inline]
    fn from(modality: EmbeddingModality) -> Self {
        Self::single(modality)
    }
}

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

/// Input for embedding requests.
///
/// The first four variants are the OpenAI shapes and stay first so the
/// `untagged` deserializer keeps resolving them exactly as before. The two
/// multimodal variants are appended after them: an array of part objects fails
/// every earlier variant before reaching `Parts`, so no previously valid
/// request changes meaning.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingInput {
    Single(String),
    Array(Box<[String]>),
    Tokens(Box<[u32]>),
    TokenArrays(Box<[Box<[u32]>]>),
    /// One multimodal item: an ordered list of parts.
    Parts(Box<[EmbeddingPart]>),
    /// A batch of multimodal items, each of which may itself be a bare string.
    PartArrays(Box<[EmbeddingBatchItem]>),
}

/// One item of an embedding request, in the uniform shape consumers need.
///
/// Text and token variants normalize to their own arms so a caller never has to
/// special-case "is this the multimodal shape".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingItem<'a> {
    /// Plain text, already carrying its task prefix upstream of here.
    Text(&'a str),
    /// Pre-tokenized ids.
    Tokens(&'a [u32]),
    /// A multimodal item, parts in order.
    Parts(&'a [EmbeddingPart]),
}

impl EmbeddingInput {
    /// Every item of the request, in order.
    ///
    /// One small allocation per request, which is noise next to an inference.
    #[must_use]
    pub fn items(&self) -> Vec<EmbeddingItem<'_>> {
        match self {
            Self::Single(text) => vec![EmbeddingItem::Text(text)],
            Self::Array(texts) => texts.iter().map(|t| EmbeddingItem::Text(t)).collect(),
            Self::Tokens(ids) => vec![EmbeddingItem::Tokens(ids)],
            Self::TokenArrays(batches) => batches
                .iter()
                .map(|ids| EmbeddingItem::Tokens(ids))
                .collect(),
            Self::Parts(parts) => vec![EmbeddingItem::Parts(parts)],
            Self::PartArrays(batches) => batches
                .iter()
                .map(|item| EmbeddingItem::Parts(item.parts()))
                .collect(),
        }
    }

    /// How many embeddings this request asks for.
    #[must_use]
    pub fn item_count(&self) -> usize {
        match self {
            Self::Single(_) | Self::Tokens(_) | Self::Parts(_) => 1,
            Self::Array(texts) => texts.len(),
            Self::TokenArrays(batches) => batches.len(),
            Self::PartArrays(batches) => batches.len(),
        }
    }

    /// Every modality the request carries.
    ///
    /// Text and token items count as [`EmbeddingModality::Text`], so a plain
    /// string request reports text rather than nothing.
    #[must_use]
    pub fn modalities(&self) -> EmbeddingModalities {
        let mut set = EmbeddingModalities::empty();
        for item in self.items() {
            match item {
                EmbeddingItem::Text(_) | EmbeddingItem::Tokens(_) => {
                    set = set.with(EmbeddingModality::Text);
                }
                EmbeddingItem::Parts(parts) => {
                    for part in parts {
                        set = set.with(part.modality());
                    }
                }
            }
        }
        set
    }

    /// Whether the request carries no usable input.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        match self {
            Self::Single(s) => s.is_empty(),
            Self::Array(arr) => arr.is_empty() || arr.iter().all(|s| s.is_empty()),
            Self::Tokens(t) => t.is_empty(),
            Self::TokenArrays(arr) => arr.is_empty() || arr.iter().all(|t| t.is_empty()),
            Self::Parts(parts) => parts.is_empty() || parts.iter().all(EmbeddingPart::is_empty),
            Self::PartArrays(batches) => {
                batches.is_empty()
                    || batches
                        .iter()
                        .all(|item| item.parts().iter().all(EmbeddingPart::is_empty))
            }
        }
    }
}

impl_enum_from! {
    EmbeddingInput {
        Single(String),
        Single(&str => ToString::to_string),
        Array(Box<[String]>),
        Array(Vec<String> => Vec::into_boxed_slice),
        Tokens(Box<[u32]>),
        Tokens(Vec<u32> => Vec::into_boxed_slice),
        TokenArrays(Box<[Box<[u32]>]>),
        TokenArrays(Vec<Vec<u32>> => |v: Vec<Vec<u32>>| v.into_iter().map(Vec::into_boxed_slice).collect::<Box<[_]>>()),
        Parts(Box<[EmbeddingPart]>),
        Parts(Vec<EmbeddingPart> => Vec::into_boxed_slice),
        PartArrays(Box<[EmbeddingBatchItem]>),
        PartArrays(Vec<EmbeddingBatchItem> => Vec::into_boxed_slice),
    }
}

/// Request payload for embedding generation (`POST /v1/embeddings`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: EmbeddingInput,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding_format: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

/// Vector embedding representation: list of floats or base64 encoded string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EmbeddingVector {
    Floats(Box<[f32]>),
    Base64(String),
}

impl_enum_from! {
    EmbeddingVector {
        Floats(Box<[f32]>),
        Floats(Vec<f32> => Vec::into_boxed_slice),
        Base64(String),
        Base64(&str => ToString::to_string),
    }
}

/// An individual embedding object in the response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingObject {
    #[serde(default = "default_embedding_object")]
    pub object: String,
    pub embedding: EmbeddingVector,
    pub index: usize,
}

fn default_embedding_object() -> String {
    "embedding".to_string()
}

/// Usage statistics for an embedding request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct EmbeddingUsage {
    pub prompt_tokens: u32,
    pub total_tokens: u32,
}

/// Response payload for embedding generation (`POST /v1/embeddings`).
///
/// Multimodal responses are shaped exactly like text ones — a list of vectors —
/// so nothing here changes when a request carries media.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    #[serde(default = "default_embedding_response_object")]
    pub object: String,
    pub data: Box<[EmbeddingObject]>,
    pub model: String,
    pub usage: EmbeddingUsage,
}

fn default_embedding_response_object() -> String {
    "list".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::CoreError;

    #[test]
    fn test_embedding_input_single_serde() {
        let json_str = r#"{"model":"text-embedding-3-small","input":"hello world"}"#;
        let req: EmbeddingRequest = serde_json::from_str(json_str).expect("deserialize");
        assert_eq!(req.model, "text-embedding-3-small");
        assert_eq!(req.input, EmbeddingInput::Single("hello world".to_string()));
        assert!(!req.input.is_empty());
        assert_eq!(req.encoding_format, None);
        assert_eq!(req.dimensions, None);
        assert_eq!(req.user, None);

        let serialized = serde_json::to_string(&req).expect("serialize");
        assert!(serialized.contains(r#""input":"hello world""#));
    }

    #[test]
    fn test_embedding_input_array_serde() {
        let json_str = r#"{
            "model":"text-embedding-3-large",
            "input":["hello", "world"],
            "dimensions": 1536,
            "encoding_format": "float",
            "user": "user-123"
        }"#;
        let req: EmbeddingRequest = serde_json::from_str(json_str).expect("deserialize");
        assert_eq!(req.model, "text-embedding-3-large");
        assert_eq!(
            req.input,
            EmbeddingInput::Array(
                vec!["hello".to_string(), "world".to_string()].into_boxed_slice()
            )
        );
        assert!(!req.input.is_empty());
        assert_eq!(req.dimensions, Some(1536));
        assert_eq!(req.encoding_format.as_deref(), Some("float"));
        assert_eq!(req.user.as_deref(), Some("user-123"));
    }

    #[test]
    fn test_embedding_input_tokens_serde() {
        let json_str = r#"{"model":"text-embedding-3-small","input":[101, 2054, 2003, 102]}"#;
        let req: EmbeddingRequest = serde_json::from_str(json_str).expect("deserialize");
        assert_eq!(req.model, "text-embedding-3-small");
        assert_eq!(
            req.input,
            EmbeddingInput::Tokens(vec![101, 2054, 2003, 102].into_boxed_slice())
        );
        assert!(!req.input.is_empty());

        let serialized = serde_json::to_string(&req).expect("serialize");
        assert!(serialized.contains(r#""input":[101,2054,2003,102]"#));
    }

    #[test]
    fn test_embedding_input_token_arrays_serde() {
        let json_str = r#"{"model":"text-embedding-3-small","input":[[101, 2054], [2003, 102]]}"#;
        let req: EmbeddingRequest = serde_json::from_str(json_str).expect("deserialize");
        assert_eq!(req.model, "text-embedding-3-small");
        assert_eq!(
            req.input,
            EmbeddingInput::TokenArrays(
                vec![
                    vec![101, 2054].into_boxed_slice(),
                    vec![2003, 102].into_boxed_slice()
                ]
                .into_boxed_slice()
            )
        );
        assert!(!req.input.is_empty());
    }

    #[test]
    fn test_embedding_input_is_empty() {
        let empty_single = EmbeddingInput::Single(String::new());
        assert!(empty_single.is_empty());

        let empty_arr = EmbeddingInput::Array(vec![].into_boxed_slice());
        assert!(empty_arr.is_empty());

        let empty_arr_strings =
            EmbeddingInput::Array(vec![String::new(), String::new()].into_boxed_slice());
        assert!(empty_arr_strings.is_empty());

        let non_empty =
            EmbeddingInput::Array(vec![String::new(), "foo".to_string()].into_boxed_slice());
        assert!(!non_empty.is_empty());

        let empty_tokens = EmbeddingInput::Tokens(vec![].into_boxed_slice());
        assert!(empty_tokens.is_empty());

        let non_empty_tokens = EmbeddingInput::Tokens(vec![1, 2, 3].into_boxed_slice());
        assert!(!non_empty_tokens.is_empty());

        let empty_token_arrays = EmbeddingInput::TokenArrays(vec![].into_boxed_slice());
        assert!(empty_token_arrays.is_empty());

        let empty_token_arrays_inner = EmbeddingInput::TokenArrays(
            vec![vec![].into_boxed_slice(), vec![].into_boxed_slice()].into_boxed_slice(),
        );
        assert!(empty_token_arrays_inner.is_empty());

        let non_empty_token_arrays = EmbeddingInput::TokenArrays(
            vec![vec![1].into_boxed_slice(), vec![].into_boxed_slice()].into_boxed_slice(),
        );
        assert!(!non_empty_token_arrays.is_empty());

        let empty_parts = EmbeddingInput::Parts(vec![].into_boxed_slice());
        assert!(empty_parts.is_empty());

        let empty_text_parts = EmbeddingInput::Parts(
            vec![EmbeddingPart::Text {
                text: String::new(),
            }]
            .into_boxed_slice(),
        );
        assert!(empty_text_parts.is_empty());

        let media_parts = EmbeddingInput::Parts(
            vec![EmbeddingPart::Image {
                url: "data:image/png;base64,AAAA".to_string(),
            }]
            .into_boxed_slice(),
        );
        assert!(!media_parts.is_empty());

        let empty_part_arrays = EmbeddingInput::PartArrays(vec![].into_boxed_slice());
        assert!(empty_part_arrays.is_empty());
    }

    #[test]
    fn test_embedding_response_floats_serde() {
        let json_str = r#"{
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": [0.1, 0.2, 0.3],
                    "index": 0
                }
            ],
            "model": "text-embedding-3-small",
            "usage": {
                "prompt_tokens": 5,
                "total_tokens": 5
            }
        }"#;
        let res: EmbeddingResponse = serde_json::from_str(json_str).expect("deserialize");
        assert_eq!(res.object, "list");
        assert_eq!(res.model, "text-embedding-3-small");
        assert_eq!(res.data.len(), 1);
        assert_eq!(res.data[0].object, "embedding");
        assert_eq!(
            res.data[0].embedding,
            EmbeddingVector::Floats(vec![0.1, 0.2, 0.3].into_boxed_slice())
        );
        assert_eq!(res.data[0].index, 0);
        assert_eq!(res.usage.prompt_tokens, 5);
        assert_eq!(res.usage.total_tokens, 5);

        let serialized = serde_json::to_string(&res).expect("serialize");
        assert!(serialized.contains(r#""embedding":[0.1,0.2,0.3]"#));
    }

    #[test]
    fn test_embedding_response_base64_serde() {
        let json_str = r#"{
            "object": "list",
            "data": [
                {
                    "object": "embedding",
                    "embedding": "eJwrycxNLSoBAAhFAw0=",
                    "index": 0
                }
            ],
            "model": "text-embedding-3-small",
            "usage": {
                "prompt_tokens": 3,
                "total_tokens": 3
            }
        }"#;
        let res: EmbeddingResponse = serde_json::from_str(json_str).expect("deserialize");
        assert_eq!(res.object, "list");
        assert_eq!(res.model, "text-embedding-3-small");
        assert_eq!(res.data.len(), 1);
        assert_eq!(res.data[0].object, "embedding");
        assert_eq!(
            res.data[0].embedding,
            EmbeddingVector::Base64("eJwrycxNLSoBAAhFAw0=".to_string())
        );

        let serialized = serde_json::to_string(&res).expect("serialize");
        assert!(serialized.contains(r#""embedding":"eJwrycxNLSoBAAhFAw0=""#));
    }

    #[test]
    fn test_embedding_response_defaults() {
        let json_str = r#"{
            "data": [
                {
                    "embedding": [0.5, 0.6],
                    "index": 0
                }
            ],
            "model": "text-embedding-ada-002",
            "usage": {
                "prompt_tokens": 2,
                "total_tokens": 2
            }
        }"#;
        let res: EmbeddingResponse = serde_json::from_str(json_str).expect("deserialize");
        assert_eq!(res.object, "list");
        assert_eq!(res.data[0].object, "embedding");
        assert_eq!(
            res.data[0].embedding,
            EmbeddingVector::Floats(vec![0.5, 0.6].into_boxed_slice())
        );
    }

    #[test]
    fn test_embedding_input_from_conversions() {
        let from_str: EmbeddingInput = "hello".into();
        assert_eq!(from_str, EmbeddingInput::Single("hello".to_string()));

        let from_string: EmbeddingInput = String::from("world").into();
        assert_eq!(from_string, EmbeddingInput::Single("world".to_string()));

        let from_vec_str: EmbeddingInput = vec!["a".to_string(), "b".to_string()].into();
        assert_eq!(
            from_vec_str,
            EmbeddingInput::Array(vec!["a".to_string(), "b".to_string()].into_boxed_slice())
        );

        let from_tokens: EmbeddingInput = vec![1u32, 2, 3].into();
        assert_eq!(
            from_tokens,
            EmbeddingInput::Tokens(vec![1, 2, 3].into_boxed_slice())
        );

        let from_token_arrays: EmbeddingInput = vec![vec![1u32, 2], vec![3u32, 4]].into();
        assert_eq!(
            from_token_arrays,
            EmbeddingInput::TokenArrays(
                vec![vec![1, 2].into_boxed_slice(), vec![3, 4].into_boxed_slice()]
                    .into_boxed_slice()
            )
        );
    }

    // --- multimodal schema -------------------------------------------------

    #[test]
    fn test_part_accepts_every_dialect() {
        let cases: &[(&str, EmbeddingPart)] = &[
            // OpenAI chat / embeddings dialect, nested and flattened.
            (
                r#"{"type":"image_url","image_url":{"url":"https://x/a.png"}}"#,
                EmbeddingPart::Image {
                    url: "https://x/a.png".to_string(),
                },
            ),
            (
                r#"{"type":"image_url","image_url":"https://x/a.png"}"#,
                EmbeddingPart::Image {
                    url: "https://x/a.png".to_string(),
                },
            ),
            // OpenAI Responses dialect.
            (
                r#"{"type":"input_image","image_url":"https://x/a.png"}"#,
                EmbeddingPart::Image {
                    url: "https://x/a.png".to_string(),
                },
            ),
            // Anthropic dialect, url source.
            (
                r#"{"type":"image","source":{"type":"url","url":"https://x/a.png"}}"#,
                EmbeddingPart::Image {
                    url: "https://x/a.png".to_string(),
                },
            ),
            // Anthropic dialect, base64 source.
            (
                r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"QUJD"}}"#,
                EmbeddingPart::Image {
                    url: "data:image/png;base64,QUJD".to_string(),
                },
            ),
            // Gemini dialect, which carries no `type` at all.
            (
                r#"{"inline_data":{"mime_type":"image/png","data":"QUJD"}}"#,
                EmbeddingPart::Image {
                    url: "data:image/png;base64,QUJD".to_string(),
                },
            ),
            // Simple flat form.
            (
                r#"{"type":"image","url":"https://x/a.png"}"#,
                EmbeddingPart::Image {
                    url: "https://x/a.png".to_string(),
                },
            ),
            // Text dialects.
            (r#"{"type":"text","text":"hi"}"#, text_part("hi")),
            (r#"{"text":"hi"}"#, text_part("hi")),
            (r#"{"type":"text","content":"hi"}"#, text_part("hi")),
            ("\"hi\"", text_part("hi")),
            // Audio dialects.
            (
                r#"{"type":"input_audio","input_audio":{"data":"QUJD","format":"wav"}}"#,
                EmbeddingPart::Audio {
                    url: "data:audio/wav;base64,QUJD".to_string(),
                },
            ),
            (
                r#"{"type":"audio","url":"https://x/a.wav"}"#,
                EmbeddingPart::Audio {
                    url: "https://x/a.wav".to_string(),
                },
            ),
            (
                r#"{"inline_data":{"mime_type":"audio/wav","data":"QUJD"}}"#,
                EmbeddingPart::Audio {
                    url: "data:audio/wav;base64,QUJD".to_string(),
                },
            ),
            // Video dialects.
            (
                r#"{"type":"video_url","video_url":{"url":"https://x/a.mp4"}}"#,
                EmbeddingPart::Video {
                    url: "https://x/a.mp4".to_string(),
                },
            ),
            (
                r#"{"type":"input_video","video_url":"https://x/a.mp4"}"#,
                EmbeddingPart::Video {
                    url: "https://x/a.mp4".to_string(),
                },
            ),
            (
                r#"{"inline_data":{"mime_type":"video/mp4","data":"QUJD"}}"#,
                EmbeddingPart::Video {
                    url: "data:video/mp4;base64,QUJD".to_string(),
                },
            ),
        ];

        for (json, expected) in cases {
            let part: EmbeddingPart =
                serde_json::from_str(json).unwrap_or_else(|e| panic!("{json} should parse: {e}"));
            assert_eq!(&part, expected, "dialect {json}");
        }
    }

    #[test]
    fn test_part_rejects_unusable_shapes() {
        let cases = &[
            // A number is neither a string nor an object.
            "42",
            // Unknown type.
            r#"{"type":"document","source":{}}"#,
            // Media with no reference at all.
            r#"{"type":"image"}"#,
            // Empty reference.
            r#"{"type":"image","url":""}"#,
            // Base64 without a MIME type to build a data URL from.
            r#"{"type":"image","source":{"type":"base64","data":"QUJD"}}"#,
            // inline_data whose MIME names no embeddable modality.
            r#"{"inline_data":{"mime_type":"application/zip","data":"QUJD"}}"#,
            // An object with nothing recognizable in it.
            r#"{"foo":"bar"}"#,
        ];

        for json in cases {
            let result: std::result::Result<EmbeddingPart, _> = serde_json::from_str(json);
            assert!(result.is_err(), "{json} should not parse");
        }
    }

    #[test]
    fn a_pdf_part_parses_as_a_document() {
        // Gemini reads PDFs through the same `inline_data` slot as images, so
        // the modality has to come from the MIME type, not the field name.
        let part: EmbeddingPart = serde_json::from_str(
            r#"{"inline_data":{"mime_type":"application/pdf","data":"QUJD"}}"#,
        )
        .expect("pdf parses");
        assert_eq!(part.modality(), EmbeddingModality::Document);
        let EmbeddingPart::Document { url } = &part else {
            panic!("expected a document part, got {part:?}");
        };
        assert_eq!(url, "data:application/pdf;base64,QUJD");
    }

    #[test]
    fn an_unrelated_application_mime_is_still_rejected() {
        let result: std::result::Result<EmbeddingPart, _> = serde_json::from_str(
            r#"{"inline_data":{"mime_type":"application/zip","data":"QUJD"}}"#,
        );
        assert!(result.is_err(), "a zip is not an embeddable document");
    }

    #[test]
    fn test_part_round_trips_through_canonical_form() {
        let part = EmbeddingPart::Image {
            url: "data:image/png;base64,QUJD".to_string(),
        };
        let json = serde_json::to_string(&part).expect("serialize");
        assert_eq!(
            json,
            r#"{"type":"image","url":"data:image/png;base64,QUJD"}"#
        );

        let back: EmbeddingPart = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, part);
    }

    #[test]
    fn test_multimodal_input_variants_resolve_after_text_ones() {
        // A single multimodal item.
        let req: EmbeddingRequest = serde_json::from_str(
            r#"{"model":"m","input":[{"type":"text","text":"a cat"},{"type":"image_url","image_url":{"url":"https://x/a.png"}}]}"#,
        )
        .expect("deserialize");
        assert_eq!(req.input.item_count(), 1);
        assert_eq!(
            req.input.modalities(),
            EmbeddingModalities::single(EmbeddingModality::Text).with(EmbeddingModality::Image)
        );

        // A batch of multimodal items.
        let batch: EmbeddingRequest = serde_json::from_str(
            r#"{"model":"m","input":[[{"type":"text","text":"a"}],[{"type":"text","text":"b"},{"type":"audio","url":"https://x/a.wav"}]]}"#,
        )
        .expect("deserialize");
        assert_eq!(batch.input.item_count(), 2);
        assert_eq!(
            batch.input.modalities(),
            EmbeddingModalities::single(EmbeddingModality::Text).with(EmbeddingModality::Audio)
        );
    }

    #[test]
    fn test_text_shapes_keep_their_previous_meaning() {
        // The regression guard for the untagged ordering: every shape that was
        // valid before the multimodal variants existed must still resolve to
        // the same variant.
        let single: EmbeddingRequest =
            serde_json::from_str(r#"{"model":"m","input":"hi"}"#).expect("deserialize");
        assert_eq!(single.input, EmbeddingInput::Single("hi".to_string()));

        let array: EmbeddingRequest =
            serde_json::from_str(r#"{"model":"m","input":["a","b"]}"#).expect("deserialize");
        assert_eq!(
            array.input,
            EmbeddingInput::Array(vec!["a".to_string(), "b".to_string()].into_boxed_slice())
        );

        let tokens: EmbeddingRequest =
            serde_json::from_str(r#"{"model":"m","input":[1,2,3]}"#).expect("deserialize");
        assert_eq!(
            tokens.input,
            EmbeddingInput::Tokens(vec![1, 2, 3].into_boxed_slice())
        );

        let token_arrays: EmbeddingRequest =
            serde_json::from_str(r#"{"model":"m","input":[[1,2],[3,4]]}"#).expect("deserialize");
        assert_eq!(
            token_arrays.input,
            EmbeddingInput::TokenArrays(
                vec![vec![1, 2].into_boxed_slice(), vec![3, 4].into_boxed_slice()]
                    .into_boxed_slice()
            )
        );
    }

    #[test]
    fn test_items_normalizes_every_shape() {
        let single = EmbeddingInput::Single("hi".to_string());
        assert_eq!(single.items(), vec![EmbeddingItem::Text("hi")]);

        let array =
            EmbeddingInput::Array(vec!["a".to_string(), "b".to_string()].into_boxed_slice());
        assert_eq!(
            array.items(),
            vec![EmbeddingItem::Text("a"), EmbeddingItem::Text("b")]
        );

        let tokens = EmbeddingInput::Tokens(vec![1, 2].into_boxed_slice());
        assert_eq!(tokens.items(), vec![EmbeddingItem::Tokens(&[1, 2])]);

        let parts = EmbeddingInput::Parts(
            vec![
                EmbeddingPart::Text {
                    text: "a".to_string(),
                },
                EmbeddingPart::Image {
                    url: "https://x/a.png".to_string(),
                },
            ]
            .into_boxed_slice(),
        );
        assert_eq!(parts.items().len(), 1);
        match &parts.items()[0] {
            EmbeddingItem::Parts(slice) => assert_eq!(slice.len(), 2),
            other => panic!("expected Parts, got {other:?}"),
        }
    }

    #[test]
    fn test_modalities_of_plain_text_is_text() {
        // A plain string request reports text rather than an empty set, so a
        // capability gate never mistakes it for "carries nothing".
        let input = EmbeddingInput::Single("hi".to_string());
        assert_eq!(
            input.modalities(),
            EmbeddingModalities::single(EmbeddingModality::Text)
        );

        let tokens = EmbeddingInput::Tokens(vec![1].into_boxed_slice());
        assert_eq!(
            tokens.modalities(),
            EmbeddingModalities::single(EmbeddingModality::Text)
        );
    }

    #[test]
    fn test_modalities_set_operations() {
        let set = EmbeddingModalities::empty();
        assert!(set.is_empty());
        assert!(!set.contains(EmbeddingModality::Text));

        let set = set
            .with(EmbeddingModality::Text)
            .with(EmbeddingModality::Image);
        assert!(!set.is_empty());
        assert!(set.contains(EmbeddingModality::Text));
        assert!(set.contains(EmbeddingModality::Image));
        assert!(!set.contains(EmbeddingModality::Audio));

        assert!(set.contains_all(EmbeddingModalities::single(EmbeddingModality::Text)));
        assert!(!set.contains_all(EmbeddingModalities::single(EmbeddingModality::Audio)));

        let collected: Vec<_> = set.iter().collect();
        assert_eq!(
            collected,
            vec![EmbeddingModality::Text, EmbeddingModality::Image]
        );
    }

    #[test]
    fn test_modality_parse_and_display() {
        use std::str::FromStr;

        assert_eq!(EmbeddingModality::Text.as_str(), "text");
        assert_eq!(EmbeddingModality::Image.as_str(), "image");
        assert_eq!(EmbeddingModality::Audio.as_str(), "audio");
        assert_eq!(EmbeddingModality::Video.as_str(), "video");
        assert_eq!(EmbeddingModality::Document.as_str(), "document");

        assert_eq!(
            EmbeddingModality::from_str("image").expect("parse"),
            EmbeddingModality::Image
        );
        assert_eq!(
            EmbeddingModality::from_str("document").expect("parse"),
            EmbeddingModality::Document
        );
        // `core_error:` builds a CoreError, which is not PartialEq, so the
        // failure is matched rather than compared.
        match EmbeddingModality::from_str("spreadsheet") {
            Err(CoreError::Validation(message)) => {
                assert!(message.contains("embedding_modality"), "{message}");
            }
            other => panic!("expected a validation error, got {other:?}"),
        }
    }

    fn text_part(text: &str) -> EmbeddingPart {
        EmbeddingPart::Text {
            text: text.to_string(),
        }
    }
}
