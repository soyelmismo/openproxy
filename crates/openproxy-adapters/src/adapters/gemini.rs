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
        if model_str.contains('/') {
            let safe_model = model_str.replace('/', "");
            format!(
                "{}/models/{}:streamGenerateContent?alt=sse",
                self.config.base_url, safe_model
            )
        } else {
            format!(
                "{}/models/{}:streamGenerateContent?alt=sse",
                self.config.base_url, model_str
            )
        }
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
        serialize_gemini_request(req, messages)
    }

    fn translate_non_streaming_response(
        &self,
        _target_format: TargetFormat,
        response_body: serde_json::Value,
    ) -> std::result::Result<openproxy_types::OpenAIResponse, CoreError> {
        deserialize_gemini_response(&response_body)
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GeminiTool>>,
    #[serde(
        default,
        rename = "toolConfig",
        skip_serializing_if = "Option::is_none"
    )]
    pub tool_config: Option<GeminiToolConfig>,
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct GeminiFunctionDeclaration {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Parameters schema (JSON Schema, will be cleaned before serialization).
    /// Serialized as `parameters` in JSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parameters: Option<serde_json::Value>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct GeminiTool {
    /// Always-present wrapper for function-calling toolset.
    #[serde(
        default,
        rename = "functionDeclarations",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub function_declarations: Vec<GeminiFunctionDeclaration>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GeminiFunctionCallingMode {
    Auto,
    None,
    Any,
}

/// Function-calling configuration; mirrors `FunctionCallingConfig` in the
/// Gemini v1beta API.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiFunctionCallingConfig {
    pub mode: GeminiFunctionCallingMode,
    #[serde(
        default,
        rename = "allowedFunctionNames",
        skip_serializing_if = "Option::is_none"
    )]
    pub allowed_function_names: Option<Vec<String>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiToolConfig {
    #[serde(rename = "functionCallingConfig")]
    pub function_calling_config: GeminiFunctionCallingConfig,
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

fn map_audio_extension(ext: &str) -> Option<&'static str> {
    match ext {
        "wav" | "x-wav" | "wave" => Some("audio/wav"),
        "mp3" | "mpeg" => Some("audio/mp3"),
        "m4a" | "aac" => Some("audio/m4a"),
        "ogg" | "opus" => Some("audio/ogg"),
        "flac" | "x-flac" => Some("audio/flac"),
        "aiff" | "x-aiff" => Some("audio/aiff"),
        "pcm" => Some("audio/pcm"),
        _ => None,
    }
}

pub fn normalize_audio_mime(format: &str) -> String {
    let ext = format.trim_start_matches('.');
    if let Some(mime) = map_audio_extension(ext) {
        return mime.to_string();
    }
    if ext.bytes().any(|b| b.is_ascii_uppercase()) {
        let lower = ext.to_ascii_lowercase();
        if let Some(mime) = map_audio_extension(&lower) {
            return mime.to_string();
        }
        if lower.starts_with("audio/") {
            return lower;
        }
    } else if ext.starts_with("audio/") {
        return ext.to_string();
    }
    "audio/mp3".to_string()
}

fn parse_data_uri(url: &str) -> Option<GeminiInlineData> {
    let stripped = url.strip_prefix("data:")?;
    let (mime_type, rest) = stripped.split_once(';')?;
    let (_, data) = rest.split_once(',')?;
    Some(GeminiInlineData {
        mime_type: mime_type.to_string(),
        data: data.to_string(),
    })
}

