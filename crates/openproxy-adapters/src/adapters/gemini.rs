use super::{
    AdapterAuthType, AdapterFormat, Arc, CoreError, DiscoveredModel, ModelId, ProviderAdapter,
    ProviderAdapterConfig, ProviderId, Result, TargetFormat, UpstreamClient,
    build_discovered_model_full, fetch_models_with_auth,
};

// =====================================================================
// Gemini (Google AI Studio)
// =====================================================================

/// Adapter for Google's Gemini API (`generativelanguage.googleapis.com`).
///
/// Gemini uses its own wire format (not OpenAI-compatible):
/// - Auth: `x-goog-api-key: <key>` header
/// - Chat URL: `${base}/models/${model}:generateContent`
/// - Models URL: `${base}/models`
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct GeminiAdapter {
    config: ProviderAdapterConfig,
}

impl GeminiAdapter {
    pub fn new() -> Self {
        Self {
            config: ProviderAdapterConfig {
                id: ProviderId::new("gemini"),
                name: "Google Gemini".into(),
                anonymous_fallback: false,
                rate_limit_scope: "account".into(),
                base_url: "https://generativelanguage.googleapis.com/v1beta".into(),
                auth_type: AdapterAuthType::GoogApiKey,
                format: AdapterFormat::Gemini,
                extra_headers: vec![],
            },
        }
    }
}

crate::adapters::derive_default_from_new!(GeminiAdapter);

impl ProviderAdapter for GeminiAdapter {
    fn config(&self) -> &ProviderAdapterConfig {
        &self.config
    }

    fn models_dev_canonical_ids(&self) -> &'static [&'static str] {
        &["google"]
    }

    fn build_chat_url(&self, _target_format: TargetFormat, model: &ModelId) -> String {
        // Gemini puts the model in the URL path.
        // Since openproxy always uses streaming to the upstream (dispatch_upstream
        // forces is_streaming = true and expects SSE chunks), we must use the
        // streamGenerateContent?alt=sse endpoint. Calling generateContent would
        // return a non-streaming JSON body, which blocks headers until completion
        // and causes timeouts.
        //
        // Sanitize the model id to prevent path traversal — strip "/"
        // segments.  Dot characters are *kept* because real model names
        // like "gemini-2.5-flash" contain them.
        let model_str = model.as_str();
        let safe_model: String = model_str.replace('/', "");
        format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.config.base_url, safe_model
        )
    }

    fn models_url(&self) -> Option<String> {
        Some(format!("{}/models", self.config.base_url))
    }

    async fn fetch_models(
        &self,
        upstream_client: &Arc<UpstreamClient>,
        api_key: &str,
    ) -> Result<Vec<DiscoveredModel>> {
        let url = self
            .models_url()
            .ok_or_else(|| CoreError::Internal("gemini: models_url is None (impossible)".into()))?;

        fetch_models_with_auth(
            &url,
            upstream_client,
            &[("x-goog-api-key", api_key)],
            "models",
            "gemini",
            |m| {
                let full_name = m.get("name").and_then(|v| v.as_str())?;
                let id = full_name.strip_prefix("models/").unwrap_or(full_name);
                let display_name = m
                    .get("displayName")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                let ctx = m.get("inputTokenLimit").and_then(serde_json::Value::as_i64);
                let out = m
                    .get("outputTokenLimit")
                    .and_then(serde_json::Value::as_i64);
                Some(build_discovered_model_full(
                    id.to_string(),
                    display_name,
                    TargetFormat::Gemini,
                    ctx,
                    out,
                ))
            },
        )
        .await
    }

    fn format_request(
        &self,
        _target_format: TargetFormat,
        req: &openproxy_types::OpenAIRequest,
        _model: &ModelId,
        messages: &[openproxy_types::OpenAIMessage],
        _stream: bool,
    ) -> std::result::Result<bytes::Bytes, CoreError> {
        let gemini_req = openai_to_gemini(req, messages);
        serde_json::to_vec(&gemini_req)
            .map(bytes::Bytes::from)
            .map_err(|e| CoreError::Parse(format!("serialize gemini request: {e}")))
    }

    fn translate_non_streaming_response(
        &self,
        _target_format: TargetFormat,
        response_body: serde_json::Value,
    ) -> std::result::Result<openproxy_types::OpenAIResponse, CoreError> {
        let gemini_resp: GeminiResponse =
            <GeminiResponse as serde::Deserialize>::deserialize(&response_body)
                .map_err(|e| CoreError::Parse(format!("parse gemini response: {e}")))?;
        Ok(gemini_to_openai(&gemini_resp))
    }
}

// =====================================================================
// Gemini translation & wire types
// =====================================================================

