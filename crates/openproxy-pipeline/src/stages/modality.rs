//! Input-modality gate.
//!
//! A combo advertises the *union* of its leaves' capabilities, so a combo with
//! one vision-capable leaf and ten text-only leaves still reports
//! `input_modalities: ["text", "image"]` in `/v1/models`. Without a gate the
//! router happily walks the text-only hops first and the upstream rejects the
//! request with a 400 that carries no retry signal, so the whole combo walk
//! dies on a target that could never have served the request.
//!
//! This stage-level gate inspects the outgoing payload and drops every resolved
//! target whose model does not declare the modalities the request actually
//! carries. Sub-combos are already flattened into leaf targets by
//! [`crate::Pipeline::flatten_targets`] before this point, so the decision is
//! made per real model, not per nested combo.

use crate::context::ResolvedTarget;
use openproxy_types::config::ModalityRoutingConfig;
use openproxy_types::models::ModalityFlags;

/// The non-text input modalities a request carries.
///
/// Text is always accepted, so it is never part of the requirement set: a
/// text-only request must keep every target.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequiredModalities {
    pub image: bool,
    pub video: bool,
    pub audio: bool,
    pub file: bool,
}

impl RequiredModalities {
    /// True when the request needs at least one non-text modality.
    pub fn any(&self) -> bool {
        self.image || self.video || self.audio || self.file
    }

    fn flags(&self) -> u8 {
        let mut flags = 0u8;
        if self.image {
            flags |= ModalityFlags::IMAGE;
        }
        if self.video {
            flags |= ModalityFlags::VIDEO;
        }
        if self.audio {
            flags |= ModalityFlags::AUDIO;
        }
        flags
    }

    /// A file part is not a modality of its own: a PDF is an image-ish
    /// document and every provider that accepts `file` also accepts images.
    /// Treating it as an image requirement keeps the gate useful instead of
    /// dropping every target whose catalog never listed `file`.
    fn gate_flags(&self) -> u8 {
        let mut flags = self.flags();
        if self.file {
            flags |= ModalityFlags::IMAGE;
        }
        flags
    }
}

/// Returns the modalities the request needs.
///
/// Recognises every shape that reaches the pipeline: OpenAI chat (`image_url`,
/// `input_audio`, `file`), OpenAI Responses (`input_image`, `input_file`),
/// Anthropic (`image` / `video` / `document` with a `source` block) and the
/// bare union members clients sometimes send without a `type`.
pub fn request_required_modalities(req: &openproxy_types::OpenAIRequest) -> RequiredModalities {
    let mut required = RequiredModalities::default();
    for msg in &req.messages {
        match msg.content.as_ref() {
            Some(serde_json::Value::Array(parts)) => {
                for part in parts {
                    accumulate_part(part, &mut required);
                }
            }
            Some(value @ serde_json::Value::Object(_)) => accumulate_part(value, &mut required),
            _ => {}
        }
    }
    required
}

fn accumulate_part(part: &serde_json::Value, required: &mut RequiredModalities) {
    if let Some(kind) = part.get("type").and_then(|v| v.as_str()) {
        match kind {
            "image_url" | "input_image" | "image" => required.image = true,
            "input_video" | "video_url" | "video" => required.video = true,
            "input_audio" | "audio" => required.audio = true,
            "file" | "input_file" | "document" => required.file = true,
            _ => {}
        }
    }
    // Some clients omit `type` and send the bare union member.
    if part.get("image_url").is_some() || part.get("input_image").is_some() {
        required.image = true;
    }
    if part.get("video_url").is_some() || part.get("input_video").is_some() {
        required.video = true;
    }
    if part.get("input_audio").is_some() || part.get("audio_url").is_some() {
        required.audio = true;
    }
    if part.get("file").is_some() || part.get("file_id").is_some() {
        required.file = true;
    }
}

/// Returns `true` when any message carries an image content part.
///
/// Kept for callers that only care about the vision gate; prefer
/// [`request_required_modalities`] when the other modalities matter too.
pub fn request_requires_image_input(req: &openproxy_types::OpenAIRequest) -> bool {
    request_required_modalities(req).image
}

/// Whether a target can accept every modality the request carries.
///
/// A model that never declared its modalities is treated as compatible: the
/// gate only removes targets we have positive evidence about. Silently
/// excluding unknown models would break combos over providers whose catalog
/// refresh does not report modalities.
fn target_accepts(target: &ResolvedTarget, required: &RequiredModalities) -> bool {
    let Some(_) = target.model.input_modalities_json else {
        return true;
    };
    let declared = target.model.input_modalities();
    let needed = required.gate_flags();
    declared.0 & needed == needed
}

/// Resolves the effective gate config for one combo walk.
///
/// A per-combo `modality_gate` override wins over the global `enabled` flag;
/// the fallback policy is global because it describes how the proxy answers
/// when a request cannot be served at all, not a per-combo routing taste.
pub fn effective_config(
    global: ModalityRoutingConfig,
    combo_override: Option<bool>,
) -> ModalityRoutingConfig {
    match combo_override {
        Some(enabled) => ModalityRoutingConfig { enabled, ..global },
        None => global,
    }
}

/// Applies the gate. Returns the target list unchanged when the request is
/// text-only or the feature is disabled.
pub fn filter_targets_for_input(
    targets: Vec<ResolvedTarget>,
    requires_image: bool,
    config: ModalityRoutingConfig,
) -> Vec<ResolvedTarget> {
    filter_targets_for_modalities(
        targets,
        RequiredModalities {
            image: requires_image,
            ..Default::default()
        },
        config,
    )
}

/// Applies the gate for any combination of input modalities.
pub fn filter_targets_for_modalities(
    targets: Vec<ResolvedTarget>,
    required: RequiredModalities,
    config: ModalityRoutingConfig,
) -> Vec<ResolvedTarget> {
    if !config.enabled || !required.any() {
        return targets;
    }

    let total = targets.len();
    let mut kept = Vec::with_capacity(total);
    let mut dropped = 0usize;
    for target in &targets {
        if target_accepts(target, &required) {
            kept.push(target.clone());
        } else {
            dropped += 1;
            tracing::debug!(
                target_id = target.target.id.0,
                provider = %target.target.provider_id,
                model = %target.model.model_id,
                image = required.image,
                video = required.video,
                audio = required.audio,
                file = required.file,
                "modality gate: dropping target that cannot accept the request's media input"
            );
        }
    }

    if kept.is_empty() {
        if config.fallback_to_text_only {
            tracing::warn!(
                dropped,
                "modality gate: no target accepts the request's media input; falling back to the unfiltered target set"
            );
            return targets;
        }
        return kept;
    }

    if dropped > 0 {
        tracing::info!(
            kept = kept.len(),
            dropped,
            "modality gate: media request restricted to capable targets"
        );
    }
    kept
}

#[cfg(test)]
mod tests;