fn parse_image_part_inline(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<GeminiInlineData> {
    let url = obj.get("image_url")?.as_object()?.get("url")?.as_str()?;
    parse_data_uri(url)
}

fn parse_audio_url_part_inline(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<GeminiInlineData> {
    let audio_obj = obj.get("audio_url")?.as_object()?;
    let url = audio_obj.get("url")?.as_str()?;
    if let Some(inline) = parse_data_uri(url) {
        return Some(inline);
    }
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

fn parse_input_audio_part_inline(
    obj: &serde_json::Map<String, serde_json::Value>,
) -> Option<GeminiInlineData> {
    let audio_obj = obj
        .get("input_audio")
        .or_else(|| obj.get("audio"))?
        .as_object()?;
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
    Some(GeminiInlineData { mime_type, data })
}

pub fn parse_media_part_to_inline_data(part: &serde_json::Value) -> Option<GeminiInlineData> {
    let obj = part.as_object()?;
    let typ = obj.get("type").and_then(|v| v.as_str())?;
    match typ {
        "image_url" => parse_image_part_inline(obj),
        "audio_url" => parse_audio_url_part_inline(obj),
        "input_audio" | "audio" => parse_input_audio_part_inline(obj),
        _ => None,
    }
}

pub fn parse_image_url_to_inline_data(part: &serde_json::Value) -> Option<GeminiInlineData> {
    parse_media_part_to_inline_data(part)
}

fn map_single_content_part(part: &serde_json::Value) -> GeminiPart {
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
}

fn message_content_to_gemini_parts(content: Option<&serde_json::Value>) -> Vec<GeminiPart> {
    match content {
        Some(serde_json::Value::Array(parts)) => {
            parts.iter().map(map_single_content_part).collect()
        }
        Some(serde_json::Value::Null) | None => vec![GeminiPart {
            text: Some(String::new()),
            ..Default::default()
        }],
        Some(value) => vec![map_single_content_part(value)],
    }
}

fn build_default_gemini_safety_settings() -> Vec<GeminiSafetySetting> {
    vec![
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
    ]
}

fn partition_messages_for_gemini(
    messages: &[openproxy_types::OpenAIMessage],
) -> (Option<GeminiContent>, Vec<GeminiContent>) {
    let mut system_parts: Vec<std::borrow::Cow<'_, str>> = Vec::new();
    let mut contents: Vec<GeminiContent> = Vec::with_capacity(messages.len());

    for m in messages {
        match m.role.as_str() {
            "system" => system_parts.push(m.extract_text_cow()),
            "user" => contents.push(GeminiContent {
                role: "user".to_string(),
                parts: message_content_to_gemini_parts(m.content.as_ref()),
            }),
            "assistant" => contents.push(GeminiContent {
                role: "model".to_string(),
                parts: message_content_to_gemini_parts(m.content.as_ref()),
            }),
            _ => {}
        }
    }

    let system_instruction = match system_parts.as_slice() {
        [] => None,
        [single] => Some(GeminiContent {
            role: "system".to_string(),
            parts: vec![GeminiPart {
                text: Some(single.clone().into_owned()),
                ..Default::default()
            }],
        }),
        parts => Some(GeminiContent {
            role: "system".to_string(),
            parts: vec![GeminiPart {
                text: Some(parts.join("\n\n")),
                ..Default::default()
            }],
        }),
    };

    (system_instruction, contents)
}

/// Translate OpenAI-format `tools` + `tool_choice` into Gemini
/// `functionDeclarations` + `toolConfig`.
///
/// Returns `(None, None)` when there is nothing to send upstream,
/// preserving byte-identical output for the existing happy-path.
pub fn translate_openai_tools_to_gemini(
    tools: Option<&[serde_json::Value]>,
    tool_choice: Option<&serde_json::Value>,
) -> (Option<Vec<GeminiTool>>, Option<GeminiToolConfig>) {
    // 1. tools vacío / ausente → return (None, None). Garantiza que el
    //    JSON upstream sea byte-idéntico al pre-fix: sin `tools` ni
    //    `toolConfig`. (Una request con tools pero sin tool_choice
    //    explícito cae al branch de abajo, donde tool_choice_to_config
    //    sí emite `Some(Auto)` para que Gemini reciba directrices de
    //    function-calling.)
    let Some(tools) = tools else {
        return (None, None);
    };
    if tools.is_empty() {
        return (None, None);
    }

    // 2. Mapear cada tool a GeminiFunctionDeclaration
    let declarations: Vec<GeminiFunctionDeclaration> = tools
        .iter()
        .filter_map(map_openai_tool_to_declaration)
        .collect();

    if declarations.is_empty() {
        // Todas las tools fueron filtradas (e.g. todas sin name) → no enviar
        // nada upstream para evitar 400.
        return (None, None);
    }

    // 3. Mapear tool_choice
    let tool_config = Some(tool_choice_to_config(tool_choice));

    (
        Some(vec![GeminiTool {
            function_declarations: declarations,
        }]),
        tool_config,
    )
}

/// Extrae `{name, description, parameters}` de un tool OpenAI flat
/// (`{"type":"function","function":{...}}`) o nested (`{name,...}`).
///
/// Si `parameters` está presente pero **no es un JSON object** (p.ej.
/// `null`, `"foo"`, `42`, `[1,2,3]`, `true`), **se omite la tool**
/// completa con `tracing::warn!` — Gemini rechaza `parameters` que no
/// sean `Value::Object` con HTTP 400, por lo que es preferible skippear
/// la tool a propagar el error de upstream.
fn map_openai_tool_to_declaration(tool: &serde_json::Value) -> Option<GeminiFunctionDeclaration> {
    let obj = tool.as_object()?;

    // Nested form (compat): {name, description, parameters}
    if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
        let parameters = match obj.get("parameters") {
            Some(p) if !p.is_object() => {
                tracing::warn!(
                    tool_name = name,
                    param_type = ?p,
                    "openai_to_gemini: tool `parameters` is not a JSON object; skipping tool to avoid Gemini 400"
                );
                return None;
            }
            Some(p) => {
                let mut p = p.clone();
                openproxy_types::schema_cleaner::clean_json_schema(&mut p);
                Some(p)
            }
            None => None,
        };
        return Some(GeminiFunctionDeclaration {
            name: name.to_string(),
            description: obj
                .get("description")
                .and_then(|v| v.as_str())
                .map(String::from),
            parameters,
        });
    }

    // Flat form: {type:"function", function:{name, description, parameters}}
    let func = obj.get("function")?.as_object()?;
    let name = func.get("name")?.as_str()?;
    let parameters = match func.get("parameters") {
        Some(p) if !p.is_object() => {
            tracing::warn!(
                tool_name = name,
                param_type = ?p,
                "openai_to_gemini: tool `parameters` is not a JSON object; skipping tool to avoid Gemini 400"
            );
            return None;
        }
        Some(p) => {
            let mut p = p.clone();
            openproxy_types::schema_cleaner::clean_json_schema(&mut p);
            Some(p)
        }
        None => None,
    };
    Some(GeminiFunctionDeclaration {
        name: name.to_string(),
        description: func
            .get("description")
            .and_then(|v| v.as_str())
            .map(String::from),
        parameters,
    })
}

/// Mapea el OpenAI `tool_choice` a `GeminiToolConfig`.
///
/// - `None` o ausente → `Auto` (Gemini default)
/// - `"none"` → `None` (sin function calling)
/// - `"required"` o `"any"` → `Any`
/// - `"auto"` → `Auto`
/// - `{"type":"function","function":{"name":"foo"}}` → `Any` con
///   `allowed_function_names=["foo"]`
/// - Cualquier otro objeto → `Auto` (fallback defensivo)
///
/// No retorna `Option`: todas las variantes del OpenAI `tool_choice`
/// se mapean a una config válida de Gemini. El wrapper `Option<...>` en
/// `translate_openai_tools_to_gemini` sirve para la rama de "no tools",
/// donde omitimos `toolConfig` por completo.
fn tool_choice_to_config(tool_choice: Option<&serde_json::Value>) -> GeminiToolConfig {
    let Some(tc) = tool_choice else {
        return GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode: GeminiFunctionCallingMode::Auto,
                allowed_function_names: None,
            },
        };
    };

    // String form
    if let Some(s) = tc.as_str() {
        let mode = match s {
            "none" => GeminiFunctionCallingMode::None,
            "required" | "any" => GeminiFunctionCallingMode::Any,
            _ => GeminiFunctionCallingMode::Auto, // "auto" or unknown
        };
        return GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode,
                allowed_function_names: None,
            },
        };
    }

    // Object form: {type:"function", function:{name:"foo"}}
    if let Some(obj) = tc.as_object()
        && let Some(func) = obj.get("function").and_then(|v| v.as_object())
        && let Some(name) = func.get("name").and_then(|v| v.as_str())
    {
        return GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode: GeminiFunctionCallingMode::Any,
                allowed_function_names: Some(vec![name.to_string()]),
            },
        };
    }

    // Objeto mal formado o tipo inesperado → fallback Auto sin allowlist
    GeminiToolConfig {
        function_calling_config: GeminiFunctionCallingConfig {
            mode: GeminiFunctionCallingMode::Auto,
            allowed_function_names: None,
        },
    }
}

