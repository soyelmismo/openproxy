//! Heuristics for inferring model capabilities from the `model_id`.
//!
//! Mirrors OmniRoute's `modelCapabilities.ts` heuristics so the public
//! `GET /v1/models` endpoint can hand clients like Cursor and Cline a
//! `context_length`, `vision`/`tool_calling`/`reasoning` flags, and
//! input/output modalities without the operator having to fill them
//! in by hand. When the operator *does* fill them in (by editing the
//! row in the admin UI), the DB value takes precedence and the
//! heuristic is only consulted as a fallback.
//!
//! All infer functions take `&str` and return either an `Option` or a
//! borrowed `&'static str` so they are zero-allocation on the happy
//! path; the few `String`-returning helpers (`infer_*_json`) only
//! allocate when the caller actually asks for serialized JSON.

use serde::{Deserialize, Serialize};

/// Capability flags surfaced to clients via `GET /v1/models`.
///
/// Every field is `Option<bool>` rather than `bool` so the JSON
/// projection can distinguish "explicitly false" from "unknown" — the
/// heuristic only sets the fields it has evidence for, and the JSON
/// output omits `None` values via `skip_serializing_if` so the wire
/// shape stays compact for models that only have a few known
/// capabilities.
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
    /// Build an empty capability set. Every field is `None`; `to_json`
    /// returns `None` for this state.
    pub fn empty() -> Self {
        Self::default()
    }

    /// True when no field has been set. Used by [`Self::to_json`] to
    /// decide whether to emit `None` or a serialized object.
    pub fn is_empty(&self) -> bool {
        self.vision.is_none()
            && self.tool_calling.is_none()
            && self.reasoning.is_none()
            && self.thinking.is_none()
            && self.attachment.is_none()
            && self.structured_output.is_none()
            && self.temperature.is_none()
    }

    /// Serialize to a JSON string. Returns `None` when every field is
    /// unset so the caller can store `NULL` in `capabilities_json`
    /// rather than the string `"{}"`.
    pub fn to_json(&self) -> Option<String> {
        if self.is_empty() {
            None
        } else {
            serde_json::to_string(self).ok()
        }
    }

    /// Parse a stored JSON blob back into a `ModelCapabilities`.
    /// Tolerant of `None`/empty strings/bad JSON — all of those fall
    /// back to `Self::empty()`.
    pub fn from_json(s: Option<&str>) -> Self {
        s.and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_else(Self::empty)
    }
}

/// Infer capabilities from a `model_id` using keyword heuristics.
/// Returns a `ModelCapabilities` with the inferred fields; missing
/// fields mean "unknown" and stay as `None`.
pub fn infer_capabilities(model_id: &str) -> ModelCapabilities {
    let lower = model_id.to_lowercase();
    let mut caps = ModelCapabilities::empty();

    // Vision: gpt-4o, claude-3+, gemini, pixtral, qwen-vl, llava, kimi.
    const VISION_KW: &[&str] = &[
        "gpt-4o",
        "gpt-4-vision",
        "claude-3",
        "claude-4",
        "gemini",
        "pixtral",
        "qwen-vl",
        "qwen2-vl",
        "llava",
        "vision",
        "multimodal",
        "kimi",
    ];
    if VISION_KW.iter().any(|k| lower.contains(k)) {
        caps.vision = Some(true);
    }

    // Reasoning / thinking: o1, o3, deepseek-r1, qwq, etc.
    const REASONING_KW: &[&str] = &[
        "o1", "o3", "o4", "reasoning", "r1", "qwq", "think", "opus-4",
    ];
    if REASONING_KW.iter().any(|k| lower.contains(k)) {
        caps.reasoning = Some(true);
        caps.thinking = Some(true);
    }

    // Default assumptions for modern chat models.
    caps.tool_calling = Some(true);
    caps.structured_output = Some(true);
    caps.temperature = Some(true);

    // Attachment mirrors vision — a model that can see images can
    // also accept image attachments from the user.
    if caps.vision == Some(true) {
        caps.attachment = Some(true);
    }

    caps
}

/// Infer the input modalities from a `ModelCapabilities`. Always
/// includes `text`; adds `image` when vision is on.
pub fn infer_input_modalities(caps: &ModelCapabilities) -> Vec<&'static str> {
    let mut out = vec!["text"];
    if caps.vision == Some(true) {
        out.push("image");
    }
    out
}

