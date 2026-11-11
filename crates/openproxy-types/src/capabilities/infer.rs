use super::model::ModelCapabilities;
use super::model_type::is_video_model;
use super::util::{contains_delimited_token, to_lower_cow};

pub const DECISION_KEYWORDS: &[&str] = &["jev", "laya", "systemone", "system-one", "decision"];

pub fn is_decision_model(lower: &str) -> bool {
    DECISION_KEYWORDS.iter().any(|k| lower.contains(k))
}

pub fn infer_capabilities(model_id: &str) -> ModelCapabilities {
    let lower = to_lower_cow(model_id);
    infer_capabilities_lower(&lower)
}

pub(crate) fn infer_capabilities_lower(lower: &str) -> ModelCapabilities {
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
        // Vision-language families whose names carry no "vision" keyword. The
        // `-vl` suffix is the standard HuggingFace/NVIDIA marker for a VLM, and
        // Nemotron VL models are multimodal without spelling it out.
        "nemotron",
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

    // `-vl` / `-vlm` suffix: the standard HuggingFace and NVIDIA marker for a
    // vision-language model. Checked as a delimited token so a model like
    // `qwen3-vl-8b` is caught without matching an unrelated `...vl...` substring.
    if !caps.vision.unwrap_or(false)
        && (contains_delimited_token(lower, "vl") || contains_delimited_token(lower, "vlm"))
    {
        caps.vision = Some(true);
    }

    if REASONING_KW.iter().any(|k| lower.contains(k)) {
        caps.reasoning = Some(true);
        caps.thinking = Some(true);
    }

    if is_decision_model(lower) {
        caps.decisions = Some(true);
    }

    if is_video_model(lower) {
        caps.video = Some(true);
    }

    caps.tool_calling = Some(true);
    caps.structured_output = Some(true);
    caps.temperature = Some(true);

    if caps.vision.unwrap_or(false) {
        caps.attachment = Some(true);
    }

    caps
}