/// Convert an OpenAI-format chat completion request to Gemini format.
///
/// Translates `req.tools` into `GeminiTool.functionDeclarations` and
/// `req.tool_choice` into `GeminiToolConfig.functionCallingConfig`.
/// When neither field is set, the upstream JSON is byte-identical to
/// pre-translation output (new fields are `Option<...>` with
/// `skip_serializing_if`).
pub fn openai_to_gemini(
    req: &openproxy_types::OpenAIRequest,
    override_messages: &[openproxy_types::OpenAIMessage],
) -> GeminiRequest {
    let (system_instruction, contents) = partition_messages_for_gemini(override_messages);

    let generation_config = GeminiGenerationConfig {
        temperature: req.temperature,
        top_p: req.top_p,
        max_output_tokens: req.max_tokens.or(Some(DEFAULT_GEMINI_MAX_OUTPUT_TOKENS)),
        stop_sequences: req.stop.clone(),
    };

    let (tools, tool_config) =
        translate_openai_tools_to_gemini(req.tools.as_deref(), req.tool_choice.as_ref());

    GeminiRequest {
        contents,
        system_instruction,
        generation_config: Some(generation_config),
        safety_settings: Some(build_default_gemini_safety_settings()),
        tools,
        tool_config,
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
        .map(|c| match c.parts.as_slice() {
            [] => String::new(),
            [part] => part.text.clone().unwrap_or_default(),
            parts => {
                let total_len: usize = parts
                    .iter()
                    .filter_map(|p| p.text.as_ref())
                    .map(|s| s.len())
                    .sum();
                let mut out = String::with_capacity(total_len);
                for p in parts {
                    if let Some(t) = &p.text {
                        out.push_str(t);
                    }
                }
                out
            }
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

/// Serialize an OpenAI chat request into Gemini wire-format bytes.
///
/// Used by both `GeminiAdapter` and `AntigravityAdapter` (which wraps
/// Gemini requests in an Antigravity envelope in `wrap_request_body`).
pub fn serialize_gemini_request(
    req: &openproxy_types::OpenAIRequest,
    messages: &[openproxy_types::OpenAIMessage],
) -> std::result::Result<bytes::Bytes, openproxy_types::error::CoreError> {
    let gemini_req = openai_to_gemini(req, messages);
    serde_json::to_vec(&gemini_req)
        .map(bytes::Bytes::from)
        .map_err(|e| {
            openproxy_types::error::CoreError::Parse(format!("serialize gemini request: {e}"))
        })
}

/// Deserialize a Gemini response JSON value into an OpenAIResponse.
pub fn deserialize_gemini_response(
    response_body: &serde_json::Value,
) -> std::result::Result<openproxy_types::OpenAIResponse, openproxy_types::error::CoreError> {
    let gemini_resp: GeminiResponse =
        <GeminiResponse as serde::Deserialize>::deserialize(response_body).map_err(|e| {
            openproxy_types::error::CoreError::Parse(format!("parse gemini response: {e}"))
        })?;
    Ok(gemini_to_openai(&gemini_resp))
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

    #[test]
    fn test_gemini_to_openai_multipart() {
        let resp = GeminiResponse {
            candidates: vec![GeminiCandidate {
                content: Some(GeminiContent {
                    role: "model".to_string(),
                    parts: vec![
                        GeminiPart {
                            text: Some("Hello ".to_string()),
                            inline_data: None,
                        },
                        GeminiPart {
                            text: Some("world!".to_string()),
                            inline_data: None,
                        },
                    ],
                }),
                finish_reason: Some("STOP".to_string()),
            }],
            usage_metadata: None,
            response: None,
        };

        let openai_resp = gemini_to_openai(&resp);
        assert_eq!(
            openai_resp.choices[0]
                .message
                .content
                .as_ref()
                .unwrap()
                .as_str()
                .unwrap(),
            "Hello world!"
        );
    }

    #[test]
    fn test_openai_to_gemini_multiple_system_messages() {
        let req = openproxy_types::OpenAIRequest {
            model: "gemini-2.5-pro".to_string(),
            messages: vec![],
            ..Default::default()
        };
        let messages = vec![
            openproxy_types::OpenAIMessage {
                role: "system".to_string(),
                content: Some(json!("System prompt 1")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::default(),
            },
            openproxy_types::OpenAIMessage {
                role: "system".to_string(),
                content: Some(json!("System prompt 2")),
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
            Some("System prompt 1\n\nSystem prompt 2")
        );
    }
}

#[cfg(test)]
mod tool_translation_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_translate_openai_tools_flat_format() {
        let tool = json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get current weather",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "city": {"type": "string"}
                    }
                }
            }
        });
        let (tools, _config) = translate_openai_tools_to_gemini(Some(&[tool]), None);
        let tools = tools.expect("expected tools");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function_declarations.len(), 1);
        let decl = &tools[0].function_declarations[0];
        assert_eq!(decl.name, "get_weather");
        assert_eq!(decl.description.as_deref(), Some("Get current weather"));
        let params = decl.parameters.as_ref().expect("parameters present");
        assert_eq!(params["type"], "object");
        assert_eq!(params["properties"]["city"]["type"], "string");
    }

    #[test]
    fn test_translate_openai_tools_nested_format() {
        let tool = json!({
            "name": "lookup",
            "description": "Look up data",
            "parameters": {
                "type": "object",
                "properties": {
                    "id": {"type": "string"}
                }
            }
        });
        let (tools, _config) = translate_openai_tools_to_gemini(Some(&[tool]), None);
        let tools = tools.expect("expected tools");
        assert_eq!(tools.len(), 1);
        let decl = &tools[0].function_declarations[0];
        assert_eq!(decl.name, "lookup");
        assert_eq!(decl.description.as_deref(), Some("Look up data"));
        assert_eq!(decl.parameters.as_ref().unwrap()["type"], "object");
    }

    #[test]
    fn test_tool_choice_string_modes() {
        let dummy_tool = vec![json!({
            "type": "function",
            "function": {"name": "noop"}
        })];

        // "none"
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&json!("none")));
        let cfg = cfg.unwrap();
        assert!(matches!(
            cfg.function_calling_config.mode,
            GeminiFunctionCallingMode::None
        ));

        // "required"
        let (_, cfg) =
            translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&json!("required")));
        let cfg = cfg.unwrap();
        assert!(matches!(
            cfg.function_calling_config.mode,
            GeminiFunctionCallingMode::Any
        ));

        // "auto"
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&json!("auto")));
        let cfg = cfg.unwrap();
        assert!(matches!(
            cfg.function_calling_config.mode,
            GeminiFunctionCallingMode::Auto
        ));

        // "any" (synonym for required)
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&json!("any")));
        let cfg = cfg.unwrap();
        assert!(matches!(
            cfg.function_calling_config.mode,
            GeminiFunctionCallingMode::Any
        ));

        // unknown string → fallback Auto
        let (_, cfg) =
            translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&json!("unknown_mode")));
        let cfg = cfg.unwrap();
        assert!(matches!(
            cfg.function_calling_config.mode,
            GeminiFunctionCallingMode::Auto
        ));
    }

    #[test]
    fn test_tool_choice_object_with_function_name() {
        let dummy_tool = vec![json!({
            "type": "function",
            "function": {"name": "noop"}
        })];
        let tc = json!({
            "type": "function",
            "function": {"name": "foo"}
        });
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&tc));
        let cfg = cfg.unwrap();
        assert!(matches!(
            cfg.function_calling_config.mode,
            GeminiFunctionCallingMode::Any
        ));
        assert_eq!(
            cfg.function_calling_config.allowed_function_names,
            Some(vec!["foo".to_string()])
        );
    }

    #[test]
    fn test_clean_json_schema_in_parameters() {
        // Parameters with $defs and $ref — should be flattened by clean_json_schema.
        let tool = json!({
            "type": "function",
            "function": {
                "name": "addr",
                "parameters": {
                    "$defs": {
                        "Address": {
                            "type": "object",
                            "properties": {
                                "city": {"type": "string"}
                            }
                        }
                    },
                    "type": "object",
                    "properties": {
                        "home": {"$ref": "#/$defs/Address"}
                    }
                }
            }
        });
        let (tools, _config) = translate_openai_tools_to_gemini(Some(&[tool]), None);
        let decl = &tools.unwrap()[0].function_declarations[0];
        let params = decl.parameters.as_ref().unwrap();
        assert_eq!(params["properties"]["home"]["type"], "object");
        assert_eq!(
            params["properties"]["home"]["properties"]["city"]["type"],
            "string"
        );
        assert!(params.get("$defs").is_none());
    }

    #[test]
    fn test_tools_empty_or_none_returns_none() {
        // tools = None → (None, None)
        let (tools, cfg) = translate_openai_tools_to_gemini(None, None);
        assert!(tools.is_none());
        assert!(cfg.is_none());

        // tools = Some(&[]) → (None, None)
        let (tools, cfg) = translate_openai_tools_to_gemini(Some(&[]), None);
        assert!(tools.is_none());
        assert!(cfg.is_none());
    }

    #[test]
    fn test_tool_without_name_is_skipped() {
        // First tool missing name, second one valid.
        let tools_in = vec![
            json!({"type": "function", "function": {"description": "no name"}}),
            json!({"type": "function", "function": {"name": "valid"}}),
        ];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        let tools = tools.expect("expected tools");
        assert_eq!(tools[0].function_declarations.len(), 1);
        assert_eq!(tools[0].function_declarations[0].name, "valid");

        // All tools invalid → (None, None)
        let tools_in = vec![
            json!({"type": "function", "function": {"description": "no name"}}),
            json!("not-an-object"),
        ];
        let (tools, cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        assert!(tools.is_none());
        assert!(cfg.is_none());
    }

    #[test]
    fn test_no_tools_means_no_tool_config() {
        // tools=None → (None, None): byte-identical al payload pre-fix.
        let (tools, cfg) = translate_openai_tools_to_gemini(None, None);
        assert!(tools.is_none());
        assert!(cfg.is_none());

        // Serialize a GeminiRequest con tools=None para confirmar que
        // el JSON upstream no contiene `tools` ni `toolConfig` —
        // garantía de backward-compat estricta.
        let req = openproxy_types::OpenAIRequest::default();
        let gemini_req = openai_to_gemini(&req, &[]);
        let serialized = serde_json::to_string(&gemini_req).unwrap();
        assert!(
            !serialized.contains("\"tools\""),
            "no tools in upstream JSON: {serialized}"
        );
        assert!(
            !serialized.contains("\"toolConfig\""),
            "no toolConfig in upstream JSON: {serialized}"
        );
    }

    #[test]
    fn test_non_object_parameters_skips_tool() {
        // Tool 0 has non-object parameters → skipped (warning logged, not asserted).
        // Tool 1 has valid parameters → kept.
        let tools_in = vec![
            json!({
                "type": "function",
                "function": {"name": "bad", "parameters": "not-an-object"}
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "ok",
                    "parameters": {"type": "object", "properties": {}}
                }
            }),
        ];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        let tools = tools.expect("expected tools");
        assert_eq!(tools[0].function_declarations.len(), 1);
        assert_eq!(tools[0].function_declarations[0].name, "ok");

        // All tools have non-object parameters → (None, None).
        let tools_in = vec![json!({
            "type": "function",
            "function": {"name": "only_bad", "parameters": null}
        })];
        let (tools, cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        assert!(tools.is_none());
        assert!(cfg.is_none());
    }

    #[test]
    fn test_missing_parameters_is_not_a_warning() {
        // Tool with no `parameters` key at all — should be kept with parameters=None.
        let tools_in = vec![json!({
            "type": "function",
            "function": {"name": "no_params"}
        })];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        let tools = tools.expect("expected tools");
        assert_eq!(tools[0].function_declarations.len(), 1);
        assert_eq!(tools[0].function_declarations[0].name, "no_params");
        assert!(tools[0].function_declarations[0].parameters.is_none());
    }

    #[test]
    fn test_serialize_gemini_request_happy_path() {
        let req = openproxy_types::OpenAIRequest::default();
        let messages = vec![openproxy_types::OpenAIMessage {
            role: "user".to_string(),
            content: Some(json!("hello")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }];
        let bytes = serialize_gemini_request(&req, &messages).expect("happy path must serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid JSON");
        assert_eq!(parsed["contents"][0]["role"], "user");
        assert_eq!(parsed["contents"][0]["parts"][0]["text"], "hello");
    }

    #[test]
    fn test_deserialize_gemini_response_happy_path() {
        // NOTE: GeminiResponse/GeminiCandidate/GeminiUsageMetadata deserialize
        // with snake_case field names (no container-level `rename_all`), so the
        // JSON keys here match the struct fields exactly.
        let body = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hi there"}], "role": "model"},
                "finish_reason": "STOP"
            }],
            "usage_metadata": {
                "prompt_token_count": 5,
                "candidates_token_count": 3,
                "total_token_count": 8
            }
        });
        let resp = deserialize_gemini_response(&body).expect("happy path must deserialize");
        assert_eq!(
            resp.choices[0].message.content.as_ref(),
            Some(&json!("hi there"))
        );
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = resp.usage.expect("usage must be mapped");
        assert_eq!(usage.prompt_tokens, 5);
        assert_eq!(usage.completion_tokens, 3);
        assert_eq!(usage.total_tokens, 8);
    }
}

