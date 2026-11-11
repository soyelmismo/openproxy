use super::keywords::STT_KEYWORDS;
use super::model::ModelCapabilities;
use super::model_type::infer_model_type_lower;
use super::util::to_lower_cow;

pub fn infer_input_modalities(caps: &ModelCapabilities) -> Vec<&'static str> {
    let mut out = vec!["text"];
    if caps.vision.unwrap_or(false) {
        out.push("image");
    }
    if caps.video.unwrap_or(false) {
        out.push("video");
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
    // NVIDIA's embedding family is multimodal, but the version number sits
    // between the vendor and the word: `nemotron-3.5-embed-8b` never contains
    // the literal `nemotron-embed`, so a keyword list cannot catch it. The two
    // halves are matched separately instead.
    if lower.contains("nemotron") && lower.contains("embed") {
        return true;
    }
    const EMBED_VL_KW: &[&str] = &["vl", "multimodal", "embed-vl", "gemini-embedding-2"];
    EMBED_VL_KW.iter().any(|k| lower.contains(k))
}

/// gemini-embedding-2 and its previews: the only embedding models that accept
/// text, image, audio, video and documents alike.
///
/// The `-2` is load-bearing. `gemini-embedding-001` is text-only, and the
/// preview shares the second generation's modalities, so both match while the
/// first generation does not.
fn is_gemini_embedding_2(lower: &str) -> bool {
    lower.contains("gemini-embedding-2")
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
    let has_video = caps.video.unwrap_or(false);
    let mut out = vec!["text"];
    if has_vision {
        out.push("image");
    }
    if has_audio {
        out.push("audio");
    }
    if has_video {
        out.push("video");
    }
    out
}

pub(crate) fn infer_input_modalities_for_model_lower(
    lower: &str,
    caps: &ModelCapabilities,
) -> Vec<&'static str> {
    match infer_model_type_lower(lower) {
        "embedding" => {
            // gemini-embedding-2 is the only embedding model here that takes
            // every modality, documents included. The rest of the multimodal
            // embeddings are text plus image, which is what their cards say.
            if is_gemini_embedding_2(lower) {
                vec!["text", "image", "audio", "video", "document"]
            } else if is_multimodal_embedding(lower) {
                vec!["text", "image"]
            } else {
                vec!["text"]
            }
        }
        "rerank" => vec!["text"],
        "decision" => vec!["text"],
        "video" => {
            // Generative video models accept a prompt plus optional reference
            // media. No reliable keyword tells image reference support apart,
            // so advertise both and let upstream validation decide.
            vec!["text", "image"]
        }
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

pub(crate) fn infer_output_modalities_lower(lower: &str) -> Vec<&'static str> {
    match infer_model_type_lower(lower) {
        "decision" => vec!["decision"],
        "embedding" => vec!["embedding"],
        "image" => vec!["image"],
        "video" => vec!["video"],
        "audio" => infer_audio_output_modalities(lower),
        "rerank" => vec!["text"],
        _ => infer_chat_output_modalities(lower),
    }
}

pub fn infer_default_output_modalities() -> Vec<&'static str> {
    vec!["text"]
}
