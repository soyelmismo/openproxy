use super::infer::infer_capabilities_lower;
use super::keywords::STT_KEYWORDS;
use super::modalities::{infer_input_modalities_for_model_lower, infer_output_modalities_lower};
use super::util::to_lower_cow;

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