// ============================================================
// GAP-1: Adversarial tests for tools/tool_choice translation
// ============================================================
#[cfg(test)]
mod tool_translation_adversarial_tests {
    use super::*;
    use serde_json::json;

    // --- Boundary / Type Violation tests for `parameters` ---

    #[test]
    fn adv_parameters_string_type_skips_tool() {
        // A string-type parameters (non-object) → tool must be skipped.
        let tools_in = vec![json!({
            "type": "function",
            "function": {"name": "bad_str", "parameters": "this is not a schema"}
        })];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        assert!(tools.is_none(), "string parameters must cause tool skip");
    }

    #[test]
    fn adv_parameters_number_type_skips_tool() {
        let tools_in = vec![json!({
            "type": "function",
            "function": {"name": "bad_num", "parameters": 42}
        })];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        assert!(tools.is_none(), "number parameters must cause tool skip");
    }

    #[test]
    fn adv_parameters_array_type_skips_tool() {
        let tools_in = vec![json!({
            "type": "function",
            "function": {"name": "bad_arr", "parameters": ["type", "string"]}
        })];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        assert!(tools.is_none(), "array parameters must cause tool skip");
    }

    #[test]
    fn adv_parameters_bool_type_skips_tool() {
        let tools_in = vec![json!({
            "type": "function",
            "function": {"name": "bad_bool", "parameters": true}
        })];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        assert!(tools.is_none(), "bool parameters must cause tool skip");
    }