/// Default output modalities for a chat model — text only.
pub fn infer_output_modalities() -> Vec<&'static str> {
    vec!["text"]
}

/// Infer the upstream context window (input + output) from a
/// `model_id`. Returns `None` for unknown models so the caller can
/// fall back to a generic default (128k) at the public endpoint.
pub fn infer_context_length(model_id: &str) -> Option<i64> {
    let lower = model_id.to_lowercase();

    // Substring-keyed lookup. Order matters: longer / more specific
    // prefixes are matched first by being listed first in the table,
    // but since we use `contains` (not prefix match) we put the most
    // specific strings near the top of the list so e.g. `gpt-4o`
    // beats `gpt-4`.
    const KNOWN: &[(&str, i64)] = &[
        // Claude
        ("claude-3", 200_000),
        ("claude-sonnet-4", 200_000),
        ("claude-opus-4", 200_000),
        ("claude-opus-3", 200_000),
        // Gemini
        ("gemini-2.5-pro", 1_048_576),
        ("gemini-2.5-flash", 1_048_576),
        ("gemini-2", 1_048_576),
        ("gemini-1.5-pro", 2_097_152),
        ("gemini-1.5-flash", 1_048_576),
        // GPT-4o / o-series
        ("gpt-4o", 128_000),
        ("gpt-4-turbo", 128_000),
        ("gpt-4-32k", 32_000),
        ("o1", 200_000),
        ("o3", 200_000),
        // DeepSeek
        ("deepseek-chat", 64_000),
        ("deepseek-reasoner", 64_000),
        // Llama
        ("llama-3.1-405b", 131_072),
        ("llama-3.1-70b", 131_072),
        ("llama-3.1-8b", 131_072),
        ("llama-3.3-70b", 131_072),
        // Qwen
        ("qwen2.5", 32_768),
        ("qwen-max", 32_768),
        // Mistral
        ("mistral-large", 128_000),
    ];

    for (k, v) in KNOWN {
        if lower.contains(k) {
            return Some(*v);
        }
    }
    None
}

/// Infer the upstream max output tokens from a `model_id`. Returns
/// `None` for unknown models; the public endpoint falls back to 8 192.
pub fn infer_max_output_tokens(model_id: &str) -> Option<i64> {
    let lower = model_id.to_lowercase();

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

    for (k, v) in KNOWN {
        if lower.contains(k) {
            return Some(*v);
        }
    }
    None
}

/// Classify a `model_id` into a `model_type` ("chat" by default).
/// Used both by the heuristic and the backfill path.
pub fn infer_model_type(model_id: &str) -> &'static str {
    let lower = model_id.to_lowercase();
    if lower.contains("embed") {
        "embedding"
    } else if lower.contains("dall-e") || lower.contains("flux") {
        "image"
    } else if lower.contains("whisper") || lower.contains("tts") {
        "audio"
    } else if lower.contains("rerank") {
        "rerank"
    } else {
        "chat"
    }
}

