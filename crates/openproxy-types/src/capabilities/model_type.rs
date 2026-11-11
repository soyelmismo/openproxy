use super::infer::is_decision_model;
use super::keywords::{
    AUDIO_KEYWORDS, CHAT_GUARD_KEYWORDS, EMBEDDING_KEYWORDS, IMAGE_KEYWORDS, RERANK_KEYWORDS,
    VIDEO_KEYWORDS, VIDEO_TOKENS,
};
use super::util::{contains_delimited_token, to_lower_cow};

pub fn is_video_model(lower: &str) -> bool {
    VIDEO_KEYWORDS.iter().any(|k| lower.contains(k))
        || VIDEO_TOKENS
            .iter()
            .any(|t| contains_delimited_token(lower, t))
}

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

pub(crate) fn infer_model_type_lower(lower: &str) -> &'static str {
    if is_decision_model(lower) {
        "decision"
    } else if RERANK_KEYWORDS.iter().any(|k| lower.contains(k)) {
        "rerank"
    } else if is_embedding_model(lower) {
        "embedding"
    } else if let Some(guarded_type) = check_chat_guard_model(lower) {
        guarded_type
    } else if is_video_model(lower) {
        // Before image/audio: `stable-video-diffusion` or `kling-tts`-style
        // composites must keep their video-generation classification.
        "video"
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
