//! Provider adapter for AI Horde (<https://aihorde.net>).
//!
//! AI Horde is a crowdsourced distributed cluster of image generation workers.
//! Image generation is asynchronous: jobs are submitted to `/generate/async`,
//! queued across volunteer GPUs, and polled via `/generate/check/{id}` and
//! `/generate/status/{id}` until finished.

use super::{
    AdapterAuthType, AdapterFormat, Arc, CoreError, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient,
};
use bytes::Bytes;
use openproxy_types::ImageGenerationRequest;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HordeAdapter {
    config: ProviderAdapterConfig,
}

impl HordeAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("horde"),
                name: "AI Horde".into(),
                anonymous_fallback: true,
                rate_limit_scope: "account".into(),
                base_url: "https://aihorde.net/api/v2".into(),
                auth_type: AdapterAuthType::Bearer,
                format: AdapterFormat::Openai,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(HordeAdapter);

#[derive(Debug, Serialize)]
struct HordeGenerationParams {
    n: u32,
    width: u32,
    height: u32,
    steps: u32,
    sampler_name: String,
    cfg_scale: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    seed: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    denoising_strength: Option<f32>,
    karras: bool,
    hires_fix: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    post_processing: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct HordeGenerationPayload {
    prompt: String,
    params: HordeGenerationParams,
    models: Vec<String>,
    nsfw: bool,
    censor_nsfw: bool,
    r2: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_mask: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_processing: Option<String>,
}

impl ProviderAdapter for HordeAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn build_auth_header(&self, api_key: &str) -> Option<(String, String)> {
        let key = if api_key.trim().is_empty() {
            "0000000000"
        } else {
            api_key.trim()
        };
        Some(("Authorization".into(), format!("Bearer {key}")))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let key = if api_key.trim().is_empty() {
            "0000000000"
        } else {
            api_key.trim()
        };
        let mut headers = Vec::with_capacity(4);
        headers.push(("Authorization".into(), format!("Bearer {key}")));
        headers.push(("apikey".into(), key.to_string()));
        headers.push((
            "Client-Agent".into(),
            concat!("openproxy:", env!("CARGO_PKG_VERSION")).into(),
        ));
        headers.push(("Content-Type".into(), "application/json".into()));
        headers
    }

    fn build_chat_url(&self, _target_format: TargetFormat, _model: &ModelId) -> String {
        "https://oai.aihorde.net/v1/chat/completions".to_string()
    }

    fn models_url(&self) -> Option<String> {
        Some("https://oai.aihorde.net/v1/models".to_string())
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let headers = self.build_headers(api_key, TargetFormat::Openai, &ModelId::new(""));
        let header_refs: Vec<(&str, &str)> =
            headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        let mut discovered = Vec::new();

        // 1. Fetch text models from official OpenAI-compatible endpoint
        let oai_models_url = "https://oai.aihorde.net/v1/models";
        if let Ok(json_val) =
            crate::adapters::upstream_get_json(upstream_client, oai_models_url, &header_refs).await
            && let Some(arr) = json_val.get("data").and_then(|v| v.as_array())
        {
            let mut text_models: Vec<(u64, DiscoveredModel)> = arr
                .iter()
                .filter_map(|item| {
                    let id = item.get("id")?.as_str()?.to_string();
                    let clean_name = item
                        .get("clean_name")
                        .and_then(|v| v.as_str())
                        .unwrap_or(&id)
                        .to_string();
                    let workers = item
                        .get("worker_threads")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    if workers < 1 {
                        return None;
                    }
                    let family = openproxy_types::capabilities::infer_family(&id)
                        .or_else(|| Some("instruct".into()));

                    Some((
                        workers,
                        DiscoveredModel {
                            model_id: ModelId::new(id),
                            display_name: Some(format!("{clean_name} ({workers}w)")),
                            target_format: TargetFormat::Openai,
                            context_length: None,
                            max_output_tokens: None,
                            input_modalities: Some(vec!["text".into()]),
                            output_modalities: Some(vec!["text".into()]),
                            model_type: Some("chat".into()),
                            family,
                            capabilities: None,
                        },
                    ))
                })
                .collect();

            text_models.sort_by_key(|b| std::cmp::Reverse(b.0));
            discovered.extend(text_models.into_iter().map(|(_, m)| m));
        }

        // 2. Fetch image generation models from AI Horde API
        let image_url = format!("{}/status/models?type=image", self.config.base_url);
        if let Ok(json_val) =
            crate::adapters::upstream_get_json(upstream_client, &image_url, &header_refs).await
            && let Some(arr) = json_val.as_array()
        {
            let mut image_models: Vec<(u64, u64, DiscoveredModel)> = arr
                .iter()
                .filter_map(|item| {
                    let name = item.get("name")?.as_str()?.to_string();
                    let count = item.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
                    let eta = item.get("eta").and_then(|v| v.as_u64()).unwrap_or(u64::MAX);

                    if count < 1 || eta >= 60 {
                        return None;
                    }

                    let family = infer_horde_family(&name);
                    Some((
                        count,
                        eta,
                        DiscoveredModel {
                            model_id: ModelId::new(name.clone()),
                            display_name: Some(format!("{name} ({count}w, ~{eta}s)")),
                            target_format: TargetFormat::Openai,
                            context_length: None,
                            max_output_tokens: None,
                            input_modalities: Some(vec!["text".into()]),
                            output_modalities: Some(vec!["image".into()]),
                            model_type: Some("image".into()),
                            family: Some(family),
                            capabilities: None,
                        },
                    ))
                })
                .collect();

            image_models.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
            discovered.extend(image_models.into_iter().map(|(_, _, m)| m));
        }

        Ok(discovered)
    }

