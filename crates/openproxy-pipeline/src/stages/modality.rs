//! Input-modality gate.
//!
//! A combo advertises the *union* of its leaves' capabilities, so a combo with
//! one vision-capable leaf and ten text-only leaves still reports
//! `input_modalities: ["text", "image"]` in `/v1/models`. Without a gate the
//! router happily walks the text-only hops first and the upstream rejects the
//! request with a 400 that carries no retry signal, so the whole combo walk
//! dies on a target that could never have served the request.
//!
//! This stage-level gate inspects the outgoing payload and, when it carries
//! image parts, drops every resolved target whose model does not declare
//! `image` in its input modalities. Sub-combos are already flattened into leaf
//! targets by [`crate::Pipeline::flatten_targets`] before this point, so the
//! decision is made per real model, not per nested combo.

use crate::context::ResolvedTarget;
use openproxy_types::config::ModalityRoutingConfig;

/// Returns `true` when any message carries an image content part.
///
/// Recognises the three shapes that reach the pipeline: OpenAI chat
/// (`image_url`), OpenAI Responses (`input_image`, normalised to `image_url`
/// before the pipeline) and Anthropic (`image` with a `source` block).
pub fn request_requires_image_input(req: &openproxy_types::OpenAIRequest) -> bool {
    req.messages.iter().any(|msg| match msg.content.as_ref() {
        Some(serde_json::Value::Array(parts)) => parts.iter().any(part_is_image),
        Some(value @ serde_json::Value::Object(_)) => part_is_image(value),
        _ => false,
    })
}

fn part_is_image(part: &serde_json::Value) -> bool {
    if let Some(kind) = part.get("type").and_then(|v| v.as_str())
        && matches!(kind, "image_url" | "input_image" | "image")
    {
        return true;
    }
    // Some clients omit `type` and send the bare union member.
    part.get("image_url").is_some() || part.get("input_image").is_some()
}

/// Whether a target can accept image input.
///
/// A model that never declared its modalities is treated as compatible: the
/// gate only removes targets we have positive evidence about. Silently
/// excluding unknown models would break combos over providers whose catalog
/// refresh does not report modalities.
fn target_accepts_image(target: &ResolvedTarget) -> bool {
    target.model.input_modalities_json.is_none() || target.model.input_modalities().has_image()
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
    if !config.enabled || !requires_image {
        return targets;
    }

    let total = targets.len();
    let mut kept = Vec::with_capacity(total);
    let mut dropped = 0usize;
    for target in &targets {
        if target_accepts_image(target) {
            kept.push(target.clone());
        } else {
            dropped += 1;
            tracing::debug!(
                target_id = target.target.id.0,
                provider = %target.target.provider_id,
                model = %target.model.model_id,
                "modality gate: dropping target that cannot accept image input"
            );
        }
    }

    if kept.is_empty() {
        if config.fallback_to_text_only {
            tracing::warn!(
                dropped,
                "modality gate: no target accepts image input; falling back to the unfiltered target set"
            );
            return targets;
        }
        return kept;
    }

    if dropped > 0 {
        tracing::info!(
            kept = kept.len(),
            dropped,
            "modality gate: image request restricted to vision-capable targets"
        );
    }
    kept
}

#[cfg(test)]
mod tests;
