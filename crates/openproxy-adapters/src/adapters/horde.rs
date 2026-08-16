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
}

#[derive(Debug, Serialize)]
struct HordeGenerationPayload {
    prompt: String,
    params: HordeGenerationParams,
    models: Vec<String>,
    nsfw: bool,
    censor_nsfw: bool,
    r2: bool,
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
        Some(("apikey".into(), key.to_string()))
    }

    fn build_headers(
        &self,
        api_key: &str,
        _target_format: TargetFormat,
        _model: &ModelId,
    ) -> Vec<(String, String)> {
        let mut headers = Vec::with_capacity(3);
        if let Some((name, val)) = self.build_auth_header(api_key) {
            headers.push((name, val));
        }
        headers.push((
            "Client-Agent".into(),
            concat!("openproxy:", env!("CARGO_PKG_VERSION")).into(),
        ));
        headers.push(("Content-Type".into(), "application/json".into()));
        headers
    }

    fn models_url(&self) -> Option<String> {
        Some(format!("{}/status/models?type=image", self.config.base_url))
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let Some(url) = self.models_url() else {
            return Ok(vec![]);
        };

        let headers = self.build_headers(api_key, TargetFormat::Openai, &ModelId::new(""));
        let header_refs: Vec<(&str, &str)> =
            headers.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();

        let json_val = crate::adapters::upstream_get_json(upstream_client, &url, &header_refs)
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("horde /status/models: {e}")))?;

        let arr = json_val
            .as_array()
            .ok_or_else(|| CoreError::Parse("horde /status/models: expected array".into()))?;

        let mut models: Vec<(u64, u64, DiscoveredModel)> = arr
            .iter()
            .filter_map(|item| {
                let name = item.get("name")?.as_str()?.to_string();
                let model_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("image");
                if model_type != "image" {
                    return None;
                }

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

        models.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

        Ok(models.into_iter().map(|(_, _, m)| m).collect())
    }

    fn build_image_url(&self) -> String {
        format!("{}/generate/async", self.config.base_url)
    }

    fn format_image_request(
        &self,
        req: &ImageGenerationRequest,
        upstream_model_id: &str,
    ) -> Result<Bytes> {
        let (width, height) = parse_dimensions(req.size.as_deref(), req.aspect_ratio.as_deref());

        let mut prompt = req.prompt.clone();
        if let Some(ref neg) = req.negative_prompt
            && !neg.trim().is_empty()
        {
            prompt.push_str(" ### ");
            prompt.push_str(neg.trim());
        }

        let payload = HordeGenerationPayload {
            prompt,
            params: HordeGenerationParams {
                n: req.n.unwrap_or(1),
                width,
                height,
                steps: 25,
                sampler_name: "k_euler_a".to_string(),
                cfg_scale: 6.5,
                seed: req.seed,
            },
            models: vec![upstream_model_id.to_string()],
            nsfw: true,
            censor_nsfw: false,
            r2: true,
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

fn parse_dimensions(size: Option<&str>, aspect_ratio: Option<&str>) -> (u32, u32) {
    if let Some(size) = size
        && let Some((w_str, h_str)) = size.split_once('x')
        && let (Ok(w), Ok(h)) = (w_str.parse::<u32>(), h_str.parse::<u32>())
    {
        return (w, h);
    }

    if let Some(ar) = aspect_ratio {
        match ar {
            "16:9" => (1024, 576),
            "9:16" => (576, 1024),
            "3:2" => (1024, 680),
            "2:3" => (680, 1024),
            "4:3" => (1024, 768),
            "3:4" => (768, 1024),
            _ => (1024, 1024),
        }
    } else {
        (1024, 1024)
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
    }

    #[test]
    fn test_horde_auth_header_anonymous() {
        let a = HordeAdapter::new();
        let (name, val) = a.build_auth_header("").unwrap();
        assert_eq!(name, "apikey");
        assert_eq!(val, "0000000000");

        let (name, val) = a.build_auth_header("my-custom-key").unwrap();
        assert_eq!(name, "apikey");
        assert_eq!(val, "my-custom-key");
    }

    #[test]
    fn test_format_image_request() {
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
    }
}