    fn build_image_url(&self) -> String {
        format!("{}/generate/async", self.config.base_url)
    }

    /// Horde img2img uses the same `/generate/async` endpoint with `source_image`.
    fn build_image_edits_url(&self) -> String {
        self.build_image_url()
    }

    /// Horde variations also use `/generate/async` with `source_image` + `source_processing=img2img`.
    fn build_image_variations_url(&self) -> String {
        self.build_image_url()
    }

    fn format_image_request(
        &self,
        req: &ImageGenerationRequest,
        upstream_model_id: &str,
    ) -> Result<Bytes> {
        self.build_horde_payload(req, upstream_model_id, None, None, None, None)
    }
}

impl HordeAdapter {
    /// Build a Horde `/generate/async` JSON payload.
    ///
    /// When `source_image_b64` is provided, the request becomes img2img (or inpainting if `source_mask_b64` is provided).
    /// `source_processing` defaults to `"inpainting"` if mask is present, otherwise `"img2img"`.
    /// `denoising_strength` is clamped to `0.0..=1.0`, defaulting to 0.6 if img2img and omitted.
    /// If `quality` is `"hd"`, RealESRGAN_x4plus and GFPGAN post-processors are automatically injected.
    pub fn build_horde_payload(
        &self,
        req: &ImageGenerationRequest,
        upstream_model_id: &str,
        source_image_b64: Option<String>,
        source_mask_b64: Option<String>,
        source_processing: Option<&str>,
        denoising_strength: Option<f32>,
    ) -> Result<Bytes> {
        let (width, height) = parse_dimensions(req.size.as_deref(), req.aspect_ratio.as_deref());

        let mut prompt = req.prompt.clone();
        if let Some(ref neg) = req.negative_prompt
            && !neg.trim().is_empty()
        {
            prompt.push_str(" ### ");
            prompt.push_str(neg.trim());
        }

        let is_img2img = source_image_b64.is_some();
        let default_processing = if source_mask_b64.is_some() {
            "inpainting"
        } else if is_img2img {
            "img2img"
        } else {
            "txt2img"
        };

        let post_processing = if req.quality.as_deref() == Some("hd") {
            Some(vec![
                "RealESRGAN_x4plus".to_string(),
                "GFPGAN".to_string(),
            ])
        } else {
            None
        };

        let payload = HordeGenerationPayload {
            prompt,
            params: HordeGenerationParams {
                n: req.n.unwrap_or(1).clamp(1, 10),
                width,
                height,
                steps: 25,
                sampler_name: "k_euler_a".to_string(),
                cfg_scale: 6.5,
                seed: req.seed,
                denoising_strength: if is_img2img {
                    Some(denoising_strength.unwrap_or(0.6).clamp(0.0, 1.0))
                } else {
                    None
                },
                karras: true,
                hires_fix: !is_img2img,
                post_processing,
            },
            models: vec![upstream_model_id.to_string()],
            nsfw: true,
            censor_nsfw: false,
            r2: true,
            source_image: source_image_b64,
            source_mask: source_mask_b64,
            source_processing: if is_img2img {
                Some(source_processing.unwrap_or(default_processing).to_string())
            } else {
                None
            },
        };

        let vec = serde_json::to_vec(&payload)
            .map_err(|e| CoreError::Parse(format!("failed to serialize horde request: {e}")))?;
        Ok(Bytes::from(vec))
    }
}