pub const DEFAULT_GEMINI_MAX_OUTPUT_TOKENS: u32 = 8192;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiSafetySetting {
    pub category: String,
    pub threshold: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiRequest {
    pub contents: Vec<GeminiContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GeminiContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GeminiGenerationConfig>,
    #[serde(
        default,
        rename = "safetySettings",
        skip_serializing_if = "Option::is_none"
    )]
    pub safety_settings: Option<Vec<GeminiSafetySetting>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiContent {
    pub role: String,
    pub parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct GeminiInlineData {
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct GeminiPart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<GeminiInlineData>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiGenerationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiResponse {
    #[serde(default)]
    pub candidates: Vec<GeminiCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_metadata: Option<GeminiUsageMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<GeminiInnerResponse>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiInnerResponse {
    #[serde(default)]
    pub candidates: Vec<GeminiCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiCandidate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<GeminiContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiUsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u32,
    #[serde(default)]
    pub candidates_token_count: u32,
    #[serde(default)]
    pub total_token_count: u32,
    #[serde(rename = "cachedContentTokenCount", default)]
    pub cached_content_token_count: Option<u32>,
}

pub fn normalize_audio_mime(format: &str) -> String {
    match format.to_lowercase().trim_start_matches('.') {
        "wav" | "x-wav" | "wave" => "audio/wav".to_string(),
        "mp3" | "mpeg" => "audio/mp3".to_string(),
        "m4a" | "aac" => "audio/m4a".to_string(),
        "ogg" | "opus" => "audio/ogg".to_string(),
        "flac" | "x-flac" => "audio/flac".to_string(),
        "aiff" | "x-aiff" => "audio/aiff".to_string(),
        "pcm" => "audio/pcm".to_string(),
        s if s.starts_with("audio/") => s.to_string(),
        _ => "audio/mp3".to_string(),
    }
}

pub fn parse_media_part_to_inline_data(part: &serde_json::Value) -> Option<GeminiInlineData> {
    let obj = part.as_object()?;
    let typ = obj.get("type").and_then(|v| v.as_str())?;
    match typ {
        "image_url" => {
            let url = obj.get("image_url")?.as_object()?.get("url")?.as_str()?;
            let stripped = url.strip_prefix("data:")?;
            let (mime_type, rest) = stripped.split_once(';')?;
            let (_, data) = rest.split_once(',')?;
            Some(GeminiInlineData {
                mime_type: mime_type.to_string(),
                data: data.to_string(),
            })
        }
        "audio_url" => {
            let audio_obj = obj.get("audio_url")?.as_object()?;
            let url = audio_obj.get("url")?.as_str()?;
            if let Some(stripped) = url.strip_prefix("data:") {
                let (mime_type, rest) = stripped.split_once(';')?;
                let (_, data) = rest.split_once(',')?;
                Some(GeminiInlineData {
                    mime_type: mime_type.to_string(),
                    data: data.to_string(),
                })
            } else {
                let mime = audio_obj
                    .get("mime_type")
                    .or_else(|| audio_obj.get("mimeType"))
                    .or_else(|| audio_obj.get("format"))
                    .and_then(|v| v.as_str())
                    .map_or_else(|| "audio/mp3".to_string(), normalize_audio_mime);
                Some(GeminiInlineData {
                    mime_type: mime,
                    data: url.to_string(),
                })
            }
        }
        "input_audio" | "audio" => {
            let audio_obj = obj.get("input_audio").or_else(|| obj.get("audio"))?.as_object()?;
            let data_raw = audio_obj.get("data")?.as_str()?;
            let format_raw = audio_obj
                .get("format")
                .or_else(|| audio_obj.get("mime_type"))
                .or_else(|| audio_obj.get("mimeType"))
                .and_then(|v| v.as_str())
                .unwrap_or("mp3");
            let mime_type = normalize_audio_mime(format_raw);
            let data = if let Some(stripped) = data_raw.strip_prefix("data:") {
                let (_, rest) = stripped.split_once(',')?;
                rest.to_string()
            } else {
                data_raw.to_string()
            };
            Some(GeminiInlineData {
                mime_type,
                data,
            })
        }
        _ => None,
    }
}

pub fn parse_image_url_to_inline_data(part: &serde_json::Value) -> Option<GeminiInlineData> {
    parse_media_part_to_inline_data(part)
}

fn message_content_to_gemini_parts(content: Option<&serde_json::Value>) -> Vec<GeminiPart> {
    match content {
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .map(|part| {
                if let Some(inline_data) = parse_media_part_to_inline_data(part) {
                    return GeminiPart {
                        inline_data: Some(inline_data),
                        ..Default::default()
                    };
                }
                GeminiPart {
                    text: Some(openproxy_types::extract_content_part_text(part)),
                    ..Default::default()
                }
            })
            .collect(),
        Some(serde_json::Value::String(s)) => vec![GeminiPart {
            text: Some(s.clone()),
            ..Default::default()
        }],
        Some(serde_json::Value::Null) | None => vec![GeminiPart {
            text: Some(String::new()),
            ..Default::default()
        }],
        Some(value) => vec![GeminiPart {
            text: Some(value.to_string()),
            ..Default::default()
        }],
    }
}

pub fn openai_to_gemini(
    req: &openproxy_types::OpenAIRequest,
    override_messages: &[openproxy_types::OpenAIMessage],
) -> GeminiRequest {
    let mut system_parts: Vec<String> = Vec::new();
    let mut contents: Vec<GeminiContent> = Vec::with_capacity(override_messages.len());

    for m in override_messages {
        match m.role.as_str() {
            "system" => system_parts.push(m.extract_text()),
            "user" => {
                contents.push(GeminiContent {
                    role: "user".to_string(),
                    parts: message_content_to_gemini_parts(m.content.as_ref()),
                });
            }
            "assistant" => {
                contents.push(GeminiContent {
                    role: "model".to_string(),
                    parts: message_content_to_gemini_parts(m.content.as_ref()),
                });
            }
            _ => {}
        }
    }

    let system_instruction = if system_parts.is_empty() {
        None
    } else {
        Some(GeminiContent {
            role: "system".to_string(),
            parts: vec![GeminiPart {
                text: Some(system_parts.join("\n\n")),
                ..Default::default()
            }],
        })
    };

    let generation_config = GeminiGenerationConfig {
        max_output_tokens: req.max_tokens.or(Some(DEFAULT_GEMINI_MAX_OUTPUT_TOKENS)),
        temperature: req.temperature,
        top_p: req.top_p,
        stop_sequences: req.stop.clone(),
    };

    let safety_settings = Some(vec![
        GeminiSafetySetting {
            category: "HARM_CATEGORY_HARASSMENT".to_string(),
            threshold: "BLOCK_NONE".to_string(),
        },
        GeminiSafetySetting {
            category: "HARM_CATEGORY_HATE_SPEECH".to_string(),
            threshold: "BLOCK_NONE".to_string(),
        },
        GeminiSafetySetting {
            category: "HARM_CATEGORY_SEXUALLY_EXPLICIT".to_string(),
            threshold: "BLOCK_NONE".to_string(),
        },
        GeminiSafetySetting {
            category: "HARM_CATEGORY_DANGEROUS_CONTENT".to_string(),
            threshold: "BLOCK_NONE".to_string(),
        },
        GeminiSafetySetting {
            category: "HARM_CATEGORY_CIVIC_INTEGRITY".to_string(),
            threshold: "BLOCK_NONE".to_string(),
        },
    ]);

    GeminiRequest {
        contents,
        system_instruction,
        generation_config: Some(generation_config),
        safety_settings,
    }
}

crate::define_jump_map! {
    /// O(1) jump-map for translating Gemini finish reasons to OpenAI finish reasons.
    pub fn map_gemini_finish_reason(reason: &str) -> &'static str {
        "MAX_TOKENS" => "length",
        "SAFETY" | "RECITATION" | "BLOCKLIST" => "content_filter",
        _ => "stop",
    }
}

pub fn gemini_to_openai(resp: &GeminiResponse) -> openproxy_types::OpenAIResponse {
    let candidates = if !resp.candidates.is_empty() {
        &resp.candidates
    } else if let Some(inner) = &resp.response {
        &inner.candidates
    } else {
        &resp.candidates
    };

    let candidate = candidates.first();

    let content = candidate
        .and_then(|c| c.content.as_ref())
        .map(|c| {
            c.parts
                .iter()
                .filter_map(|p| p.text.as_deref())
                .collect::<String>()
        })
        .filter(|t| !t.is_empty())
        .unwrap_or_default();

    let finish_reason = candidate
        .and_then(|c| c.finish_reason.as_deref())
        .map(map_gemini_finish_reason)
        .map(String::from);

    let usage_metadata = resp.usage_metadata.as_ref().or_else(|| {
        resp.response
            .as_ref()
            .and_then(|inner| inner.usage_metadata.as_ref())
    });

    let usage = usage_metadata.map(|u| openproxy_types::OpenAIUsage {
        prompt_tokens: u.prompt_token_count,
        completion_tokens: u.candidates_token_count,
        total_tokens: u.total_token_count,
        prompt_tokens_details: u.cached_content_token_count.map(|c| {
            openproxy_types::message::PromptTokensDetails {
                cached_tokens: Some(c),
            }
        }),
    });

    openproxy_types::OpenAIResponse {
        id: format!("gemini-{}", chrono::Utc::now().timestamp_millis()),
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: String::new(),
        choices: vec![openproxy_types::OpenAIChoice {
            index: 0,
            message: openproxy_types::OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::String(content)),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            finish_reason,
        }],
        usage,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_image_url_to_inline_data() {
        let part = json!({
            "type": "image_url",
            "image_url": { "url": "data:image/png;base64,iVBORw0KGgo=" }
        });
        let result = parse_image_url_to_inline_data(&part).unwrap();
        assert_eq!(result.mime_type, "image/png");
        assert_eq!(result.data, "iVBORw0KGgo=");
    }

    #[test]
    fn test_parse_audio_parts_to_inline_data() {
        let input_audio = json!({
            "type": "input_audio",
            "input_audio": {
                "data": "UklGRigAAABXQVZFZm10IBAAAAABAAEAQB8AAEAfAAABAAgAZGF0YQAAAAA=",
                "format": "wav"
            }
        });
        let result = parse_media_part_to_inline_data(&input_audio).unwrap();
        assert_eq!(result.mime_type, "audio/wav");
        assert_eq!(
            result.data,
            "UklGRigAAABXQVZFZm10IBAAAAABAAEAQB8AAEAfAAABAAgAZGF0YQAAAAA="
        );

        let audio_url = json!({
            "type": "audio_url",
            "audio_url": {
                "url": "data:audio/mp3;base64,//uQZAAAAAAAAAAAAAAAAAAAAAA=="
            }
        });
        let result = parse_media_part_to_inline_data(&audio_url).unwrap();
        assert_eq!(result.mime_type, "audio/mp3");
        assert_eq!(result.data, "//uQZAAAAAAAAAAAAAAAAAAAAAA==");
    }

    #[test]
    fn test_gemini_to_openai_standard() {
        let resp = GeminiResponse {
            candidates: vec![GeminiCandidate {
                content: Some(GeminiContent {
                    role: "model".to_string(),
                    parts: vec![GeminiPart {
                        text: Some("Hello from Gemini".to_string()),
                        inline_data: None,
                    }],
                }),
                finish_reason: Some("STOP".to_string()),
            }],
            usage_metadata: Some(GeminiUsageMetadata {
                prompt_token_count: 10,
                candidates_token_count: 20,
                total_token_count: 30,
                cached_content_token_count: None,
            }),
            response: None,
        };

        let openai_resp = gemini_to_openai(&resp);
        assert_eq!(openai_resp.object, "chat.completion");
        assert_eq!(openai_resp.choices.len(), 1);

        let choice = &openai_resp.choices[0];
        assert_eq!(choice.finish_reason.as_deref(), Some("stop"));
        assert_eq!(choice.message.role, "assistant");
        assert_eq!(
            choice.message.content.as_ref().unwrap().as_str().unwrap(),
            "Hello from Gemini"
        );

        let usage = openai_resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 20);
        assert_eq!(usage.total_tokens, 30);
    }

    #[test]
    fn test_openai_to_gemini_string_and_array_content() {
        let req = openproxy_types::OpenAIRequest {
            model: "gemini-2.5-pro".to_string(),
            messages: vec![],
            ..Default::default()
        };
        let messages = vec![
            openproxy_types::OpenAIMessage {
                role: "system".to_string(),
                content: Some(json!("You are helpful")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::default(),
            },
            openproxy_types::OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("Direct string")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::default(),
            },
            openproxy_types::OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(json!([
                    {"type": "text", "text": "Part 1 "},
                    {"type": "text", "content": "Part 2"}
                ])),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::default(),
            },
        ];

        let gemini_req = openai_to_gemini(&req, &messages);
        assert_eq!(
            gemini_req.system_instruction.unwrap().parts[0]
                .text
                .as_deref(),
            Some("You are helpful")
        );
        assert_eq!(gemini_req.contents.len(), 2);
        assert_eq!(gemini_req.contents[0].role, "user");
        assert_eq!(
            gemini_req.contents[0].parts[0].text.as_deref(),
            Some("Direct string")
        );
        assert_eq!(gemini_req.contents[1].role, "model");
        assert_eq!(
            gemini_req.contents[1].parts[0].text.as_deref(),
            Some("Part 1 ")
        );
        assert_eq!(
            gemini_req.contents[1].parts[1].text.as_deref(),
            Some("Part 2")
        );
    }

    #[test]
    fn test_gemini_finish_reason_mapping() {
        assert_eq!(map_gemini_finish_reason("MAX_TOKENS"), "length");
        assert_eq!(map_gemini_finish_reason("SAFETY"), "content_filter");
        assert_eq!(map_gemini_finish_reason("RECITATION"), "content_filter");
        assert_eq!(map_gemini_finish_reason("BLOCKLIST"), "content_filter");
        assert_eq!(map_gemini_finish_reason("STOP"), "stop");
        assert_eq!(map_gemini_finish_reason("OTHER"), "stop");
    }
}
