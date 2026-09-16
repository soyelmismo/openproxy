use super::prompt::{
    HordeLora, HordeTi, clean_residual_prompt, parse_dimensions, parse_prompt_directives,
};
use bytes::Bytes;
use openproxy_types::{CoreError, ImageGenerationRequest, Result};
use serde::Serialize;
use std::collections::HashSet;

#[derive(Debug, Serialize)]
pub(crate) struct HordeGenerationParams {
    pub(crate) n: u32,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) steps: u32,
    pub(crate) sampler_name: String,
    pub(crate) cfg_scale: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) denoising_strength: Option<f32>,
    pub(crate) karras: bool,
    pub(crate) hires_fix: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) hires_fix_denoising_strength: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) clip_skip: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) control_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) post_processing: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) loras: Option<Vec<HordeLora>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tis: Option<Vec<HordeTi>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workers: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
pub(crate) struct HordeGenerationPayload {
    pub(crate) prompt: String,
    pub(crate) params: HordeGenerationParams,
    pub(crate) models: Vec<String>,
    pub(crate) nsfw: bool,
    pub(crate) censor_nsfw: bool,
    pub(crate) r2: bool,
    pub(crate) slow_workers: bool,
    pub(crate) trusted_workers: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) workers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_mask: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_processing: Option<String>,
}

pub(crate) fn assemble_final_prompt(
    clean_prompt: &str,
    req_neg: Option<&str>,
    parsed_negs: &[String],
) -> String {
    let mut all_negatives = Vec::new();
    let mut seen_negatives = HashSet::new();
    if let Some(neg) = req_neg {
        let clean_neg = clean_residual_prompt(neg);
        if !clean_neg.is_empty() {
            seen_negatives.insert(clean_neg.clone());
            all_negatives.push(clean_neg);
        }
    }
    for n in parsed_negs {
        let clean_n = clean_residual_prompt(n);
        if !clean_n.is_empty() && seen_negatives.insert(clean_n.clone()) {
            all_negatives.push(clean_n);
        }
    }

    let mut prompt = clean_prompt.to_string();
    if !all_negatives.is_empty() {
        prompt.push_str(" ### ");
        prompt.push_str(&all_negatives.join(", "));
    }
    prompt
}

pub(crate) fn assemble_post_processing(
    quality: Option<&str>,
    req_pp: Option<&[String]>,
    parsed_pp: &[String],
) -> Option<Vec<String>> {
    let mut post_processing_list = Vec::new();
    let mut seen_post_processing = HashSet::new();
    if quality == Some("hd") {
        for item in ["RealESRGAN_x4plus", "GFPGAN"] {
            if seen_post_processing.insert(item) {
                post_processing_list.push(item.to_string());
            }
        }
    }
    if let Some(req_pp) = req_pp {
        for pp in req_pp {
            if seen_post_processing.insert(pp.as_str()) {
                post_processing_list.push(pp.clone());
            }
        }
    }
    for pp in parsed_pp {
        if seen_post_processing.insert(pp.as_str()) {
            post_processing_list.push(pp.clone());
        }
    }
    (!post_processing_list.is_empty()).then_some(post_processing_list)
}

pub(crate) fn resolve_horde_models(upstream_model_id: &str) -> Vec<String> {
    let list: Vec<String> = upstream_model_id
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if list.is_empty() {
        vec!["AlbedoBase XL (SDXL)".to_string()]
    } else {
        list
    }
}

pub(crate) fn resolve_source_processing(
    is_img2img: bool,
    has_mask: bool,
    explicit_processing: Option<&str>,
) -> Option<String> {
    if is_img2img {
        let default_proc = if has_mask { "inpainting" } else { "img2img" };
        Some(explicit_processing.unwrap_or(default_proc).to_string())
    } else {
        None
    }
}

pub fn build_horde_payload(
    req: &ImageGenerationRequest,
    upstream_model_id: &str,
    source_image_b64: Option<String>,
    source_mask_b64: Option<String>,
    source_processing: Option<&str>,
    denoising_strength: Option<f32>,
) -> Result<Bytes> {
    let (width, height) = parse_dimensions(req.size.as_deref(), req.aspect_ratio.as_deref());
    let parsed = parse_prompt_directives(&req.prompt);
    let prompt = assemble_final_prompt(
        &parsed.clean_prompt,
        req.negative_prompt.as_deref(),
        &parsed.negative_prompts,
    );

    let is_img2img = source_image_b64.is_some();
    let post_processing = assemble_post_processing(
        req.quality.as_deref(),
        req.post_processing.as_deref(),
        &parsed.post_processing,
    );
    let workers = (!parsed.workers.is_empty()).then_some(parsed.workers);
    let models = resolve_horde_models(upstream_model_id);
    let source_processing =
        resolve_source_processing(is_img2img, source_mask_b64.is_some(), source_processing);

    let payload = HordeGenerationPayload {
        prompt,
        params: HordeGenerationParams {
            n: req.n.unwrap_or(1).clamp(1, 10),
            width,
            height,
            steps: parsed.steps.unwrap_or(25),
            sampler_name: parsed
                .sampler_name
                .unwrap_or_else(|| "k_euler_a".to_string()),
            cfg_scale: parsed.cfg_scale.unwrap_or(6.5),
            seed: parsed.seed.or(req.seed),
            denoising_strength: is_img2img
                .then(|| denoising_strength.unwrap_or(0.6).clamp(0.0, 1.0)),
            karras: true,
            hires_fix: parsed.hires_fix.unwrap_or(!is_img2img),
            hires_fix_denoising_strength: parsed.hires_fix_denoising_strength,
            clip_skip: parsed.clip_skip,
            control_type: parsed.control_type,
            post_processing,
            loras: (!parsed.loras.is_empty()).then_some(parsed.loras),
            tis: (!parsed.tis.is_empty()).then_some(parsed.tis),
            workers: workers.clone(),
        },
        models,
        nsfw: true,
        censor_nsfw: false,
        r2: true,
        slow_workers: parsed.slow_workers.unwrap_or(false),
        trusted_workers: parsed.trusted_workers.unwrap_or(true),
        workers,
        source_image: source_image_b64,
        source_mask: source_mask_b64,
        source_processing,
    };

    let vec = serde_json::to_vec(&payload)
        .map_err(|e| CoreError::Parse(format!("failed to serialize horde request: {e}")))?;
    Ok(Bytes::from(vec))
}
