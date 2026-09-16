//! Heuristics for inferring model capabilities from the `model_id`.

use serde::{Deserialize, Serialize};

/// Capability flags surfaced to clients via `GET /v1/models`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModelCapabilities {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vision: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calling: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attachment: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structured_output: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<bool>,
}

impl ModelCapabilities {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        [
            self.vision,
            self.tool_calling,
            self.reasoning,
            self.thinking,
            self.attachment,
            self.structured_output,
            self.temperature,
        ]
        .iter()
        .all(Option::is_none)
    }

    pub fn to_json(&self) -> Option<String> {
        if self.is_empty() {
            None
        } else {
            serde_json::to_string(self).ok()
        }
    }

    pub fn from_json(s: Option<&str>) -> Self {
        s.and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(Self::empty)
    }
}

use std::borrow::Cow;

#[inline]
fn to_lower_cow(s: &str) -> Cow<'_, str> {
    if s.bytes().any(|b| b.is_ascii_uppercase()) {
        Cow::Owned(s.to_lowercase())
    } else {
        Cow::Borrowed(s)
    }
}

pub const STT_KEYWORDS: &[&str] = &[
    "whisper",
    "asr",
    "conformer",
    "sensevoice",
    "speechmatics",
    "transcription",
    "stt",
    "audio-transcription",
];

pub fn infer_capabilities(model_id: &str) -> ModelCapabilities {
    let lower = to_lower_cow(model_id);
    infer_capabilities_lower(&lower)
}

fn infer_capabilities_lower(lower: &str) -> ModelCapabilities {
    const VISION_KW: &[&str] = &[
        "gpt-4o",
        "gpt-4-vision",
        "claude-3",
        "claude-4",
        "gemini",
        "pixtral",
        "qwen-vl",
        "qwen2-vl",
        "qwen3-vl",
        "llava",
        "vision",
        "multimodal",
        "kimi",
        "minicpm-v",
        "internvl",
        "glm-4v",
    ];
    const REASONING_KW: &[&str] = &[
        "o1",
        "o3",
        "o4",
        "reasoning",
        "reasoner",
        "r1",
        "qwq",
        "think",
        "opus-4",
        "deepseek-r1",
    ];

    let mut caps = ModelCapabilities::empty();

    if VISION_KW.iter().any(|k| lower.contains(k)) {
        caps.vision = Some(true);
    }

    if REASONING_KW.iter().any(|k| lower.contains(k)) {
        caps.reasoning = Some(true);
        caps.thinking = Some(true);
    }

    caps.tool_calling = Some(true);
    caps.structured_output = Some(true);
    caps.temperature = Some(true);

    if caps.vision.unwrap_or(false) {
        caps.attachment = Some(true);
    }

    caps
}

pub fn infer_input_modalities(caps: &ModelCapabilities) -> Vec<&'static str> {
    let mut out = vec!["text"];
    if caps.vision.unwrap_or(false) {
        out.push("image");
    }
    out
}

pub fn infer_input_modalities_for_model(
    model_id: &str,
    caps: &ModelCapabilities,
) -> Vec<&'static str> {
    let lower = to_lower_cow(model_id);
    infer_input_modalities_for_model_lower(&lower, caps)
}

fn is_multimodal_embedding(lower: &str) -> bool {
    const EMBED_VL_KW: &[&str] = &["vl", "multimodal", "embed-vl", "gemini-embedding-2"];
    EMBED_VL_KW.iter().any(|k| lower.contains(k))
}

fn is_image_editing_model(lower: &str) -> bool {
    const IMG_EDIT_KW: &[&str] = &["inpaint", "edit", "img2img", "controlnet", "variation"];
    IMG_EDIT_KW.iter().any(|k| lower.contains(k))
}

fn is_native_audio_model(lower: &str) -> bool {
    const AUDIO_CHAT_KW: &[&str] = &["gpt-4o-audio", "gpt-4-audio", "native-audio", "stepaudio"];
    AUDIO_CHAT_KW.iter().any(|k| lower.contains(k))
}

