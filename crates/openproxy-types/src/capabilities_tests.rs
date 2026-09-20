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
    assert_eq!(infer_model_type("baai/bge-large-en-v1.5"), "embedding");
    assert_eq!(infer_model_type("qwen3-embedding-8b"), "embedding");
    assert_eq!(infer_model_type("dall-e-3"), "image");
    assert_eq!(infer_model_type("black-forest-labs/flux-1.1"), "image");
    assert_eq!(infer_model_type("sdxl-lightning"), "image");
    assert_eq!(infer_model_type("gpt-4o"), "chat");
    assert_eq!(infer_model_type("whisper-1"), "audio");
    assert_eq!(infer_model_type("inworld-tts2"), "audio");
    assert_eq!(infer_model_type("melotts"), "audio");
    assert_eq!(infer_model_type("cohere/rerank-v3"), "rerank");
    assert_eq!(
        infer_model_type("accounts/fireworks/models/qwen3-reranker-8b"),
        "rerank"
    );
    assert_eq!(infer_model_type("typesafe/jev-latest"), "decision");
    assert_eq!(infer_model_type("convaiinnovations/laya"), "decision");
    assert_eq!(infer_model_type("systemone-router"), "decision");
}

#[test]
fn infer_decisions_capability() {
    let caps = infer_capabilities("typesafe/jev-latest");
    assert_eq!(caps.decisions, Some(true));
    let caps_laya = infer_capabilities("convaiinnovations/laya");
    assert_eq!(caps_laya.decisions, Some(true));
    let caps_chat = infer_capabilities("openai/gpt-4o");
    assert_eq!(caps_chat.decisions, None);
}

#[test]
fn false_positive_guards() {
    assert_eq!(
        infer_model_type("YoannDev90/diffusiongemma-26b-a4b-it:free"),
        "chat"
    );
    assert_eq!(infer_model_type("gemma-4-31b-sdft-heretic-rp"), "chat");
    assert_eq!(infer_model_type("gpt-4o-audio-preview"), "chat");
    assert_eq!(infer_model_type("gemini-2.0-flash-lite"), "chat");
    assert_eq!(
        infer_model_type("gemini-2.0-flash-lite-preview-02-05"),
        "chat"
    );
    assert_eq!(infer_model_type("gemini-1.5-flash-8b"), "chat");
    assert_eq!(infer_model_type("gemini-2.5-flash"), "chat");
    assert_eq!(
        infer_model_type("gemini-2.0-flash-thinking-exp-01-21"),
        "chat"
    );
    assert_eq!(infer_model_type("imagen-3.0-generate-002"), "image");
    assert_eq!(infer_model_type("google/imagen-3"), "image");
    assert_eq!(infer_model_type("deepgram-flux"), "audio");
    assert_eq!(infer_model_type("@cf/deepgram/flux"), "audio");
}

#[test]
fn modality_inference_consistency() {
    assert_eq!(
        infer_output_modalities("text-embedding-3-small"),
        vec!["embedding"]
    );
    assert_eq!(
        infer_output_modalities("black-forest-labs/flux-1.1"),
        vec!["image"]
    );
    assert_eq!(infer_output_modalities("whisper-1"), vec!["text"]);
    assert_eq!(
        infer_input_modalities_for_model("whisper-1", &ModelCapabilities::empty()),
        vec!["audio"]
    );
    assert_eq!(infer_output_modalities("tts-1"), vec!["audio"]);
    assert_eq!(
        infer_input_modalities_for_model("tts-1", &ModelCapabilities::empty()),
        vec!["text"]
    );
    assert_eq!(infer_output_modalities("gpt-4o"), vec!["text"]);
    assert_eq!(
        infer_output_modalities("gpt-4o-audio-preview"),
        vec!["text", "audio"]
    );
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

#[test]
fn infer_max_output_tokens_known() {
    assert_eq!(
        infer_max_output_tokens("anthropic/claude-sonnet-4"),
        Some(8_192)
    );
    assert_eq!(
        infer_max_output_tokens("google/gemini-2.5-pro"),
        Some(65_536)
    );
    assert_eq!(infer_max_output_tokens("openai/gpt-4o"), Some(16_384));
    assert_eq!(
        infer_max_output_tokens("deepseek/deepseek-chat"),
        Some(8_192)
    );
}

#[test]
fn infer_max_output_tokens_unknown() {
    assert_eq!(infer_max_output_tokens("unknown/some-model"), None);
}

#[test]
fn test_find_substring_match() {
    const TABLE: &[(&str, u32)] = &[("alpha", 1), ("beta", 2), ("gamma", 3)];
    assert_eq!(find_substring_match("contains-beta-here", TABLE), Some(2));
    assert_eq!(find_substring_match("contains-alpha-here", TABLE), Some(1));
    assert_eq!(find_substring_match("contains-none", TABLE), None);
}

#[test]
fn test_resolve_effective_model_type() {
    const RESOLVE_CASES: &[(&str, bool, &str, &str)] = &[
        ("audio", false, "chat", "chat"),
        ("image", false, "chat", "image"),
        ("", false, "chat", "chat"),
        ("chat", false, "embedding", "embedding"),
        ("embedding", false, "rerank", "rerank"),
        ("chat", false, "image", "image"),
        ("chat", false, "audio", "audio"),
        ("audio", true, "chat", "audio"),
        ("audio", false, "audio", "audio"),
    ];
    for &(endpoint, has_audio, inferred, expected) in RESOLVE_CASES {
        assert_eq!(
            resolve_effective_model_type(endpoint, has_audio, inferred),
            expected,
            "resolve failed for ({endpoint}, {has_audio}, {inferred})"
        );
    }

    const INFER_CASES: &[(&str, &str)] = &[
        ("gemini-3.7-flash-low", "chat"),
        ("gemini-2.5-flash-preview-tts", "audio"),
        ("grok-4", "chat"),
        ("grok-stt", "audio"),
        ("grok-imagine-image", "image"),
        ("spacexai/grok-imagine-image-2.0", "image"),
        ("sdxl-aungir-t6ao45", "image"),
        ("sao10k/l3.3-euryale-70b", "chat"),
        ("qwen/qwen3-asr-flash", "audio"),
        ("gpt-4o-mini-tts:free", "audio"),
        ("seedream-v4", "image"),
        ("nano-banana-pro", "image"),
        ("lucid-origin", "image"),
        ("@cf/baai/bge-reranker-base", "rerank"),
    ];
    for &(model_id, expected) in INFER_CASES {
        assert_eq!(
            infer_model_type(model_id),
            expected,
            "infer failed for {model_id}"
        );
    }
}