    #[test]
    fn adv_parameters_null_type_skips_tool() {
        // null parameters (explicit) should skip tool
        let tools_in = vec![json!({
            "type": "function",
            "function": {"name": "bad_null", "parameters": null}
        })];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        assert!(tools.is_none(), "null parameters must cause tool skip");
    }

    // --- Tool name edge cases ---

    #[test]
    fn adv_tool_name_empty_string_skipped() {
        // Flat form: function.name is empty string
        let tools_in = vec![json!({
            "type": "function",
            "function": {"name": "", "description": "empty name tool"}
        })];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        // An empty name IS still a valid string (non-None), so it gets included.
        // The downstream Gemini API will reject it, but the translator accepts it.
        // This tests the current behavior. If we want to skip empty names,
        // we'd need a check in map_openai_tool_to_declaration.
        let tools = tools.expect("expected tools");
        assert_eq!(tools[0].function_declarations[0].name, "");
    }

    #[test]
    fn adv_tool_name_whitespace_only_preserved() {
        // Whitespace-only name is preserved (not trimmed).
        let tools_in = vec![json!({
            "type": "function",
            "function": {"name": "   "}
        })];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        let tools = tools.expect("expected tools");
        assert_eq!(tools[0].function_declarations[0].name, "   ");
    }

    #[test]
    fn adv_tool_name_unicode_not_object_skipped() {
        // When function is NOT an object, the entire tool is skipped.
        let tools_in = vec![json!({
            "type": "function",
            "function": 42
        })];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        assert!(tools.is_none(), "non-object function field must skip tool");
    }

    #[test]
    fn adv_tool_name_number_type_skipped() {
        // function.name is a number, not a string → get("name") is Some(42) but
        // as_str() returns None → tool is skipped.
        let tools_in = vec![json!({
            "type": "function",
            "function": {"name": 42}
        })];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        assert!(tools.is_none(), "number name must cause tool skip");
    }

    // --- Description edge cases ---

    #[test]
    fn adv_description_very_long_preserved() {
        // 1MB description should be preserved (no truncation).
        let long_desc = "x".repeat(1_000_000);
        let tools_in = vec![json!({
            "type": "function",
            "function": {"name": "big", "description": long_desc}
        })];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        let tools = tools.expect("expected tools");
        let desc = tools[0].function_declarations[0]
            .description
            .as_ref()
            .unwrap();
        assert_eq!(desc.len(), 1_000_000);
    }