fn infer_horde_family(model_name: &str) -> String {
    let lower = model_name.to_lowercase();
    if lower.contains("flux") {
        "flux".to_string()
    } else if lower.contains("sdxl") || lower.contains("xl") {
        "sdxl".to_string()
    } else if lower.contains("pony") {
        "pony".to_string()
    } else if lower.contains("stable_diffusion") || lower.contains("sd 1.5") || lower.contains("sd15") {
        "sd15".to_string()
    } else if lower.contains("dreamshaper") {
        "dreamshaper".to_string()
    } else {
        "diffusion".to_string()
    }
}

pub const MIN_HORDE_DIMENSION: u32 = 64;
pub const MAX_HORDE_DIMENSION: u32 = 3072;
pub const DEFAULT_HORDE_DIMENSION: u32 = 1024;

/// Normalize a dimension to the nearest multiple of 64 within AI Horde bounds.
pub fn normalize_dimension_64(val: u32) -> u32 {
    let clamped = val.clamp(MIN_HORDE_DIMENSION, MAX_HORDE_DIMENSION);
    let rem = clamped % 64;
    let rounded = if rem >= 32 {
        clamped.saturating_add(64 - rem)
    } else {
        clamped.saturating_sub(rem)
    };
    rounded.clamp(MIN_HORDE_DIMENSION, MAX_HORDE_DIMENSION)
}

