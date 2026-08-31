//! Provider adapter for AI Horde (<https://aihorde.net>).
//!
//! AI Horde is a crowdsourced distributed cluster of image generation workers.
//! Image generation is asynchronous: jobs are submitted to `/generate/async`,
//! queued across volunteer GPUs, and polled via `/generate/check/{id}` and
//! `/generate/status/{id}` until finished.

use super::{
    AdapterAuthType, AdapterFormat, Arc, CancellationToken, CoreError, DiscoveredModel, ModelId,
    ProviderAdapter, ProviderAdapterConfig, ProviderId, Result, TargetFormat, TimeoutProfile,
    UpstreamClient, UpstreamRequest,
};
use bytes::Bytes;
use openproxy_types::ImageGenerationRequest;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::LazyLock;

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

static HORDE_CLIENT_AGENT: LazyLock<http::HeaderValue> = LazyLock::new(|| {
    http::HeaderValue::from_static(concat!("openproxy:", env!("CARGO_PKG_VERSION")))
});

crate::adapters::derive_default_from_new!(HordeAdapter);
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HordeLora {
    pub name: String,
    pub model: f32,
    pub clip: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inject_trigger: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HordeTi {
    pub name: String,
    pub strength: f32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParsedPromptDirectives {
    pub clean_prompt: String,
    pub negative_prompts: Vec<String>,
    pub loras: Vec<HordeLora>,
    pub tis: Vec<HordeTi>,
    pub sampler_name: Option<String>,
    pub steps: Option<u32>,
    pub cfg_scale: Option<f32>,
    pub clip_skip: Option<u32>,
    pub hires_fix: Option<bool>,
    pub hires_fix_denoising_strength: Option<f32>,
    pub seed: Option<u64>,
    pub control_type: Option<String>,
    pub post_processing: Vec<String>,
    pub slow_workers: Option<bool>,
    pub trusted_workers: Option<bool>,
    pub workers: Vec<String>,
}

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
    hires_fix_denoising_strength: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    clip_skip: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    control_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    post_processing: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    loras: Option<Vec<HordeLora>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tis: Option<Vec<HordeTi>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workers: Option<Vec<String>>,
}

#[derive(Debug, Serialize)]
struct HordeGenerationPayload {
    prompt: String,
    params: HordeGenerationParams,
    models: Vec<String>,
    nsfw: bool,
    censor_nsfw: bool,
    r2: bool,
    slow_workers: bool,
    trusted_workers: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    workers: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_image: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_mask: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_processing: Option<String>,
}

fn map_horde_cluster_model(
    item: &serde_json::Value,
    is_image: bool,
) -> Option<(u64, u64, DiscoveredModel)> {
    let name = item.get("name")?.as_str()?.to_string();
    let count = item.get("count").and_then(|v| v.as_u64()).unwrap_or(0);
    let eta = item.get("eta").and_then(|v| v.as_u64()).unwrap_or(u64::MAX);

    let (family, out_mods, m_type): (_, Box<[String]>, _) = if is_image {
        (
            Some(infer_horde_family(&name)),
            vec!["image".into()].into(),
            "image",
        )
    } else {
        (
            openproxy_types::capabilities::infer_family(&name).or_else(|| Some("instruct".into())),
            vec!["text".into()].into(),
            "chat",
        )
    };

    let display_name = if count > 0 && eta != u64::MAX {
        Some(format!("{name} ({count}w, ~{eta}s)"))
    } else if count > 0 {
        Some(format!("{name} ({count}w)"))
    } else {
        Some(name.clone())
    };

    Some((
        count,
        eta,
        DiscoveredModel {
            model_id: ModelId::new(name),
            display_name,
            target_format: TargetFormat::Openai,
            context_length: None,
            max_output_tokens: None,
            input_modalities: Some(vec!["text".into()].into()),
            output_modalities: Some(out_mods),
            model_type: Some(m_type.into()),
            family,
            capabilities: None,
        },
    ))
}

async fn fetch_horde_cluster_models(
    upstream_client: &Arc<UpstreamClient>,
    base_url: &str,
    header_refs: &[(&str, &str)],
    model_type: &'static str,
) -> Vec<DiscoveredModel> {
    let url = format!("{base_url}/status/models?type={model_type}");
    let Ok(json_val) = crate::adapters::upstream_get_json(upstream_client, &url, header_refs).await
    else {
        return Vec::new();
    };
    let Some(arr) = json_val.as_array() else {
        return Vec::new();
    };
    let is_image = model_type == "image";
    let mut models: Vec<(u64, u64, DiscoveredModel)> = arr
        .iter()
        .filter_map(|item| map_horde_cluster_model(item, is_image))
        .collect();

    models.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    models.into_iter().map(|(_, _, m)| m).collect()
}

impl ProviderAdapter for HordeAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn metadata(&self) -> openproxy_types::ProviderMetadata {
        let mut meta = openproxy_types::ProviderMetadata::custom_default();
        meta.built_in = true;
        meta.deletable = false;
        meta.supports_quota = true;
        meta.quota_refresh_supported = true;
        meta
    }

    fn is_anonymous_fallback(&self) -> bool {
        true
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
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();

        let mut discovered = fetch_horde_cluster_models(
            upstream_client,
            &self.config.base_url,
            &header_refs,
            "text",
        )
        .await;
        discovered.extend(
            fetch_horde_cluster_models(
                upstream_client,
                &self.config.base_url,
                &header_refs,
                "image",
            )
            .await,
        );

        // 3. Synthetic Vision / Interrogation model
        discovered.push(DiscoveredModel {
            model_id: ModelId::new("horde/vision"),
            display_name: Some("Horde Vision (CLIP/BLIP Interrogator)".into()),
            target_format: TargetFormat::Openai,
            context_length: None,
            max_output_tokens: None,
            input_modalities: Some(vec!["text".into(), "image".into()].into()),
            output_modalities: Some(vec!["text".into()].into()),
            model_type: Some("chat".into()),
            family: Some("vision".into()),
            capabilities: None,
        });

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

    async fn fetch_quota(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Option<Result<openproxy_types::AccountQuota>> {
        Some(self.fetch_horde_quota_local(upstream_client, api_key).await)
    }
}

async fn query_horde_user(
    upstream: &Arc<UpstreamClient>,
    base_url: &str,
    key: &str,
) -> std::result::Result<serde_json::Value, String> {
    let url = format!("{base_url}/find_user");
    let mut req = UpstreamRequest::get(url);
    if let Ok(v) = http::HeaderValue::from_str(key) {
        req.headers
            .insert(http::header::HeaderName::from_static("apikey"), v);
    }
    req.headers.insert(
        http::header::HeaderName::from_static("client-agent"),
        HORDE_CLIENT_AGENT.clone(),
    );
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );

    let cancel = CancellationToken::new();
    let response = upstream
        .call(req, TimeoutProfile::Quota, cancel)
        .await
        .map_err(|e| format!("network: {e}"))?;

    if !response.status.is_success() {
        let status = response.status.as_u16();
        let body = response.collect().await.unwrap_or_default();
        let snippet = String::from_utf8_lossy(&body)
            .chars()
            .take(200)
            .collect::<String>();
        return Err(format!("HTTP {status}: {snippet}"));
    }

    let body = response
        .collect()
        .await
        .map_err(|e| format!("collect: {e}"))?;
    serde_json::from_slice(&body).map_err(|e| format!("parse: {e}"))
}

impl HordeAdapter {
    async fn fetch_horde_quota_local(
        &self,
        upstream: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<openproxy_types::AccountQuota> {
        let key = if api_key.trim().is_empty() {
            "0000000000"
        } else {
            api_key.trim()
        };

        match query_horde_user(upstream, &self.config.base_url, key).await {
            Ok(json) => Ok(parse_horde_quota(
                &json,
                &openproxy_types::quota::now_unix_secs_str(),
            )),
            Err(err) => Ok(openproxy_types::AccountQuota {
                session_used: None,
                session_limit: None,
                session_reset_at: None,
                weekly_used: None,
                weekly_limit: None,
                weekly_reset_at: None,
                plan_name: None,
                last_fetched_at: openproxy_types::quota::now_unix_secs_str(),
                fetch_error: Some(err),
                model_details: None,
            }),
        }
    }

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

    /// Check if a model name refers to the Horde vision/interrogation synthetic model.
    pub fn is_vision_model(model_name: &str) -> bool {
        let lower = model_name.to_lowercase();
        lower == "horde/vision" || lower == "vision" || lower.ends_with("/vision")
    }

    /// Build a Horde `/interrogate/async` JSON payload.
    pub fn build_interrogate_payload(source_image: &str, forms: &[&str]) -> Result<Bytes> {
        let forms_vec: Vec<HordeInterrogateForm> = if forms.is_empty() {
            vec![HordeInterrogateForm {
                name: "caption".to_string(),
            }]
        } else {
            forms
                .iter()
                .map(|f| HordeInterrogateForm {
                    name: f.to_string(),
                })
                .collect()
        };

        let payload = HordeInterrogatePayload {
            forms: forms_vec,
            source_image: clean_image_str(source_image),
        };

        let vec = serde_json::to_vec(&payload).map_err(|e| {
            CoreError::Parse(format!(
                "failed to serialize horde interrogate request: {e}"
            ))
        })?;
        Ok(Bytes::from(vec))
    }

    /// Extract image base64 or URL from chat messages.
    pub fn extract_image_from_messages(
        messages: &[openproxy_types::OpenAIMessage],
    ) -> Option<String> {
        messages
            .iter()
            .rev()
            .filter_map(|msg| msg.content.as_ref())
            .find_map(extract_image_from_content)
    }

    fn extract_caption_from_object(
        obj: &serde_json::Map<String, serde_json::Value>,
    ) -> Option<String> {
        for key in [
            "caption",
            "text",
            "interrogation",
            "description",
            "summary",
            "result",
        ] {
            if let Some(s) = obj.get(key).and_then(|v| v.as_str())
                && !s.trim().is_empty()
            {
                return Some(s.trim().to_string());
            }
        }
        serde_json::to_string(obj).ok()
    }

    fn parse_form_result_caption(forms: &[serde_json::Value]) -> Option<String> {
        for form in forms {
            let Some(result) = form.get("result") else {
                continue;
            };
            if let Some(s) = result.as_str()
                && !s.trim().is_empty()
            {
                return Some(s.trim().to_string());
            }
            if let Some(obj) = result.as_object()
                && let Some(cap) = Self::extract_caption_from_object(obj)
            {
                return Some(cap);
            }
        }
        None
    }

    fn parse_generations_caption(gens: &[serde_json::Value]) -> Option<String> {
        for item in gens {
            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                return Some(text.trim().to_string());
            }
            if let Some(img) = item.get("img").and_then(|v| v.as_str()) {
                return Some(img.trim().to_string());
            }
        }
        None
    }

    fn check_interrogate_forms_caption(status_json: &serde_json::Value) -> Option<String> {
        let forms = status_json.get("forms")?.as_array()?;
        Self::parse_form_result_caption(forms)
    }

    fn check_interrogate_result_caption(status_json: &serde_json::Value) -> Option<String> {
        let result = status_json.get("result")?;
        if let Some(s) = result.as_str() {
            return Some(s.trim().to_string());
        }
        let caption = result.get("caption")?.as_str()?;
        Some(caption.trim().to_string())
    }

    /// Parse caption string from a Horde interrogate status response.
    pub fn parse_interrogate_status_caption(status_json: &serde_json::Value) -> Option<String> {
        if let Some(cap) = Self::check_interrogate_forms_caption(status_json) {
            return Some(cap);
        }

        if let Some(caption) = status_json.get("caption").and_then(|v| v.as_str()) {
            return Some(caption.trim().to_string());
        }

        if let Some(cap) = Self::check_interrogate_result_caption(status_json) {
            return Some(cap);
        }

        if let Some(gens) = status_json.get("generations").and_then(|v| v.as_array()) {
            return Self::parse_generations_caption(gens);
        }

        None
    }

    fn any_form_done(status_json: &serde_json::Value) -> bool {
        let Some(arr) = status_json.get("forms").and_then(|v| v.as_array()) else {
            return false;
        };
        arr.iter().any(|f| {
            f.get("state")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("done"))
                || f.get("result").is_some()
        })
    }

    /// Check if interrogation status is done or faulted.
    pub fn is_interrogate_done(status_json: &serde_json::Value) -> (bool, bool) {
        let state = status_json
            .get("state")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let is_done = state.eq_ignore_ascii_case("done")
            || status_json
                .get("done")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            || Self::any_form_done(status_json);
        let is_faulted = state.eq_ignore_ascii_case("faulted")
            || status_json
                .get("faulted")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
        (is_done, is_faulted)
    }

    /// Submit an asynchronous interrogation job to AI Horde and return its job ID.
    pub async fn submit_interrogate_job(
        upstream_client: &Arc<UpstreamClient>,
        base_url: &str,
        api_key: &str,
        source_image: &str,
        cancel_token: CancellationToken,
    ) -> Result<String> {
        let payload = Self::build_interrogate_payload(source_image, &["caption"])?;
        let async_url = format!("{base_url}/interrogate/async");
        let mut post_req = UpstreamRequest::post_json(async_url, payload);
        apply_horde_auth_headers(&mut post_req, api_key, true);

        let resp = upstream_client
            .call(post_req, TimeoutProfile::Chat, cancel_token)
            .await
            .map_err(|e| {
                CoreError::UpstreamConnection(format!("horde interrogate submit error: {e:?}"))
            })?;

        if !resp.status.is_success() {
            let status = resp.status.as_u16();
            let body = resp.collect().await.unwrap_or_default();
            let snippet = String::from_utf8_lossy(&body);
            return Err(CoreError::UpstreamConnection(format!(
                "horde interrogate submit failed (HTTP {status}): {snippet}"
            )));
        }

        let body = resp
            .collect()
            .await
            .map_err(|e| CoreError::UpstreamConnection(format!("read submit response: {e:?}")))?;

        let submit_json: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| CoreError::Parse(format!("parse horde submit response: {e}")))?;

        let Some(job_id) = submit_json.get("id").and_then(|v| v.as_str()) else {
            return Err(CoreError::UpstreamConnection(format!(
                "horde interrogate did not return a job ID: {submit_json}"
            )));
        };
        Ok(job_id.to_string())
    }

    async fn poll_interrogate_step(
        upstream_client: &Arc<UpstreamClient>,
        status_url: &str,
        api_key: &str,
        cancel_token: &CancellationToken,
    ) -> Option<Result<String>> {
        let mut req = UpstreamRequest::get(status_url);
        apply_horde_auth_headers(&mut req, api_key, false);

        let resp = match upstream_client
            .call(req, TimeoutProfile::Chat, cancel_token.clone())
            .await
        {
            Ok(r) if r.status.as_u16() == 200 => r,
            Ok(_) => return None,
            Err(e) => {
                tracing::warn!("Horde interrogate status polling network error: {e:?}");
                return None;
            }
        };

        let body = resp.collect().await.ok()?;
        let status_json = serde_json::from_slice::<serde_json::Value>(&body).ok()?;
        Self::evaluate_interrogate_status(&status_json)
    }

    fn evaluate_interrogate_status(status_json: &serde_json::Value) -> Option<Result<String>> {
        let (is_done, is_faulted) = Self::is_interrogate_done(status_json);
        if is_faulted {
            return Some(Err(CoreError::UpstreamConnection(
                "horde interrogation job faulted or worker unavailable".into(),
            )));
        }
        if (is_done || status_json.get("forms").is_some())
            && let Some(caption) = Self::parse_interrogate_status_caption(status_json)
        {
            return Some(Ok(caption));
        }
        None
    }

    /// Poll an asynchronous interrogation job on AI Horde until completion or timeout.
    pub async fn poll_interrogate_job(
        upstream_client: &Arc<UpstreamClient>,
        base_url: &str,
        api_key: &str,
        job_id: &str,
        cancel_token: CancellationToken,
    ) -> Result<String> {
        let status_url = format!("{base_url}/interrogate/status/{job_id}");
        let timeout = std::time::Duration::from_secs(120);
        let start = std::time::Instant::now();

        while start.elapsed() < timeout {
            if cancel_token.is_cancelled() {
                cancel_interrogate_job(upstream_client, base_url, job_id, api_key).await;
                return Err(CoreError::Cancelled(
                    openproxy_types::CancelReason::ClientDisconnected,
                ));
            }

            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;

            if let Some(res) =
                Self::poll_interrogate_step(upstream_client, &status_url, api_key, &cancel_token)
                    .await
            {
                if res.is_err() {
                    cancel_interrogate_job(upstream_client, base_url, job_id, api_key).await;
                }
                return res;
            }
        }

        cancel_interrogate_job(upstream_client, base_url, job_id, api_key).await;
        Err(CoreError::UpstreamTimeout {
            phase: "horde_interrogate_poll".into(),
            ms: 120_000,
        })
    }

    /// Execute an asynchronous interrogation on AI Horde and poll until done.
    pub async fn execute_interrogate(
        upstream_client: &Arc<UpstreamClient>,
        base_url: &str,
        api_key: &str,
        source_image: &str,
        cancel_token: CancellationToken,
    ) -> Result<String> {
        let job_id = Self::submit_interrogate_job(
            upstream_client,
            base_url,
            api_key,
            source_image,
            cancel_token.clone(),
        )
        .await?;
        Self::poll_interrogate_job(upstream_client, base_url, api_key, &job_id, cancel_token).await
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HordeInterrogateForm {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HordeInterrogatePayload {
    pub forms: Vec<HordeInterrogateForm>,
    pub source_image: String,
}

#[derive(Debug, Deserialize)]
pub struct HordeInterrogateStatusResponse {
    pub id: Option<String>,
    pub state: Option<String>,
    pub forms: Option<Vec<HordeInterrogateFormStatus>>,
    pub message: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HordeInterrogateFormStatus {
    pub name: Option<String>,
    pub form: Option<String>,
    pub state: Option<String>,
    pub result: Option<serde_json::Value>,
}

fn apply_horde_auth_headers(req: &mut UpstreamRequest, api_key: &str, include_bearer: bool) {
    let key = if api_key.trim().is_empty() {
        "0000000000"
    } else {
        api_key.trim()
    };
    if let Ok(val) = http::HeaderValue::from_str(key) {
        req.headers
            .insert(http::header::HeaderName::from_static("apikey"), val);
    }
    req.headers.insert(
        http::header::HeaderName::from_static("client-agent"),
        HORDE_CLIENT_AGENT.clone(),
    );
    if include_bearer {
        let mut bytes = bytes::BytesMut::with_capacity(7 + key.len());
        bytes.extend_from_slice(b"Bearer ");
        bytes.extend_from_slice(key.as_bytes());
        if let Ok(val) = http::HeaderValue::from_maybe_shared(bytes.freeze()) {
            req.headers.insert(http::header::AUTHORIZATION, val);
        }
    }
}

async fn cancel_interrogate_job(
    upstream_client: &Arc<UpstreamClient>,
    base_url: &str,
    job_id: &str,
    api_key: &str,
) {
    let cancel_url = format!("{base_url}/interrogate/status/{job_id}");
    let mut del_req = UpstreamRequest::delete(&cancel_url);
    apply_horde_auth_headers(&mut del_req, api_key, false);
    let _ = upstream_client
        .call(del_req, TimeoutProfile::Chat, CancellationToken::new())
        .await;
}

fn is_image_url_or_data(s: &str) -> bool {
    s.starts_with("data:image/") || s.starts_with("http://") || s.starts_with("https://")
}

fn extract_image_from_content(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::Array(parts) => parts.iter().find_map(extract_image_from_part),
        serde_json::Value::Object(map) => extract_image_from_json_map(map),
        serde_json::Value::String(s) if is_image_url_or_data(s) => Some(clean_image_str(s)),
        _ => None,
    }
}

fn extract_image_from_part(part: &serde_json::Value) -> Option<String> {
    let obj = part.as_object()?;
    extract_image_from_json_map(obj)
}

fn extract_image_from_json_map(map: &serde_json::Map<String, serde_json::Value>) -> Option<String> {
    // 1. type == "image_url" -> image_url.url or image_url string
    if let Some(img_url_val) = map.get("image_url") {
        if let Some(url_str) = img_url_val.as_str() {
            return Some(clean_image_str(url_str));
        }
        if let Some(url_obj) = img_url_val.as_object()
            && let Some(url_str) = url_obj.get("url").and_then(|v| v.as_str())
        {
            return Some(clean_image_str(url_str));
        }
    }

    // 2. source -> data (Anthropic style)
    if let Some(source) = map.get("source").and_then(|v| v.as_object())
        && let Some(data) = source.get("data").and_then(|v| v.as_str())
    {
        return Some(clean_image_str(data));
    }

    // 3. input_image / image -> image string / data
    if let Some(img) = map.get("image").and_then(|v| v.as_str()) {
        return Some(clean_image_str(img));
    }
    if let Some(img) = map.get("source_image").and_then(|v| v.as_str()) {
        return Some(clean_image_str(img));
    }

    None
}

fn clean_image_str(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.starts_with("data:image/")
        && let Some((_header, data)) = trimmed.split_once(',')
    {
        data.trim().to_string()
    } else {
        trimmed.to_string()
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
    } else if lower.contains("stable_diffusion")
        || lower.contains("sd 1.5")
        || lower.contains("sd15")
    {
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

/// Aspect ratio to pixel dimensions lookup.
pub fn aspect_ratio_to_dimensions(ar: &str) -> (u32, u32) {
    match ar {
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
    }
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
        let (w, h) = aspect_ratio_to_dimensions(ar);
        (normalize_dimension_64(w), normalize_dimension_64(h))
    } else {
        (DEFAULT_HORDE_DIMENSION, DEFAULT_HORDE_DIMENSION)
    }
}

fn parse_lora_tag(tag_trimmed: &str) -> Option<HordeLora> {
    if !tag_trimmed
        .get(..5)
        .is_some_and(|p| p.eq_ignore_ascii_case("lora:"))
    {
        return None;
    }
    if !tag_trimmed.is_char_boundary(5) {
        return None;
    }
    let lora_body = &tag_trimmed[5..];
    let parts: Vec<&str> = lora_body.split(':').collect();
    if parts.is_empty() {
        return None;
    }
    let name = parts[0].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let (model, clip) = match parts.len() {
        1 => (1.0, 1.0),
        2 => {
            let w = parts[1].trim().parse::<f32>().unwrap_or(1.0);
            (w, w)
        }
        _ => {
            let m = parts[1].trim().parse::<f32>().unwrap_or(1.0);
            let c = parts[2].trim().parse::<f32>().unwrap_or(m);
            (m, c)
        }
    };
    Some(HordeLora {
        name,
        model,
        clip,
        inject_trigger: None,
    })
}

fn parse_ti_tag(tag_trimmed: &str) -> Option<HordeTi> {
    let is_ti = tag_trimmed
        .get(..3)
        .is_some_and(|p| p.eq_ignore_ascii_case("ti:"));
    let is_emb = tag_trimmed
        .get(..4)
        .is_some_and(|p| p.eq_ignore_ascii_case("emb:"));
    if !is_ti && !is_emb {
        return None;
    }
    let ti_body = if is_ti {
        if !tag_trimmed.is_char_boundary(3) {
            return None;
        }
        &tag_trimmed[3..]
    } else {
        if !tag_trimmed.is_char_boundary(4) {
            return None;
        }
        &tag_trimmed[4..]
    };
    let parts: Vec<&str> = ti_body.split(':').collect();
    if parts.is_empty() {
        return None;
    }
    let name = parts[0].trim().to_string();
    if name.is_empty() {
        return None;
    }
    let strength = if parts.len() >= 2 {
        parts[1].trim().parse::<f32>().unwrap_or(1.0)
    } else {
        1.0
    };
    Some(HordeTi { name, strength })
}

fn assemble_final_prompt(
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

fn assemble_post_processing(
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

fn resolve_horde_models(upstream_model_id: &str) -> Vec<String> {
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

fn resolve_source_processing(
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

fn extract_lora_and_ti_tags(raw_prompt: &str) -> (String, Vec<HordeLora>, Vec<HordeTi>) {
    let mut loras = Vec::new();
    let mut tis = Vec::new();
    let mut without_tags = String::with_capacity(raw_prompt.len());

    let mut cursor = 0;
    while cursor < raw_prompt.len() && raw_prompt.is_char_boundary(cursor) {
        let Some(start_idx) = raw_prompt[cursor..].find('<') else {
            without_tags.push_str(&raw_prompt[cursor..]);
            break;
        };

        let absolute_start = cursor + start_idx;
        if !raw_prompt.is_char_boundary(absolute_start) {
            without_tags.push_str(&raw_prompt[cursor..]);
            break;
        }

        without_tags.push_str(&raw_prompt[cursor..absolute_start]);

        let Some(end_idx) = raw_prompt[absolute_start..].find('>') else {
            without_tags.push_str(&raw_prompt[absolute_start..]);
            break;
        };

        let absolute_end = absolute_start + end_idx;
        if !raw_prompt.is_char_boundary(absolute_start + 1)
            || !raw_prompt.is_char_boundary(absolute_end)
            || !raw_prompt.is_char_boundary(absolute_end + 1)
        {
            without_tags.push_str(&raw_prompt[absolute_start..]);
            break;
        }

        let tag_content = &raw_prompt[absolute_start + 1..absolute_end];
        let tag_trimmed = tag_content.trim();

        if let Some(lora) = parse_lora_tag(tag_trimmed) {
            loras.push(lora);
        } else if let Some(ti) = parse_ti_tag(tag_trimmed) {
            tis.push(ti);
        } else {
            without_tags.push_str(&raw_prompt[absolute_start..=absolute_end]);
        }

        cursor = absolute_end + 1;
    }

    (without_tags, loras, tis)
}

fn split_prompt_negative_suffix(without_tags: &str) -> (&str, Vec<String>) {
    let (pos_part, neg_part) = without_tags
        .split_once(" ### ")
        .or_else(|| without_tags.split_once("###"))
        .map_or((without_tags, None), |(p, n)| (p, Some(n)));

    let mut negative_prompts = Vec::new();
    if let Some(n) = neg_part {
        let cleaned = clean_residual_prompt(n);
        if !cleaned.is_empty() {
            negative_prompts.push(cleaned);
        }
    }
    (pos_part, negative_prompts)
}

fn parse_hires_flag(words: &[&str], i: &mut usize) -> bool {
    *i += 1;
    if *i < words.len() && !words[*i].starts_with("--") {
        let val = words[*i];
        if val.eq_ignore_ascii_case("false") || val == "0" || val.eq_ignore_ascii_case("off") {
            *i += 1;
            false
        } else if val.eq_ignore_ascii_case("true") || val == "1" || val.eq_ignore_ascii_case("on") {
            *i += 1;
            true
        } else {
            true
        }
    } else {
        true
    }
}

fn next_arg<'a>(words: &[&'a str], i: &mut usize) -> Option<&'a str> {
    *i += 1;
    if *i < words.len() && !words[*i].starts_with("--") {
        let arg = words[*i];
        *i += 1;
        Some(arg)
    } else {
        None
    }
}

fn parse_string_flag(words: &[&str], i: &mut usize) -> Option<String> {
    next_arg(words, i).map(|s| s.trim_matches(|c| c == '"' || c == '\'').to_string())
}

fn parse_numeric_flag<T: std::str::FromStr>(
    words: &[&str],
    i: &mut usize,
    clamp_fn: impl Fn(T) -> T,
) -> Option<T> {
    next_arg(words, i)
        .and_then(|s| s.parse::<T>().ok())
        .map(clamp_fn)
}

fn parse_post_processing_tokens(words: &[&str], i: &mut usize, post_processing: &mut Vec<String>) {
    if let Some(raw) = parse_string_flag(words, i) {
        for part in raw.split(',') {
            let p = part.trim().to_string();
            if !p.is_empty() && !post_processing.contains(&p) {
                post_processing.push(p);
            }
        }
    }
}

fn parse_worker_token(words: &[&str], i: &mut usize, workers: &mut Vec<String>) {
    if let Some(w) = parse_string_flag(words, i)
        && !workers.contains(&w)
    {
        workers.push(w);
    }
}

fn parse_negative_tokens(words: &[&str], i: &mut usize, negative_prompts: &mut Vec<String>) {
    *i += 1;
    let mut neg_tokens = Vec::new();
    while *i < words.len() && !words[*i].starts_with("--") {
        neg_tokens.push(words[*i]);
        *i += 1;
    }
    if !neg_tokens.is_empty() {
        let neg_str = neg_tokens.join(" ");
        let cleaned_neg = clean_residual_prompt(neg_str.trim_matches(|c| c == '"' || c == '\''));
        if !cleaned_neg.is_empty() {
            negative_prompts.push(cleaned_neg);
        }
    }
}

fn apply_directive(
    lower: &str,
    words: &[&str],
    i: &mut usize,
    parsed: &mut ParsedPromptDirectives,
) -> bool {
    match lower {
        "--hires" | "--hires_fix" => {
            parsed.hires_fix = Some(parse_hires_flag(words, i));
            true
        }
        "--no-hires" | "--no_hires" | "--nohires" => {
            parsed.hires_fix = Some(false);
            *i += 1;
            true
        }
        "--allow_slow" => {
            parsed.slow_workers = Some(true);
            *i += 1;
            true
        }
        "--any_worker" => {
            parsed.trusted_workers = Some(false);
            *i += 1;
            true
        }
        "--sampler" | "--sampler_name" => {
            parsed.sampler_name = parse_string_flag(words, i);
            true
        }
        "--steps" => {
            parsed.steps = parse_numeric_flag(words, i, |v: u32| v.clamp(10, 100));
            true
        }
        "--cfg" | "--cfg_scale" => {
            parsed.cfg_scale = parse_numeric_flag(words, i, |v: f32| v.clamp(1.0, 30.0));
            true
        }
        "--clip_skip" => {
            parsed.clip_skip = parse_numeric_flag(words, i, |v: u32| v.clamp(1, 12));
            true
        }
        "--hires_denoising" => {
            parsed.hires_fix_denoising_strength =
                parse_numeric_flag(words, i, |v: f32| v.clamp(0.0, 1.0));
            true
        }
        "--seed" => {
            parsed.seed = parse_numeric_flag(words, i, |v: u64| v);
            true
        }
        "--control" | "--control_type" => {
            parsed.control_type = parse_string_flag(words, i);
            true
        }
        "--post" | "--upscale" | "--post_processing" => {
            parse_post_processing_tokens(words, i, &mut parsed.post_processing);
            true
        }
        "--worker" => {
            parse_worker_token(words, i, &mut parsed.workers);
            true
        }
        "--no" | "--neg" | "--negative_prompt" => {
            parse_negative_tokens(words, i, &mut parsed.negative_prompts);
            true
        }
        _ => false,
    }
}

/// Extract AI Horde prompt directives (<lora:...>, <ti:...>, --sampler, --steps, etc.) from a prompt.
pub fn parse_prompt_directives(raw_prompt: &str) -> ParsedPromptDirectives {
    let (without_tags, loras, tis) = extract_lora_and_ti_tags(raw_prompt);
    let (pos_part, negative_prompts) = split_prompt_negative_suffix(&without_tags);

    let words: Vec<&str> = pos_part.split_whitespace().collect();
    let mut clean_words = Vec::new();
    let mut parsed = ParsedPromptDirectives {
        negative_prompts,
        loras,
        tis,
        ..Default::default()
    };

    let mut i = 0;
    while i < words.len() {
        let word = words[i];
        let lower = word.to_ascii_lowercase();
        if !apply_directive(&lower, &words, &mut i, &mut parsed) {
            clean_words.push(word);
            i += 1;
        }
    }

    parsed.clean_prompt = clean_residual_prompt(&clean_words.join(" "));
    parsed
}

/// Clean up stray punctuation, multiple spaces, and dangling commas left after extracting directives.
pub fn clean_residual_prompt(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut pending_comma = false;
    let mut pending_space = false;

    for c in s.chars() {
        if c == ',' {
            pending_comma = true;
            pending_space = false;
        } else if c.is_whitespace() {
            pending_space = true;
        } else {
            if !result.is_empty() {
                if pending_comma {
                    result.push(',');
                    if pending_space {
                        result.push(' ');
                    }
                } else if pending_space {
                    result.push(' ');
                }
            }
            result.push(c);
            pending_comma = false;
            pending_space = false;
        }
    }

    result
}

#[allow(clippy::collapsible_if)]
fn check_horde_quota_error(body: &serde_json::Value) -> Option<&str> {
    if body.get("kudos").is_none() && body.get("username").is_none() {
        if let Some(msg) = body.get("message").and_then(|v| v.as_str()) {
            return Some(msg);
        }
    }
    None
}

pub fn parse_horde_quota(
    body: &serde_json::Value,
    last_fetched_at: &str,
) -> openproxy_types::AccountQuota {
    if let Some(msg) = check_horde_quota_error(body) {
        return openproxy_types::AccountQuota {
            session_used: None,
            session_limit: None,
            session_reset_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: None,
            last_fetched_at: last_fetched_at.to_string(),
            fetch_error: Some(msg.to_string()),
            model_details: None,
        };
    }

    let username = body
        .get("username")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Anonymous");

    let kudos = body.get("kudos").and_then(|v| v.as_f64()).unwrap_or(0.0);

    let worker_count = body
        .get("worker_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let tokens_used = body
        .get("records")
        .and_then(|r| r.get("usage"))
        .and_then(|u| u.get("tokens"))
        .and_then(|t| t.as_i64())
        .or_else(|| {
            body.get("usage")
                .and_then(|u| u.get("tokens"))
                .and_then(|t| t.as_i64())
        })
        .unwrap_or(0);

    let plan_name = format!("{username} (Kudos: {kudos:.0}, Workers: {worker_count})");
    let session_limit = kudos.max(0.0) as i64;

    openproxy_types::AccountQuota {
        session_used: Some(tokens_used),
        session_limit: Some(session_limit),
        session_reset_at: None,
        weekly_used: None,
        weekly_limit: None,
        weekly_reset_at: None,
        plan_name: Some(plan_name),
        last_fetched_at: last_fetched_at.to_string(),
        fetch_error: None,
        model_details: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_horde_metadata() {
        let a = HordeAdapter::new();
        let meta = a.metadata();
        assert!(meta.supports_quota);
        assert!(meta.quota_refresh_supported);
        assert!(meta.built_in);
        assert!(!meta.deletable);
    }

    #[test]
    fn test_parse_horde_quota_authenticated() {
        let json = serde_json::json!({
            "username": "AIArtist",
            "kudos": 1520.4,
            "worker_count": 3,
            "records": {
                "usage": {
                    "tokens": 4200
                }
            }
        });
        let quota = parse_horde_quota(&json, "1700000000");
        assert_eq!(quota.session_limit, Some(1520));
        assert_eq!(quota.session_used, Some(4200));
        assert_eq!(
            quota.plan_name,
            Some("AIArtist (Kudos: 1520, Workers: 3)".to_string())
        );
        assert_eq!(quota.last_fetched_at, "1700000000");
        assert!(quota.fetch_error.is_none());
    }

    #[test]
    fn test_parse_horde_quota_anonymous_or_default() {
        let json = serde_json::json!({
            "username": "",
            "kudos": 0.0,
            "worker_count": 0
        });
        let quota = parse_horde_quota(&json, "1700000000");
        assert_eq!(quota.session_limit, Some(0));
        assert_eq!(quota.session_used, Some(0));
        assert_eq!(
            quota.plan_name,
            Some("Anonymous (Kudos: 0, Workers: 0)".to_string())
        );
        assert_eq!(quota.last_fetched_at, "1700000000");
        assert!(quota.fetch_error.is_none());
    }

    #[test]
    fn test_parse_horde_quota_error_response() {
        let json = serde_json::json!({
            "message": "Invalid API Key"
        });
        let quota = parse_horde_quota(&json, "1700000000");
        assert_eq!(quota.fetch_error, Some("Invalid API Key".to_string()));
        assert!(quota.session_limit.is_none());
        assert!(quota.session_used.is_none());
        assert!(quota.plan_name.is_none());
        assert_eq!(quota.last_fetched_at, "1700000000");
    }

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
        assert_eq!(a.models_url().unwrap(), "https://oai.aihorde.net/v1/models");
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
            post_processing: None,
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
            post_processing: None,
        };
        let body = a.format_image_request(&req, "SDXL 1.0").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let post = v["params"]["post_processing"].as_array().unwrap();
        assert_eq!(post.len(), 2);
        assert_eq!(post[0], "RealESRGAN_x4plus");
        assert_eq!(post[1], "GFPGAN");
    }

    #[test]
    fn test_format_image_request_cumulative_post_processing() {
        let a = HordeAdapter::new();
        let req = ImageGenerationRequest {
            prompt: "A portrait --post NMKD_Siax,GFPGAN".into(),
            model: "SDXL 1.0".into(),
            n: Some(1),
            quality: None,
            response_format: None,
            size: Some("1024x1024".into()),
            style: None,
            user: None,
            aspect_ratio: None,
            seed: None,
            negative_prompt: None,
            post_processing: Some(vec!["RealESRGAN_x4plus".into(), "CodeFormers".into()].into()),
        };
        let body = a.format_image_request(&req, "SDXL 1.0").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let post = v["params"]["post_processing"].as_array().unwrap();
        assert_eq!(post.len(), 4);
        assert_eq!(post[0], "RealESRGAN_x4plus");
        assert_eq!(post[1], "CodeFormers");
        assert_eq!(post[2], "NMKD_Siax");
        assert_eq!(post[3], "GFPGAN");
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
            post_processing: None,
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
            post_processing: None,
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

    #[test]
    fn test_is_vision_model() {
        assert!(HordeAdapter::is_vision_model("horde/vision"));
        assert!(HordeAdapter::is_vision_model("vision"));
        assert!(HordeAdapter::is_vision_model("HORDE/VISION"));
        assert!(HordeAdapter::is_vision_model("custom/vision"));
        assert!(!HordeAdapter::is_vision_model("horde/sdxl"));
        assert!(!HordeAdapter::is_vision_model("gpt-4o"));
    }

    #[test]
    fn test_build_interrogate_payload() {
        let bytes =
            HordeAdapter::build_interrogate_payload("data:image/png;base64,iVBORw0KGgo=", &[])
                .unwrap();
        let val: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(val["source_image"], "iVBORw0KGgo=");
        assert_eq!(val["forms"][0]["name"], "caption");

        let bytes_custom = HordeAdapter::build_interrogate_payload(
            "https://example.com/cat.jpg",
            &["caption", "nsfw"],
        )
        .unwrap();
        let val_custom: serde_json::Value = serde_json::from_slice(&bytes_custom).unwrap();
        assert_eq!(val_custom["source_image"], "https://example.com/cat.jpg");
        assert_eq!(val_custom["forms"].as_array().unwrap().len(), 2);
        assert_eq!(val_custom["forms"][0]["name"], "caption");
        assert_eq!(val_custom["forms"][1]["name"], "nsfw");
    }

    #[test]
    fn test_extract_image_from_messages() {
        use openproxy_types::OpenAIMessage;

        // 1. OpenAI format with data URI in image_url object
        let msg1 = OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::json!([
                {"type": "text", "text": "What is in this picture?"},
                {"type": "image_url", "image_url": {"url": "data:image/jpeg;base64,dGVzdGltYWdl"}}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        };
        assert_eq!(
            HordeAdapter::extract_image_from_messages(&[msg1]),
            Some("dGVzdGltYWdl".into())
        );

        // 2. OpenAI format with plain URL string
        let msg2 = OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::json!([
                {"type": "image_url", "image_url": "https://example.com/dog.png"}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        };
        assert_eq!(
            HordeAdapter::extract_image_from_messages(&[msg2]),
            Some("https://example.com/dog.png".into())
        );

        // 3. Anthropic format with source object
        let msg3 = OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::json!([
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "YW50aHJvcGlj"}}
            ])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        };
        assert_eq!(
            HordeAdapter::extract_image_from_messages(&[msg3]),
            Some("YW50aHJvcGlj".into())
        );

        // 4. Plain string with data URI
        let msg4 = OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::Value::String(
                "data:image/png;base64,c3RyaW5nZGF0YQ==".into(),
            )),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        };
        assert_eq!(
            HordeAdapter::extract_image_from_messages(&[msg4]),
            Some("c3RyaW5nZGF0YQ==".into())
        );

        // 5. No image
        let msg5 = OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::Value::String("Just plain text".into())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        };
        assert_eq!(HordeAdapter::extract_image_from_messages(&[msg5]), None);
    }

    #[test]
    fn test_parse_interrogate_status_caption() {
        // Forms format with caption in result object
        let status_json1 = serde_json::json!({
            "id": "1234-abcd",
            "state": "done",
            "forms": [
                {
                    "name": "caption",
                    "state": "done",
                    "result": {
                        "caption": "a cute fluffy orange kitten playing with yarn"
                    }
                }
            ]
        });
        assert_eq!(
            HordeAdapter::parse_interrogate_status_caption(&status_json1),
            Some("a cute fluffy orange kitten playing with yarn".into())
        );

        // Forms format with direct string result
        let status_json2 = serde_json::json!({
            "id": "1234-abcd",
            "state": "done",
            "forms": [
                {
                    "name": "caption",
                    "state": "done",
                    "result": "a beautiful landscape with mountains"
                }
            ]
        });
        assert_eq!(
            HordeAdapter::parse_interrogate_status_caption(&status_json2),
            Some("a beautiful landscape with mountains".into())
        );

        // Top level caption / result fallback
        let status_json3 = serde_json::json!({
            "id": "1234-abcd",
            "state": "done",
            "caption": "a sunset on the beach"
        });
        assert_eq!(
            HordeAdapter::parse_interrogate_status_caption(&status_json3),
            Some("a sunset on the beach".into())
        );
    }

    #[test]
    fn test_is_interrogate_done() {
        let done_json = serde_json::json!({"state": "done"});
        assert_eq!(HordeAdapter::is_interrogate_done(&done_json), (true, false));

        let faulted_json = serde_json::json!({"state": "faulted"});
        assert_eq!(
            HordeAdapter::is_interrogate_done(&faulted_json),
            (false, true)
        );

        let waiting_json = serde_json::json!({"state": "waiting"});
        assert_eq!(
            HordeAdapter::is_interrogate_done(&waiting_json),
            (false, false)
        );
    }

    #[test]
    fn test_parse_prompt_directives_loras() {
        let prompt = "A girl in armor <lora:detail:0.8> <lora:face_v2:0.9:0.7> <lora:style> <LoRA:cyber:-0.5>";
        let parsed = parse_prompt_directives(prompt);

        assert_eq!(parsed.clean_prompt, "A girl in armor");
        assert_eq!(parsed.loras.len(), 4);

        assert_eq!(parsed.loras[0].name, "detail");
        assert_eq!(parsed.loras[0].model, 0.8);
        assert_eq!(parsed.loras[0].clip, 0.8);

        assert_eq!(parsed.loras[1].name, "face_v2");
        assert_eq!(parsed.loras[1].model, 0.9);
        assert_eq!(parsed.loras[1].clip, 0.7);

        assert_eq!(parsed.loras[2].name, "style");
        assert_eq!(parsed.loras[2].model, 1.0);
        assert_eq!(parsed.loras[2].clip, 1.0);

        assert_eq!(parsed.loras[3].name, "cyber");
        assert_eq!(parsed.loras[3].model, -0.5);
        assert_eq!(parsed.loras[3].clip, -0.5);
    }

    #[test]
    fn test_parse_prompt_directives_tis_and_embeddings() {
        let prompt =
            "sunset landscape <ti:bad_hands:0.9> <ti:easynegative> <emb:deepneg:0.6> <EMB:fastneg>";
        let parsed = parse_prompt_directives(prompt);

        assert_eq!(parsed.clean_prompt, "sunset landscape");
        assert_eq!(parsed.tis.len(), 4);

        assert_eq!(parsed.tis[0].name, "bad_hands");
        assert_eq!(parsed.tis[0].strength, 0.9);

        assert_eq!(parsed.tis[1].name, "easynegative");
        assert_eq!(parsed.tis[1].strength, 1.0);

        assert_eq!(parsed.tis[2].name, "deepneg");
        assert_eq!(parsed.tis[2].strength, 0.6);

        assert_eq!(parsed.tis[3].name, "fastneg");
        assert_eq!(parsed.tis[3].strength, 1.0);
    }

    #[test]
    fn test_parse_prompt_directives_flags() {
        let prompt = "cyberpunk city --sampler k_dpmpp_2m --steps 35 --cfg 7.5 --clip_skip 2 --hires --hires_denoising 0.4 --seed 123456 --control canny --post GFPGAN --upscale RealESRGAN_x4plus --allow_slow --any_worker --worker node_77 --no blurry, low res";
        let parsed = parse_prompt_directives(prompt);

        assert_eq!(parsed.clean_prompt, "cyberpunk city");
        assert_eq!(parsed.sampler_name.as_deref(), Some("k_dpmpp_2m"));
        assert_eq!(parsed.steps, Some(35));
        assert_eq!(parsed.cfg_scale, Some(7.5));
        assert_eq!(parsed.clip_skip, Some(2));
        assert_eq!(parsed.hires_fix, Some(true));
        assert_eq!(parsed.hires_fix_denoising_strength, Some(0.4));
        assert_eq!(parsed.seed, Some(123456));
        assert_eq!(parsed.control_type.as_deref(), Some("canny"));
        assert_eq!(parsed.post_processing, vec!["GFPGAN", "RealESRGAN_x4plus"]);
        assert_eq!(parsed.slow_workers, Some(true));
        assert_eq!(parsed.trusted_workers, Some(false));
        assert_eq!(parsed.workers, vec!["node_77"]);
        assert_eq!(parsed.negative_prompts, vec!["blurry, low res"]);
    }

    #[test]
    fn test_parse_prompt_directives_clamping() {
        let prompt = "test --steps 5 --cfg 0.2 --clip_skip 0 --hires_denoising 2.5";
        let parsed = parse_prompt_directives(prompt);
        assert_eq!(parsed.steps, Some(10));
        assert_eq!(parsed.cfg_scale, Some(1.0));
        assert_eq!(parsed.clip_skip, Some(1));
        assert_eq!(parsed.hires_fix_denoising_strength, Some(1.0));

        let prompt_high = "test --steps 500 --cfg 100.0 --clip_skip 20";
        let parsed_high = parse_prompt_directives(prompt_high);
        assert_eq!(parsed_high.steps, Some(100));
        assert_eq!(parsed_high.cfg_scale, Some(30.0));
        assert_eq!(parsed_high.clip_skip, Some(12));
    }

    #[test]
    fn test_clean_residual_prompt() {
        assert_eq!(
            clean_residual_prompt("  A  cat , , on a tree , "),
            "A cat, on a tree"
        );
        assert_eq!(
            clean_residual_prompt("A mountain, , highly detailed, "),
            "A mountain, highly detailed"
        );
        assert_eq!(clean_residual_prompt(", leading comma, "), "leading comma");
    }

    #[test]
    fn bench_clean_residual_prompt_baseline() {
        let sample =
            "  A  cat , , on a tree , , highly detailed, , , masterpiece , , high resolution, , ";
        let start = std::time::Instant::now();
        for _ in 0..10_000 {
            let res = clean_residual_prompt(sample);
            std::hint::black_box(res);
        }
        let elapsed = start.elapsed();
        println!("Elapsed for 10000 iterations: {elapsed:?}");
    }

    #[test]
    fn test_parse_prompt_directives_multibyte_safe() {
        let prompt = "A prompt with <🦀> and <🎨:1.0> and <lora:🦀_style:0.8> <ti:✨:0.5>";
        let parsed = parse_prompt_directives(prompt);
        assert_eq!(parsed.loras.len(), 1);
        assert_eq!(parsed.loras[0].name, "🦀_style");
        assert_eq!(parsed.tis.len(), 1);
        assert_eq!(parsed.tis[0].name, "✨");
        assert!(parsed.clean_prompt.contains("<🦀>"));
        assert!(parsed.clean_prompt.contains("<🎨:1.0>"));
    }

    #[test]
    fn test_build_horde_payload_with_all_directives() {
        let a = HordeAdapter::new();
        let req = ImageGenerationRequest {
            prompt: "A fantasy warrior <lora:armor:0.85:0.7> <ti:bad_hands:0.95> --sampler k_euler_a --steps 40 --cfg 8.0 --clip_skip 2 --hires --hires_denoising 0.35 --seed 9999 --control depth --post CodeFormers --worker fast_gpu_1 --no bad anatomy, extra limbs".into(),
            model: "SDXL 1.0".into(),
            n: Some(1),
            quality: None,
            response_format: None,
            size: Some("1024x1024".into()),
            style: None,
            user: None,
            aspect_ratio: None,
            seed: None,
            negative_prompt: Some("blurry".into()),
            post_processing: None,
        };

        let body = a.format_image_request(&req, "SDXL 1.0").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            v["prompt"],
            "A fantasy warrior ### blurry, bad anatomy, extra limbs"
        );
        assert_eq!(v["params"]["width"], 1024);
        assert_eq!(v["params"]["height"], 1024);
        assert_eq!(v["params"]["steps"], 40);
        assert_eq!(v["params"]["sampler_name"], "k_euler_a");
        assert_eq!(v["params"]["cfg_scale"], 8.0);
        assert_eq!(v["params"]["clip_skip"], 2);
        assert_eq!(v["params"]["seed"], 9999);
        assert!(v["params"]["hires_fix"].as_bool().unwrap());
        assert_eq!(v["params"]["hires_fix_denoising_strength"], 0.35);
        assert_eq!(v["params"]["control_type"], "depth");
        assert_eq!(v["params"]["post_processing"][0], "CodeFormers");

        let loras = v["params"]["loras"].as_array().unwrap();
        assert_eq!(loras.len(), 1);
        assert_eq!(loras[0]["name"], "armor");
        assert_eq!(loras[0]["model"], 0.85);
        assert_eq!(loras[0]["clip"], 0.7);

        let tis = v["params"]["tis"].as_array().unwrap();
        assert_eq!(tis.len(), 1);
        assert_eq!(tis[0]["name"], "bad_hands");
        assert_eq!(tis[0]["strength"], 0.95);

        let workers = v["params"]["workers"].as_array().unwrap();
        assert_eq!(workers[0], "fast_gpu_1");

        assert_eq!(v["slow_workers"], false);
        assert_eq!(v["trusted_workers"], true);
    }

    #[test]
    fn test_build_horde_payload_negative_prompt_deduplication() {
        let adapter = HordeAdapter::new();
        let req = ImageGenerationRequest {
            prompt:
                "A beautiful landscape --no blurry --no dark --no blurry --no low_quality --no dark"
                    .into(),
            model: "SDXL 1.0".into(),
            n: None,
            quality: None,
            response_format: None,
            size: None,
            style: None,
            user: None,
            aspect_ratio: None,
            seed: None,
            negative_prompt: Some("blurry".into()),
            post_processing: None,
        };

        let body = adapter.format_image_request(&req, "SDXL 1.0").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            v["prompt"],
            "A beautiful landscape ### blurry, dark, low_quality"
        );
    }
}
