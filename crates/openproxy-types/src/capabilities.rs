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
        self.vision.is_none()
            && self.tool_calling.is_none()
            && self.reasoning.is_none()
            && self.thinking.is_none()
            && self.attachment.is_none()
            && self.structured_output.is_none()
            && self.temperature.is_none()
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

pub fn infer_capabilities(model_id: &str) -> ModelCapabilities {
    let lower = model_id.to_lowercase();
    let mut caps = ModelCapabilities::empty();

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

    const REASONING_KW: &[&str] = &[
        "o1",
        "o3",
        "o4",
        "reasoning",
        "r1",
        "qwq",
        "think",
        "opus-4",
    ];
    if REASONING_KW.iter().any(|k| lower.contains(k)) {
        caps.reasoning = Some(true);
        caps.thinking = Some(true);
    }

    caps.tool_calling = Some(true);
    caps.structured_output = Some(true);
    caps.temperature = Some(true);

    if caps.vision == Some(true) {
        caps.attachment = Some(true);
    }

    caps
}

pub fn infer_input_modalities(caps: &ModelCapabilities) -> Vec<&'static str> {
    let mut out = vec!["text"];
    if caps.vision == Some(true) {
        out.push("image");
    }
    out
}

pub fn infer_output_modalities() -> Vec<&'static str> {
    vec!["text"]
}

pub fn infer_context_length(model_id: &str) -> Option<i64> {
    let lower = model_id.to_lowercase();

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

    for (k, v) in KNOWN {
        if lower.contains(k) {
            return Some(*v);
        }
    }
    None
}

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

pub fn infer_input_modalities_json(model_id: &str) -> String {
    let caps = infer_capabilities(model_id);
    let mods = infer_input_modalities(&caps);
    serde_json::to_string(&mods).unwrap_or_else(|_| r#"["text"]"#.to_string())
}

pub fn infer_output_modalities_json(model_id: &str) -> String {
    let model_type = infer_model_type(model_id);
    match model_type {
        "image" => r#"["image"]"#.to_string(),
        "audio" => r#"["audio"]"#.to_string(),
        _ => r#"["text"]"#.to_string(),
    }
}

pub fn infer_family(model_id: &str) -> Option<String> {
    let lower = model_id.to_lowercase();
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
        assert_eq!(
            infer_model_type("openai/text-embedding-3-large"),
            "embedding"
        );
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
        assert_eq!(
            infer_family("anthropic/claude-3-5-sonnet-20241022"),
            Some("claude-3-5".to_string())
        );
        assert_eq!(infer_family("unknown/some-model"), None);
    }
}