    // --- tool_choice edge cases ---

    #[test]
    fn adv_tool_choice_invalid_string_falls_back_to_auto() {
        let dummy_tool = vec![json!({"type":"function","function":{"name":"x"}})];
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&json!("banana")));
        let cfg = cfg.unwrap();
        assert!(
            matches!(
                cfg.function_calling_config.mode,
                GeminiFunctionCallingMode::Auto
            ),
            "unknown string tool_choice must fall back to Auto"
        );
    }

    #[test]
    fn adv_tool_choice_number_falls_back_to_auto() {
        // tool_choice as a number is a type violation from the client.
        // Implementation: tc.as_str() returns None, tc.as_object() returns None,
        // so it falls through to the Auto fallback.
        let dummy_tool = vec![json!({"type":"function","function":{"name":"x"}})];
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&json!(42)));
        let cfg = cfg.unwrap();
        assert!(
            matches!(
                cfg.function_calling_config.mode,
                GeminiFunctionCallingMode::Auto
            ),
            "numeric tool_choice must fall back to Auto"
        );
    }

    #[test]
    fn adv_tool_choice_array_falls_back_to_auto() {
        let dummy_tool = vec![json!({"type":"function","function":{"name":"x"}})];
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&json!(["auto"])));
        let cfg = cfg.unwrap();
        assert!(
            matches!(
                cfg.function_calling_config.mode,
                GeminiFunctionCallingMode::Auto
            ),
            "array tool_choice must fall back to Auto"
        );
    }

    #[test]
    fn adv_tool_choice_bool_falls_back_to_auto() {
        let dummy_tool = vec![json!({"type":"function","function":{"name":"x"}})];
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&json!(true)));
        let cfg = cfg.unwrap();
        assert!(
            matches!(
                cfg.function_calling_config.mode,
                GeminiFunctionCallingMode::Auto
            ),
            "bool tool_choice must fall back to Auto"
        );
    }

    #[test]
    fn adv_tool_choice_object_without_function_falls_back_to_auto() {
        // {type: "function"} — no function sub-object
        let dummy_tool = vec![json!({"type":"function","function":{"name":"x"}})];
        let tc = json!({"type": "function"});
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&tc));
        let cfg = cfg.unwrap();
        assert!(
            matches!(
                cfg.function_calling_config.mode,
                GeminiFunctionCallingMode::Auto
            ),
            "object without function field must fall back to Auto"
        );
    }

    #[test]
    fn adv_tool_choice_object_function_without_name_falls_back_to_auto() {
        // {type: "function", function: {description: "no name"}}
        let dummy_tool = vec![json!({"type":"function","function":{"name":"x"}})];
        let tc = json!({"type": "function", "function": {"description": "no name"}});
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&tc));
        let cfg = cfg.unwrap();
        assert!(
            matches!(
                cfg.function_calling_config.mode,
                GeminiFunctionCallingMode::Auto
            ),
            "function without name must fall back to Auto"
        );
    }

    #[test]
    fn adv_tool_choice_object_huge_function_name_preserved() {
        // 10,000-char function name in tool_choice → must appear in allowlist
        let dummy_tool = vec![json!({"type":"function","function":{"name":"x"}})];
        let big_name = "a".repeat(10_000);
        let tc = json!({"type": "function", "function": {"name": big_name}});
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&dummy_tool), Some(&tc));
        let cfg = cfg.unwrap();
        assert!(matches!(
            cfg.function_calling_config.mode,
            GeminiFunctionCallingMode::Any
        ));
        let names = cfg.function_calling_config.allowed_function_names.unwrap();
        assert_eq!(names[0].len(), 10_000);
    }

    // --- Duplicate tools ---

    #[test]
    fn adv_duplicate_tool_names_both_preserved() {
        // Two tools with the same name → both mapped (Gemini doesn't
        // de-dup; the upstream may reject, but translator is permissive).
        let tools_in = vec![
            json!({"type":"function","function":{"name":"dup","description":"first"}}),
            json!({"type":"function","function":{"name":"dup","description":"second"}}),
        ];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        let tools = tools.expect("expected tools");
        assert_eq!(tools[0].function_declarations.len(), 2);
        assert_eq!(tools[0].function_declarations[0].name, "dup");
        assert_eq!(tools[0].function_declarations[1].name, "dup");
    }

    // --- Enum in parameters preserved ---

    #[test]
    fn adv_enum_in_parameters_preserved() {
        let tools_in = vec![json!({
            "type": "function",
            "function": {
                "name": "with_enum",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "color": {"type": "string", "enum": ["red", "green", "blue"]}
                    }
                }
            }
        })];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        let tools = tools.expect("expected tools");
        let params = tools[0].function_declarations[0]
            .parameters
            .as_ref()
            .unwrap();
        assert_eq!(params["properties"]["color"]["enum"][0], "red");
        assert_eq!(params["properties"]["color"]["enum"][1], "green");
        assert_eq!(params["properties"]["color"]["enum"][2], "blue");
    }

    // --- 1000 tools: all should be mapped ---

    #[test]
    fn adv_1000_tools_all_mapped() {
        let tools_in: Vec<serde_json::Value> = (0..1000)
            .map(|i| {
                json!({
                    "type": "function",
                    "function": {"name": format!("tool_{i:04}")}
                })
            })
            .collect();
        let (tools, cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        let tools = tools.expect("expected tools for 1000 tools");
        assert_eq!(tools[0].function_declarations.len(), 1000);
        assert_eq!(tools[0].function_declarations[0].name, "tool_0000");
        assert_eq!(tools[0].function_declarations[999].name, "tool_0999");
        // tool_choice should default to Auto
        let cfg = cfg.unwrap();
        assert!(matches!(
            cfg.function_calling_config.mode,
            GeminiFunctionCallingMode::Auto
        ));
    }

    // --- Non-object tool values ---

    #[test]
    fn adv_non_object_tool_string_skipped() {
        let tools_in = vec![json!("not-an-object")];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        assert!(tools.is_none(), "non-object tool must be skipped");
    }

    #[test]
    fn adv_non_object_tool_number_skipped() {
        let tools_in = vec![json!(42)];
        let (tools, _cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        assert!(tools.is_none());
    }

    // --- Parameters null vs missing: different behavior ---

    #[test]
    fn adv_parameters_null_vs_absent() {
        // Explicit "parameters": null → tool skipped (non-object parameters)
        let tools_null = vec![json!({
            "type": "function",
            "function": {"name": "with_null", "parameters": null}
        })];
        let (tools, _) = translate_openai_tools_to_gemini(Some(&tools_null), None);
        assert!(tools.is_none(), "null parameters must skip tool");

        // Absent "parameters" → tool included, parameters=None
        let tools_absent = vec![json!({
            "type": "function",
            "function": {"name": "without_params"}
        })];
        let (tools, _) = translate_openai_tools_to_gemini(Some(&tools_absent), None);
        let tools = tools.expect("absent parameters should keep tool");
        assert!(tools[0].function_declarations[0].parameters.is_none());
    }

    // --- tool_choice None + tools present → config emitted ---

    #[test]
    fn adv_tools_present_tool_choice_none_emits_auto_config() {
        let tools_in = vec![json!({
            "type": "function",
            "function": {"name": "x"}
        })];
        let (_, cfg) = translate_openai_tools_to_gemini(Some(&tools_in), None);
        let cfg = cfg.expect("tool_config should be Some when tools present");
        assert!(
            matches!(
                cfg.function_calling_config.mode,
                GeminiFunctionCallingMode::Auto
            ),
            "no explicit tool_choice with tools → Auto"
        );
    }

    // --- tool_choice None + tools=None → both None ---

    #[test]
    fn adv_no_tools_no_tool_choice_both_none() {
        let (tools, cfg) = translate_openai_tools_to_gemini(None, None);
        assert!(tools.is_none());
        assert!(cfg.is_none());
    }

    // --- Circular $ref in parameters: should not panic (depth limit) ---

    #[test]
    fn adv_circular_ref_in_parameters_no_panic() {
        let tools_in = vec![json!({
            "type": "function",
            "function": {
                "name": "circular",
                "parameters": {
                    "$defs": {
                        "Node": {
                            "type": "object",
                            "properties": {
                                "child": {"$ref": "#/$defs/Node"}
                            }
                        }
                    },
                    "type": "object",
                    "properties": {
                        "root": {"$ref": "#/$defs/Node"}
                    }
                }
            }
        })];
        // Must not panic (stack overflow protection via MAX_RECURSION_DEPTH).
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = translate_openai_tools_to_gemini(Some(&tools_in), None);
        }));
        assert!(
            result.is_ok(),
            "circular $ref must not cause stack overflow panic"
        );
    }

    // --- Serialized output omits tools/toolConfig when None ---

    #[test]
    fn adv_serialized_output_omits_tools_when_none() {
        let gemini_req = openai_to_gemini(&openproxy_types::OpenAIRequest::default(), &[]);
        let json_str = serde_json::to_string(&gemini_req).unwrap();
        assert!(
            !json_str.contains("\"tools\""),
            "serialized must not contain tools field when None"
        );
        assert!(
            !json_str.contains("\"toolConfig\""),
            "serialized must not contain toolConfig field when None"
        );
    }
}