/// JSON-serialized input modalities. Allocates a `String`.
pub fn infer_input_modalities_json(model_id: &str) -> String {
    let caps = infer_capabilities(model_id);
    let mods = infer_input_modalities(&caps);
    serde_json::to_string(&mods).unwrap_or_else(|_| r#"["text"]"#.to_string())
}

/// JSON-serialized output modalities. Allocates a `String`.
pub fn infer_output_modalities_json(model_id: &str) -> String {
    let model_type = infer_model_type(model_id);
    match model_type {
        "image" => r#"["image"]"#.to_string(),
        "audio" => r#"["audio"]"#.to_string(),
        _ => r#"["text"]"#.to_string(),
    }
}

/// Map a `model_id` to a `family` string used by clients that group
/// related models in their UI (e.g. Cursor's model picker). Returns
/// `None` for unrecognized ids.
pub fn infer_family(model_id: &str) -> Option<String> {
    let lower = model_id.to_lowercase();
    // Order matters: more specific patterns first. `claude-3` is a
    // substring of `claude-3.5`, so `claude-3.5` has to be listed
    // first. We also test the hyphenated variant
    // (`claude-3-5-sonnet-…`) because real Anthropic model ids use
    // hyphens, not dots.
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
        "qwen2.5",
    ];
    for f in FAMILIES {
        if lower.contains(f) {
            return Some((*f).to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn infer_vision_for_gpt4o() {
        let caps = infer_capabilities("openai/gpt-4o");
        assert_eq!(caps.vision, Some(true));
    }

    #[test]
    fn infer_no_vision_for_gpt35() {
        let caps = infer_capabilities("openai/gpt-3.5-turbo");
        assert_eq!(caps.vision, None);
    }

    #[test]
    fn infer_reasoning_for_o1() {
        let caps = infer_capabilities("openai/o1-preview");
        assert_eq!(caps.reasoning, Some(true));
        assert_eq!(caps.thinking, Some(true));
    }

    #[test]
    fn attachment_mirrors_vision() {
        let caps = infer_capabilities("anthropic/claude-3-5-sonnet");
        assert_eq!(caps.vision, Some(true));
        assert_eq!(caps.attachment, Some(true));
    }

    #[test]
    fn tool_calling_default_true() {
        let caps = infer_capabilities("some/random-model");
        assert_eq!(caps.tool_calling, Some(true));
        assert_eq!(caps.structured_output, Some(true));
        assert_eq!(caps.temperature, Some(true));
    }

    #[test]
    fn context_length_for_known_models() {
        assert_eq!(
            infer_context_length("anthropic/claude-sonnet-4"),
            Some(200_000)
        );
        assert_eq!(
            infer_context_length("google/gemini-2.5-pro"),
            Some(1_048_576)
        );
        assert_eq!(infer_context_length("openai/gpt-4o"), Some(128_000));
    }

    #[test]
    fn model_type_classification() {
        assert_eq!(infer_model_type("text-embedding-3-small"), "embedding");
        assert_eq!(infer_model_type("openai/text-embedding-3-large"), "embedding");
        assert_eq!(infer_model_type("dall-e-3"), "image");
        assert_eq!(infer_model_type("black-forest-labs/flux-1.1"), "image");
        assert_eq!(infer_model_type("gpt-4o"), "chat");
        assert_eq!(infer_model_type("whisper-1"), "audio");
        assert_eq!(infer_model_type("cohere/rerank-v3"), "rerank");
    }

    #[test]
    fn family_inference() {
        assert_eq!(
            infer_family("openai/gpt-4o-mini"),
            Some("gpt-4o".to_string())
        );
        // Anthropic model ids use hyphens, so the family resolves to
        // the hyphenated `claude-3-5` form. Both `claude-3-5` and the
        // dot-style `claude-3.5` are in the lookup table.
        assert_eq!(
            infer_family("anthropic/claude-3-5-sonnet-20241022"),
            Some("claude-3-5".to_string())
        );
        assert_eq!(infer_family("unknown/some-model"), None);
    }

    #[test]
    fn input_modalities_json_shape() {
        let json = infer_input_modalities_json("openai/gpt-4o");
        let parsed: Vec<&str> = serde_json::from_str(&json).unwrap();
        assert!(parsed.contains(&"text"));
        assert!(parsed.contains(&"image"));
    }

    #[test]
    fn output_modalities_json_default() {
        let json = infer_output_modalities_json("gpt-4o");
        assert_eq!(json, r#"["text"]"#);
    }

    #[test]
    fn capabilities_serde_omits_none() {
        // Only vision set → the serialized JSON should not include
        // the other fields.
        let caps = ModelCapabilities {
            vision: Some(true),
            ..ModelCapabilities::empty()
        };
        let json = serde_json::to_string(&caps).unwrap();
        assert!(json.contains("vision"));
        assert!(!json.contains("tool_calling"));
        assert!(!json.contains("reasoning"));
    }

    #[test]
    fn empty_caps_to_json_is_none() {
        assert!(ModelCapabilities::empty().to_json().is_none());
    }

    #[test]
    fn from_json_tolerates_bad_input() {
        assert!(ModelCapabilities::from_json(None).is_empty());
        assert!(ModelCapabilities::from_json(Some("not json")).is_empty());
        assert!(ModelCapabilities::from_json(Some("{}")).is_empty());
    }
}