fn infer_chat_input_modalities(lower: &str, caps: &ModelCapabilities) -> Vec<&'static str> {
    let has_vision = caps.vision.unwrap_or(false);
    let has_audio = is_native_audio_model(lower);
    match (has_vision, has_audio) {
        (true, true) => vec!["text", "image", "audio"],
        (true, false) => vec!["text", "image"],
        (false, true) => vec!["text", "audio"],
        (false, false) => vec!["text"],
    }
}

fn infer_input_modalities_for_model_lower(
    lower: &str,
    caps: &ModelCapabilities,
) -> Vec<&'static str> {
    match infer_model_type_lower(lower) {
        "embedding" => {
            if is_multimodal_embedding(lower) {
                vec!["text", "image"]
            } else {
                vec!["text"]
            }
        }
        "rerank" => vec!["text"],
        "image" => {
            if is_image_editing_model(lower) {
                vec!["text", "image"]
            } else {
                vec!["text"]
            }
        }
        "audio" => {
            if STT_KEYWORDS.iter().any(|k| lower.contains(k)) {
                vec!["audio"]
            } else {
                vec!["text"]
            }
        }
        _ => infer_chat_input_modalities(lower, caps),
    }
}

pub fn infer_output_modalities(model_id: &str) -> Vec<&'static str> {
    let lower = to_lower_cow(model_id);
    infer_output_modalities_lower(&lower)
}

fn infer_audio_output_modalities(lower: &str) -> Vec<&'static str> {
    if STT_KEYWORDS.iter().any(|k| lower.contains(k)) {
        vec!["text"]
    } else {
        vec!["audio"]
    }
}

fn infer_chat_output_modalities(lower: &str) -> Vec<&'static str> {
    if is_native_audio_model(lower) {
        vec!["text", "audio"]
    } else {
        vec!["text"]
    }
}

fn infer_output_modalities_lower(lower: &str) -> Vec<&'static str> {
    match infer_model_type_lower(lower) {
        "embedding" => vec!["embedding"],
        "image" => vec!["image"],
        "audio" => infer_audio_output_modalities(lower),
        "rerank" => vec!["text"],
        _ => infer_chat_output_modalities(lower),
    }
}

pub fn infer_default_output_modalities() -> Vec<&'static str> {
    vec!["text"]
}

/// Finds the first entry in `table` where `haystack` contains the pattern key.
#[inline]
pub fn find_substring_match<T: Copy>(haystack: &str, table: &[(&str, T)]) -> Option<T> {
    table
        .iter()
        .find_map(|&(k, v)| if haystack.contains(k) { Some(v) } else { None })
}

/// Checks whether `token` exists in `haystack` bounded by string ends or delimiters (`-`, `_`, `/`, `.`, `:`, ` `).
#[inline]
pub fn contains_delimited_token(haystack: &str, token: &str) -> bool {
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(token) {
        let abs_pos = start + pos;
        let end_pos = abs_pos + token.len();

        let prev_ok = abs_pos == 0
            || matches!(
                haystack.as_bytes()[abs_pos - 1],
                b'/' | b'-' | b'_' | b'.' | b':' | b' '
            );
        let next_ok = end_pos == haystack.len()
            || matches!(
                haystack.as_bytes()[end_pos],
                b'/' | b'-' | b'_' | b'.' | b':' | b' '
            );

        if prev_ok && next_ok {
            return true;
        }
        start = abs_pos + 1;
    }
    false
}

pub fn infer_context_length(model_id: &str) -> Option<i64> {
    const KNOWN: &[(&str, i64)] = &[
        ("claude-3", 200_000),
        ("claude-sonnet-4", 200_000),
        ("claude-opus-4", 200_000),
        ("claude-opus-3", 200_000),
        ("gemini-2.5-pro", 1_048_576),
        ("gemini-2.5-flash", 1_048_576),
        ("gemini-2", 1_048_576),
        ("gemini-1.5-pro", 2_097_152),
        ("gemini-1.5-flash", 1_048_576),
        ("gpt-4o", 128_000),
        ("gpt-4-turbo", 128_000),
        ("gpt-4-32k", 32_000),
        ("o1", 200_000),
        ("o3", 200_000),
        ("deepseek-chat", 64_000),
        ("deepseek-reasoner", 64_000),
        ("llama-3.1-405b", 131_072),
        ("llama-3.1-70b", 131_072),
        ("llama-3.1-8b", 131_072),
        ("llama-3.3-70b", 131_072),
        ("qwen2.5", 32_768),
        ("qwen-max", 32_768),
        ("mistral-large", 128_000),
    ];

    let lower = to_lower_cow(model_id);
    find_substring_match(&lower, KNOWN)
}

