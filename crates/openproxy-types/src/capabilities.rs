//! Heuristics for inferring model capabilities from the `model_id`.

mod infer;
mod json;
mod keywords;
mod limits;
mod modalities;
mod model;
mod model_type;
mod util;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod vl_inference_tests;

pub use infer::{DECISION_KEYWORDS, infer_capabilities, is_decision_model};
pub use json::{
    infer_family, infer_input_modalities_json, infer_output_modalities_json, is_stt_model,
};
pub use keywords::{
    AUDIO_KEYWORDS, CHAT_GUARD_KEYWORDS, EMBEDDING_KEYWORDS, IMAGE_KEYWORDS, RERANK_KEYWORDS,
    STT_KEYWORDS, VIDEO_KEYWORDS, VIDEO_TOKENS,
};
pub use limits::{infer_context_length, infer_max_output_tokens};
pub use modalities::{
    infer_default_output_modalities, infer_input_modalities, infer_input_modalities_for_model,
    infer_output_modalities,
};
pub use model::ModelCapabilities;
pub use model_type::{
    infer_model_type, is_chat_guarded_family, is_video_model, resolve_effective_model_type,
};
pub use util::{contains_delimited_token, find_substring_match};