/// Parse dimensions from size string (e.g. "1024x1024") or aspect ratio (e.g. "16:9"),
/// guaranteeing both width and height are strict multiples of 64.
pub fn parse_dimensions(size: Option<&str>, aspect_ratio: Option<&str>) -> (u32, u32) {
    if let Some(size) = size
        && let Some((w_str, h_str)) = size.split_once('x')
        && let (Ok(w), Ok(h)) = (w_str.parse::<u32>(), h_str.parse::<u32>())
    {
        return (normalize_dimension_64(w), normalize_dimension_64(h));
    }

    if let Some(ar) = aspect_ratio {
        let (w, h) = match ar {
            "16:9" => (1024, 576),
            "9:16" => (576, 1024),
            "3:2" => (960, 640),
            "2:3" => (640, 960),
            "4:3" => (1024, 768),
            "3:4" => (768, 1024),
            "21:9" => (1344, 576),
            "9:21" => (576, 1344),
            "1:1" => (1024, 1024),
            _ => (DEFAULT_HORDE_DIMENSION, DEFAULT_HORDE_DIMENSION),
        };
        (normalize_dimension_64(w), normalize_dimension_64(h))
    } else {
        (DEFAULT_HORDE_DIMENSION, DEFAULT_HORDE_DIMENSION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_horde_adapter_config() {
        let a = HordeAdapter::new();
        assert_eq!(a.id().as_str(), "horde");
        assert_eq!(a.config().name, "AI Horde");
        assert_eq!(a.config().base_url, "https://aihorde.net/api/v2");
        assert!(a.config().anonymous_fallback);
        assert_eq!(
            a.build_chat_url(TargetFormat::Openai, &ModelId::new("any")),
            "https://oai.aihorde.net/v1/chat/completions"
        );
        assert_eq!(
            a.models_url().unwrap(),
            "https://oai.aihorde.net/v1/models"
        );
    }

    #[test]
    fn test_horde_auth_header_anonymous() {
        let a = HordeAdapter::new();
        let (name, val) = a.build_auth_header("").unwrap();
        assert_eq!(name, "Authorization");
        assert_eq!(val, "Bearer 0000000000");

        let (name, val) = a.build_auth_header("my-custom-key").unwrap();
        assert_eq!(name, "Authorization");
        assert_eq!(val, "Bearer my-custom-key");
    }

    #[test]
    fn test_normalize_dimension_64() {
        assert_eq!(normalize_dimension_64(0), 64);
        assert_eq!(normalize_dimension_64(30), 64);
        assert_eq!(normalize_dimension_64(64), 64);
        assert_eq!(normalize_dimension_64(65), 64);
        assert_eq!(normalize_dimension_64(95), 64);
        assert_eq!(normalize_dimension_64(96), 128);
        assert_eq!(normalize_dimension_64(500), 512);
        assert_eq!(normalize_dimension_64(700), 704);
        assert_eq!(normalize_dimension_64(1024), 1024);
        assert_eq!(normalize_dimension_64(5000), 3072);
    }

    #[test]
    fn test_parse_dimensions_aspect_ratios() {
        let pairs = [
            ("16:9", (1024, 576)),
            ("9:16", (576, 1024)),
            ("3:2", (960, 640)),
            ("2:3", (640, 960)),
            ("4:3", (1024, 768)),
            ("3:4", (768, 1024)),
            ("21:9", (1344, 576)),
            ("9:21", (576, 1344)),
            ("1:1", (1024, 1024)),
        ];

        for (ar, expected) in pairs {
            let (w, h) = parse_dimensions(None, Some(ar));
            assert_eq!((w, h), expected, "aspect ratio {ar}");
            assert_eq!(w % 64, 0, "width not multiple of 64: {w}");
            assert_eq!(h % 64, 0, "height not multiple of 64: {h}");
        }

        let (w, h) = parse_dimensions(Some("700x700"), None);
        assert_eq!((w, h), (704, 704));
        assert_eq!(w % 64, 0);
        assert_eq!(h % 64, 0);
    }

    #[test]
    fn test_format_image_request_standard() {
        let a = HordeAdapter::new();
        let req = ImageGenerationRequest {
            prompt: "A beautiful sunset".into(),
            model: "SDXL 1.0".into(),
            n: Some(1),
            quality: None,
            response_format: None,
            size: Some("512x512".into()),
            style: None,
            user: None,
            aspect_ratio: None,
            seed: Some(42),
            negative_prompt: Some("blurry, low quality".into()),
        };
        let body = a.format_image_request(&req, "SDXL 1.0").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["prompt"], "A beautiful sunset ### blurry, low quality");
        assert_eq!(v["params"]["width"], 512);
        assert_eq!(v["params"]["height"], 512);
        assert_eq!(v["params"]["seed"], 42);
        assert_eq!(v["models"][0], "SDXL 1.0");
        assert!(v["params"]["karras"].as_bool().unwrap());
        assert!(v["params"]["hires_fix"].as_bool().unwrap());
        assert!(v["params"].get("post_processing").is_none());
        assert!(v.get("source_image").is_none());
        assert!(v.get("source_processing").is_none());
    }

    #[test]
    fn test_format_image_request_hd_quality() {
        let a = HordeAdapter::new();
        let req = ImageGenerationRequest {
            prompt: "Cyberpunk street".into(),
            model: "SDXL 1.0".into(),
            n: Some(1),
            quality: Some("hd".into()),
            response_format: None,
            size: Some("1024x1024".into()),
            style: None,
            user: None,
            aspect_ratio: None,
            seed: None,
            negative_prompt: None,
        };
        let body = a.format_image_request(&req, "SDXL 1.0").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let post = v["params"]["post_processing"].as_array().unwrap();
        assert_eq!(post.len(), 2);
        assert_eq!(post[0], "RealESRGAN_x4plus");
        assert_eq!(post[1], "GFPGAN");
    }

    #[test]
    fn test_format_img2img_request() {
        let a = HordeAdapter::new();
        let req = ImageGenerationRequest {
            prompt: "Add sunglasses".into(),
            model: "SDXL 1.0".into(),
            n: Some(1),
            quality: None,
            response_format: None,
            size: Some("512x512".into()),
            style: None,
            user: None,
            aspect_ratio: None,
            seed: None,
            negative_prompt: None,
        };
        let body = a
            .build_horde_payload(
                &req,
                "SDXL 1.0",
                Some("aGVsbG8=".into()),
                None,
                None,
                Some(0.75),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["source_image"], "aGVsbG8=");
        assert_eq!(v["source_processing"], "img2img");
        assert_eq!(v["params"]["denoising_strength"], 0.75);
        assert!(!v["params"]["hires_fix"].as_bool().unwrap());
    }

    #[test]
    fn test_format_inpainting_request() {
        let a = HordeAdapter::new();
        let req = ImageGenerationRequest {
            prompt: "Replace car with motorcycle".into(),
            model: "SDXL 1.0".into(),
            n: Some(1),
            quality: None,
            response_format: None,
            size: Some("512x512".into()),
            style: None,
            user: None,
            aspect_ratio: None,
            seed: None,
            negative_prompt: None,
        };
        let body = a
            .build_horde_payload(
                &req,
                "SDXL 1.0",
                Some("aW1hZ2U=".into()),
                Some("bWFzaw==".into()),
                None,
                Some(1.5), // should be clamped to 1.0
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["source_image"], "aW1hZ2U=");
        assert_eq!(v["source_mask"], "bWFzaw==");
        assert_eq!(v["source_processing"], "inpainting");
        assert_eq!(v["params"]["denoising_strength"], 1.0);
    }
}