pub fn infer_max_output_tokens(model_id: &str) -> Option<i64> {
    const KNOWN: &[(&str, i64)] = &[
        ("claude-3", 8_192),
        ("claude-sonnet-4", 8_192),
        ("claude-opus-4", 32_000),
        ("gpt-4o", 16_384),
        ("o1", 100_000),
        ("o3", 100_000),
        ("gemini-2.5", 65_536),
        ("deepseek", 8_192),
    ];

    let lower = to_lower_cow(model_id);
    find_substring_match(&lower, KNOWN)
}

pub const RERANK_KEYWORDS: &[&str] = &["rerank"];

pub const EMBEDDING_KEYWORDS: &[&str] = &[
    "text-embedding",
    "embedding",
    "embeddings",
    "embedder",
    "model2vec",
    "bge-",
    "/bge-",
    "bge_",
    "bge.",
    "embed-qa",
    "embedcode",
    "pplx-embed",
    "mistral-embed",
    "codestral-embed",
    "arctic-embed",
    "nomic-embed",
    "voyage-embed",
    "nv-embed",
    "gte-",
    "e5-",
    "embed-v",
];

pub const CHAT_GUARD_KEYWORDS: &[&str] = &[
    "gemini",
    "gpt-4",
    "gpt-3",
    "claude",
    "deepseek",
    "qwen",
    "llama",
    "mistral",
    "mixtral",
    "gemma",
    "phi-",
    "kimi",
    "glm-",
    "stepaudio",
    "grok",
];

pub const AUDIO_KEYWORDS: &[&str] = &[
    "deepgram",
    "whisper",
    "speechify",
    "melotts",
    "melo-tts",
    "kokoro",
    "fish-audio",
    "fish-speech",
    "chattts",
    "cosyvoice",
    "openvoice",
    "parler-tts",
    "speechmatics",
    "tts-1",
    "inworld-tts",
    "elevenlabs",
    "eleven-labs",
    "eleven_multilingual",
    "stable-audio",
    "musicgen",
    "audioldm",
    "seamless-m4t",
    "sensevoice",
    "voxtral-mini-tts",
    "xai-tts",
    "grok-stt",
    "-tts",
    "_tts",
    "/tts",
    "preview-tts",
    "-tts-preview",
    "-asr",
];

pub const IMAGE_KEYWORDS: &[&str] = &[
    "dall-e",
    "dalle",
    "midjourney",
    "ideogram",
    "recraft",
    "flux",
    "sdxl",
    "stable-diffusion",
    "stable_diffusion",
    "stablediffusion",
    "stable-image",
    "sd-turbo",
    "sdxl-turbo",
    "sd-1.5",
    "sd-2.1",
    "sd-3",
    "sd-3.5",
    "sd3",
    "sd3.5",
    "imagen-",
    "imagen/",
    "dreamshaper",
    "pony",
    "animagine",
    "zavychroma",
    "novafast",
    "albedobase",
    "edge of realism",
    "zeipher female",
    "mhxl",
    "rag illustrious",
    "mistoon anime",
    "bb95 furry",
    "camelliamix",
    "anything v3",
    "anything v5",
    "perfect world",
    "abyss orangemix",
    "stable cascade",
    "playbookxl",
    "rundiffusion",
    "playground-v2",
    "kandinsky",
    "kolors",
    "auraflow",
    "lumina-image",
    "hunyuan-dit",
    "pixart",
    "cogview",
    "gameart",
    "art of mtg",
    "duchaiten",
    "duc haiten",
    "nai-diffusion",
    "diffusion",
    "seedream",
    "grok-imagine",
    "lucid-origin",
    "nano-banana",
    "quiverai/arrow",
];

pub fn infer_model_type(model_id: &str) -> &'static str {
    let lower = to_lower_cow(model_id);
    infer_model_type_lower(&lower)
}