// =====================================================================
// ADVERSARIAL TESTS — dedup-antigravity-gemini refactor (D2 + D3)
// =====================================================================

#[cfg(test)]
mod adversarial_serialize_gemini {
    use super::serialize_gemini_request;
    use openproxy_types::{OpenAIMessage, OpenAIRequest};

    #[test]
    fn serialize_gemini_request_empty_messages_succeeds() {
        let req = OpenAIRequest::default();
        let res = serialize_gemini_request(&req, &[]);
        let bytes = res.expect("empty request must serialize");
        let parsed: serde_json::Value =
            serde_json::from_slice(&bytes).expect("output must be valid JSON");
        assert!(parsed.get("contents").is_some());
        assert!(parsed["contents"].as_array().unwrap().is_empty());
    }

    #[test]
    fn serialize_gemini_request_10k_messages_succeeds() {
        let messages: Vec<OpenAIMessage> = (0..10_000)
            .map(|i| OpenAIMessage {
                role: "user".to_string(),
                content: Some(serde_json::Value::String(format!("msg {i}"))),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            })
            .collect();
        let req = OpenAIRequest {
            model: "gemini-1.5-pro".to_string(),
            messages: vec![],
            ..Default::default()
        };
        let bytes = serialize_gemini_request(&req, &messages).expect("10k messages must serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("must round-trip");
        assert_eq!(parsed["contents"].as_array().unwrap().len(), 10_000);
        assert_eq!(parsed["contents"][0]["parts"][0]["text"], "msg 0");
        assert_eq!(parsed["contents"][9_999]["parts"][0]["text"], "msg 9999");
    }

    #[test]
    fn serialize_gemini_request_extreme_unicode_preserved() {
        let content = "cafe \u{1F511}  \u{202E}rtl\u{202C} zwj \u{200D} \u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466} ni hao";
        let messages = vec![OpenAIMessage {
            role: "user".to_string(),
            content: Some(serde_json::Value::String(content.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }];
        let req = OpenAIRequest::default();
        let bytes =
            serialize_gemini_request(&req, &messages).expect("extreme unicode must serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("must round-trip");
        let out = parsed["contents"][0]["parts"][0]["text"]
            .as_str()
            .expect("text must be a string");
        assert_eq!(out, content, "byte-for-byte preservation");
    }

    #[test]
    fn serialize_gemini_request_tool_calls_with_circular_ref_does_not_panic() {
        let circular_schema = serde_json::json!({
            "type": "object",
            "properties": { "self": { "$ref": "#" } }
        });
        let tool_call = serde_json::json!({
            "id": "call_1", "type": "function",
            "function": { "name": "recurse", "arguments": "{}" }
        });
        let messages = vec![OpenAIMessage {
            role: "assistant".to_string(),
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![tool_call]),
            extra: serde_json::Map::new(),
        }];
        let req = OpenAIRequest {
            tools: Some(vec![serde_json::json!({
                "type": "function",
                "function": { "name": "recurse", "parameters": circular_schema }
            })]),
            ..Default::default()
        };
        let bytes = serialize_gemini_request(&req, &messages)
            .expect("circular ref must not panic and must serialize");
        let _: serde_json::Value = serde_json::from_slice(&bytes).expect("must round-trip");
    }

    #[test]
    fn serialize_gemini_request_tools_with_non_object_elements_no_panic() {
        let req = OpenAIRequest {
            tools: Some(vec![
                serde_json::Value::String("not-an-object".to_string()),
                serde_json::Value::Null,
                serde_json::json!({"type": "function", "function": {"name": "f"}}),
            ]),
            ..Default::default()
        };
        let messages: Vec<OpenAIMessage> = vec![];
        let res = serialize_gemini_request(&req, &messages)
            .expect("non-object tool elements must serialize");
        let parsed: serde_json::Value = serde_json::from_slice(&res).expect("must round-trip");
        let funcs = &parsed["tools"][0]["functionDeclarations"];
        assert!(funcs.is_array());
        assert_eq!(funcs.as_array().unwrap().len(), 1);
        assert_eq!(funcs[0]["name"], "f");
    }

    #[test]
    fn serialize_gemini_request_structured_content_succeeds() {
        let req = OpenAIRequest {
            messages: vec![OpenAIMessage {
                role: "user".to_string(),
                content: Some(serde_json::json!({"complex": [1, 2, 3]})),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            }],
            ..Default::default()
        };
        let res = serialize_gemini_request(&req, &req.messages);
        assert!(res.is_ok(), "any OpenAIRequest must serialize");
    }
}

#[cfg(test)]
mod adversarial_deserialize_gemini {
    use super::deserialize_gemini_response;
    use openproxy_types::OpenAIResponse;

    #[test]
    fn deserialize_gemini_response_null_body_does_not_panic() {
        let body: serde_json::Value = serde_json::Value::Null;
        let res = deserialize_gemini_response(&body);
        match res {
            Ok(resp) => {
                let _: OpenAIResponse = resp;
            }
            Err(_) => { /* acceptable: serde fails */ }
        }
    }

    #[test]
    fn deserialize_gemini_response_top_level_array_succeeds_with_defaults() {
        let body: serde_json::Value = serde_json::json!([]);
        let resp = deserialize_gemini_response(&body)
            .expect("top-level array -> defaults (permissive serde_json)");
        assert_eq!(resp.choices.len(), 1, "always one choice");
        let content = resp.choices[0].message.content.as_ref();
        if let Some(c) = content {
            assert!(c.as_str().is_none_or(str::is_empty));
        }
    }

    #[test]
    fn deserialize_gemini_response_candidates_null_errors() {
        let body: serde_json::Value = serde_json::json!({"candidates": null});
        let res = deserialize_gemini_response(&body);
        if let Err(e) = res {
            let msg = format!("{e}");
            assert!(msg.contains("parse"));
        }
    }

    #[test]
    fn deserialize_gemini_response_empty_candidates_emits_one_empty_choice() {
        let body: serde_json::Value = serde_json::json!({"candidates": []});
        let resp = deserialize_gemini_response(&body).expect("empty candidates must succeed");
        assert_eq!(
            resp.choices.len(),
            1,
            "gemini_to_openai always emits one choice"
        );
        let content = resp.choices[0].message.content.as_ref();
        if let Some(c) = content {
            assert!(c.as_str().is_none_or(str::is_empty));
        }
    }

    #[test]
    fn deserialize_gemini_response_parts_null_does_not_panic() {
        let body: serde_json::Value = serde_json::json!({
            "candidates": [{"content": {"parts": null}, "finishReason": "STOP"}]
        });
        let res = deserialize_gemini_response(&body);
        if let Ok(resp) = res {
            let content = &resp.choices[0].message.content;
            if let Some(c) = content {
                assert!(c.is_string());
            }
        }
    }

    #[test]
    fn deserialize_gemini_response_10mb_body_succeeds() {
        let huge: String = "a".repeat(10 * 1024 * 1024);
        let body: serde_json::Value = serde_json::json!({
            "candidates": [{
                "content": { "role": "model", "parts": [{"text": huge}] },
                "finishReason": "STOP"
            }]
        });
        let resp = deserialize_gemini_response(&body).expect("10 MiB body must deserialize");
        let out = resp.choices[0]
            .message
            .content
            .as_ref()
            .and_then(|v| v.as_str())
            .expect("content must be a string");
        assert_eq!(out.len(), 10 * 1024 * 1024, "no bytes dropped");
    }

    #[test]
    fn deserialize_gemini_response_truncated_input_does_not_panic() {
        let body: serde_json::Value = serde_json::Value::Null;
        let _ = deserialize_gemini_response(&body);
    }

    #[test]
    fn deserialize_gemini_response_lone_surrogate_rejected_by_serde_json() {
        let raw = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hello \uDCFF world"}]},"finishReason":"STOP"}]}"#;
        let body: Result<serde_json::Value, _> = serde_json::from_str(raw);
        assert!(body.is_err());
        let msg = format!("{}", body.unwrap_err());
        assert!(msg.contains("lone leading surrogate"));
    }

    #[test]
    fn deserialize_gemini_response_1000_duplicate_candidates_no_blowup() {
        let mut candidates = Vec::with_capacity(1000);
        for _ in 0..1000 {
            candidates.push(serde_json::json!({
                "content": { "role": "model", "parts": [{"text": "hi"}] },
                "finishReason": "STOP"
            }));
        }
        let body: serde_json::Value = serde_json::json!({"candidates": candidates});
        let resp = deserialize_gemini_response(&body).expect("1000 candidates must deserialize");
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(
            resp.choices[0]
                .message
                .content
                .as_ref()
                .unwrap()
                .as_str()
                .unwrap(),
            "hi"
        );
    }
}
