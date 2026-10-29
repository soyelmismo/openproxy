use std::borrow::Cow;

pub const DEFAULT_GEMINI_MAX_OUTPUT_TOKENS: u32 = 8192;
pub const SENTINEL_SIGNATURE: &str = "skip_thought_signature_validator";

pub fn is_real_signature(sig: &str) -> bool {
    sig.len() >= 50 && sig != SENTINEL_SIGNATURE
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiSafetySetting {
    pub category: Cow<'static, str>,
    pub threshold: Cow<'static, str>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiRequest {
    pub contents: Vec<GeminiContent>,
    #[serde(
        default,
        rename = "systemInstruction",
        skip_serializing_if = "Option::is_none"
    )]
    pub system_instruction: Option<GeminiContent>,
    #[serde(
        default,
        rename = "generationConfig",
        skip_serializing_if = "Option::is_none"
    )]
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
    #[serde(rename = "mimeType")]
    pub mime_type: String,
    pub data: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct GeminiFunctionCall {
    pub name: String,
    #[serde(default)]
    pub args: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct GeminiFunctionResponse {
    pub name: String,
    pub response: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct GeminiPart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thought: Option<bool>,
    #[serde(
        default,
        rename = "thoughtSignature",
        skip_serializing_if = "Option::is_none"
    )]
    pub thought_signature: Option<String>,
    #[serde(
        default,
        rename = "inlineData",
        skip_serializing_if = "Option::is_none"
    )]
    pub inline_data: Option<GeminiInlineData>,
    #[serde(
        default,
        rename = "functionCall",
        skip_serializing_if = "Option::is_none"
    )]
    pub function_call: Option<GeminiFunctionCall>,
    #[serde(
        default,
        rename = "functionResponse",
        skip_serializing_if = "Option::is_none"
    )]
    pub function_response: Option<GeminiFunctionResponse>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiThinkingConfig {
    #[serde(rename = "thinkingBudget")]
    pub thinking_budget: i32,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiGenerationConfig {
    #[serde(
        default,
        rename = "maxOutputTokens",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(
        default,
        rename = "stopSequences",
        skip_serializing_if = "Option::is_none"
    )]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(
        default,
        rename = "thinkingConfig",
        skip_serializing_if = "Option::is_none"
    )]
    pub thinking_config: Option<GeminiThinkingConfig>,
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
    #[serde(
        default,
        alias = "usageMetadata",
        skip_serializing_if = "Option::is_none"
    )]
    pub usage_metadata: Option<GeminiUsageMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<GeminiInnerResponse>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiInnerResponse {
    #[serde(default)]
    pub candidates: Vec<GeminiCandidate>,
    #[serde(
        default,
        alias = "usageMetadata",
        skip_serializing_if = "Option::is_none"
    )]
    pub usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiCandidate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<GeminiContent>,
    #[serde(
        default,
        alias = "finishReason",
        skip_serializing_if = "Option::is_none"
    )]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct GeminiUsageMetadata {
    #[serde(default, alias = "promptTokenCount")]
    pub prompt_token_count: u32,
    #[serde(default, alias = "candidatesTokenCount")]
    pub candidates_token_count: u32,
    #[serde(default, alias = "totalTokenCount")]
    pub total_token_count: u32,
    #[serde(rename = "cachedContentTokenCount", default)]
    pub cached_content_token_count: Option<u32>,
}