fn is_embedding_model(lower: &str) -> bool {
    EMBEDDING_KEYWORDS.iter().any(|k| lower.contains(k))
        || (lower.contains("embed")
            && !lower.contains("embedded-")
            && !lower.contains("embed_chat")
            && !lower.contains("embeddable"))
}

pub fn is_chat_guarded_family(lower: &str) -> bool {
    CHAT_GUARD_KEYWORDS.iter().any(|k| lower.contains(k))
        || contains_delimited_token(lower, "o1")
        || contains_delimited_token(lower, "o3")
        || contains_delimited_token(lower, "o4")
}

fn check_chat_guard_model(lower: &str) -> Option<&'static str> {
    if !is_chat_guarded_family(lower) {
        return None;
    }
    if is_image_model(lower) {
        Some("image")
    } else if is_audio_model(lower) {
        Some("audio")
    } else {
        Some("chat")
    }
}

fn is_audio_model(lower: &str) -> bool {
    AUDIO_KEYWORDS.iter().any(|k| lower.contains(k))
        || (lower.contains("telnyx-") && lower.contains("tts"))
}

fn is_image_model(lower: &str) -> bool {
    if lower.contains("diffusiongemma") || lower.contains("sdft") {
        return false;
    }
    IMAGE_KEYWORDS.iter().any(|k| lower.contains(k)) || lower == "imagen"
}

fn infer_model_type_lower(lower: &str) -> &'static str {
    if RERANK_KEYWORDS.iter().any(|k| lower.contains(k)) {
        "rerank"
    } else if is_embedding_model(lower) {
        "embedding"
    } else if let Some(guarded_type) = check_chat_guard_model(lower) {
        guarded_type
    } else if is_audio_model(lower) {
        "audio"
    } else if is_image_model(lower) {
        "image"
    } else {
        "chat"
    }
}

pub fn resolve_effective_model_type<'a>(
    model_type: &'a str,
    custom: bool,
    inferred_type: &'a str,
) -> &'a str {
    if custom {
        if model_type.is_empty() {
            inferred_type
        } else {
            model_type
        }
    } else if model_type.is_empty()
        || (model_type == "chat" && inferred_type != "chat")
        || (model_type == "audio" && inferred_type == "chat")
        || (model_type == "embedding" && inferred_type == "rerank")
    {
        inferred_type
    } else {
        model_type
    }
}

fn serialize_modalities(mods: &[&str]) -> String {
    serde_json::to_string(mods).unwrap_or_else(|_| r#"["text"]"#.to_string())
}

pub fn infer_input_modalities_json(model_id: &str) -> String {
    let lower = to_lower_cow(model_id);
    let caps = infer_capabilities_lower(&lower);
    let mods = infer_input_modalities_for_model_lower(&lower, &caps);
    serialize_modalities(&mods)
}

pub fn infer_output_modalities_json(model_id: &str) -> String {
    let lower = to_lower_cow(model_id);
    let mods = infer_output_modalities_lower(&lower);
    serialize_modalities(&mods)
}

pub fn is_stt_model(model_id: &str) -> bool {
    let lower = to_lower_cow(model_id);
    STT_KEYWORDS.iter().any(|k| lower.contains(k))
}

pub fn infer_family(model_id: &str) -> Option<String> {
    const FAMILIES: &[&str] = &[
        "gpt-4o",
        "gpt-4",
        "gpt-3.5",
        "o1",
        "o3",
        "claude-opus-4",
        "claude-sonnet-4",
        "claude-3.5",
        "claude-3-5",
        "claude-3",
        "gemini-2.5",
        "gemini-1.5",
        "deepseek",
        "llama-3.3",
        "llama-3.1",
        "qwen3",
        "qwen2.5",
        "qwen2",
        "gemma-3",
        "gemma-2",
        "mistral",
        "mixtral",
        "phi-3",
        "nemotron",
        "command-r",
        "cogito",
    ];

    let lower = to_lower_cow(model_id);
    for f in FAMILIES {
        if lower.contains(f) {
            return Some((*f).to_string());
        }
    }
    None
}

#[cfg(test)]
#[path = "capabilities_tests.rs"]
mod tests;
