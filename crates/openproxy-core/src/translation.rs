//! Translation between OpenAI Chat Completions and Anthropic/Gemini formats.
//! Also includes streaming SSE translation for Anthropic -> OpenAI.

use crate::error::{CoreError, Result};
use serde::de::Deserializer;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

// =====================
// OpenAI Chat Completions types (input/output)
// =====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIRequest {
    pub model: String,
    pub messages: Vec<OpenAIMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
    // H4 fix: function-calling fields. None of these are translated
    // by the openai_to_anthropic boundary yet, which means a caller
    // that asks for `tools=[...]` on the OpenAI chat endpoint and
    // is routed to an Anthropic upstream silently loses the tool
    // declarations — the model then has no way to know which tools
    // exist, and any tool_use response is rejected because the
    // tool definition is missing. Adding typed fields here forces
    // the translator to surface them; serde keeps the unknown-
    // key tolerance of `extra` for callers using newer/draft
    // field names that haven't been added here yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

fn deserialize_optional_content<'de, D>(deserializer: D) -> std::result::Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIMessage {
    /// "system" | "user" | "assistant" | "tool" | "function" | "developer"
    pub role: String,
    #[serde(default, deserialize_with = "deserialize_optional_content")]
    pub content: Option<serde_json::Value>,
    /// Optional `name` for the speaker (used by a few multi-user
    /// prompts). Serialized only when `Some` — emitting `"name": null`
    /// would trip up some upstream validators (notably OpenRouter's
    /// Nemotron path, which uses the OpenAI Python SDK v1.x Pydantic
    /// validator and resolves the discriminated
    /// `ChatCompletionMessageParam` union to the `developer` variant
    /// when a `name` key is present with a `null` value, then
    /// complains the role is not `"developer"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<serde_json::Value>>,
    #[serde(default, flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIResponse {
    pub id: String,
    /// "chat.completion"
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<OpenAIChoice>,
    pub usage: Option<OpenAIUsage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIChoice {
    pub index: u32,
    pub message: OpenAIMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

// =====================
// Anthropic Messages types
// =====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicRequest {
    pub model: String,
    pub messages: Vec<AnthropicMessage>,
    pub max_tokens: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    #[serde(default)]
    pub stream: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessage {
    /// "user" | "assistant"
    pub role: String,
    /// Anthropic accepts `content` as either a plain string
    /// (`"content": "text"`) or an array of typed content blocks
    /// (`"content": [{"type":"text","text":"..."},
    /// {"type":"tool_use","id":"...","name":"...","input":{...}},
    /// {"type":"tool_result","tool_use_id":"...","content":"..."}]`).
    /// We use `serde_json::Value` so the translator can emit either
    /// form depending on whether the source OpenAI message carried
    /// only text or also carried `tool_calls` / was a `tool`-role
    /// message.
    pub content: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicResponse {
    pub id: String,
    /// "message"
    #[serde(rename = "type")]
    pub response_type: String,
    /// "assistant"
    pub role: String,
    pub content: Vec<AnthropicContentBlock>,
    pub model: String,
    #[serde(default)]
    pub stop_reason: Option<String>,
    pub usage: AnthropicUsage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicContentBlock {
    /// "text"
    #[serde(rename = "type")]
    pub block_type: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Default max_tokens used when the client doesn't provide one. Anthropic requires it.
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

/// Default max_output_tokens for Gemini when the client doesn't provide one.
pub const DEFAULT_GEMINI_MAX_OUTPUT_TOKENS: u32 = 8192;

// =====================
// Gemini types
// =====================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiRequest {
    pub contents: Vec<GeminiContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GeminiContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GeminiGenerationConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiContent {
    pub role: String,
    pub parts: Vec<GeminiPart>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiPart {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiResponse {
    #[serde(default)]
    pub candidates: Vec<GeminiCandidate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiCandidate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<GeminiContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeminiUsageMetadata {
    #[serde(default)]
    pub prompt_token_count: u32,
    #[serde(default)]
    pub candidates_token_count: u32,
    #[serde(default)]
    pub total_token_count: u32,
}

// =====================
// Conversion functions
// =====================

fn message_content_to_text(content: &Option<serde_json::Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(openai_content_part_to_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(""),
        // `null` content (common when an assistant message carries only
        // `tool_calls` and no text) must be treated as empty, NOT as the
        // string "null" — otherwise the translator would emit a spurious
        // `{"type":"text","text":"null"}` block before the tool_use
        // blocks, which confuses Anthropic-compatible upstreams.
        Some(Value::Null) | None => String::new(),
        Some(value) => value.to_string(),
    }
}

fn openai_content_part_to_text(part: &serde_json::Value) -> String {
    if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
        return text.to_string();
    }

    if let Some(content) = part.get("content").and_then(|v| v.as_str()) {
        return content.to_string();
    }

    match part {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn message_content_to_gemini_parts(content: &Option<serde_json::Value>) -> Vec<GeminiPart> {
    match content {
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| GeminiPart {
                text: Some(openai_content_part_to_text(part)),
            })
            .collect(),
        Some(Value::Null) => vec![GeminiPart { text: Some(String::new()) }],
        Some(value) => vec![GeminiPart {
            text: Some(value.to_string()),
        }],
        None => vec![GeminiPart { text: Some(String::new()) }],
    }
}

/// Convert OpenAI request to Anthropic request.
///
/// - Extracts system messages (role=system) and joins them with "\n\n" into
///   `AnthropicRequest.system`.
/// - Remaining messages with role=user|assistant go into `AnthropicRequest.messages`.
/// - Assistant messages with `tool_calls` are translated to Anthropic
///   `tool_use` content blocks (the text content, if any, becomes a `text`
///   block preceding the `tool_use` blocks).
/// - `tool`-role messages (OpenAI's tool-result format) are translated to
///   Anthropic `tool_result` content blocks under a `user`-role message
///   (Anthropic has no `tool` role; tool results are sent as user messages
///   with `tool_result` blocks).
/// - `function`-role messages are dropped (legacy OpenAI format, no
///   Anthropic equivalent without a function name).
/// - `max_tokens` is required by Anthropic; defaults to [`DEFAULT_MAX_TOKENS`] when absent.
/// - `tools` (OpenAI shape: `[{type:"function",function:{name,description,parameters}}]`)
///   are translated to Anthropic shape (`[{name,description,input_schema}]`).
///   Tools with empty `name` are filtered out (MiniMax rejects with `(2013)`
///   "function name or parameters is empty").
/// - `tool_choice` (OpenAI shape: `"auto"/"none"/"required"` or
///   `{type:"function",function:{name}}`) is translated to Anthropic shape
///   (`{type:"auto"/"none"/"any"/"tool",name?}`).
pub fn openai_to_anthropic(req: &OpenAIRequest) -> AnthropicRequest {
    let mut system_parts: Vec<String> = Vec::new();
    let mut conversation: Vec<AnthropicMessage> = Vec::with_capacity(req.messages.len());

    for m in &req.messages {
        match m.role.as_str() {
            "system" => system_parts.push(message_content_to_text(&m.content)),
            "assistant" => {
                // If the assistant message has `tool_calls`, emit a
                // content-blocks array with optional text + tool_use
                // blocks. Anthropic requires tool_use blocks to carry
                // both `id` and `name` and a valid JSON `input` —
                // MiniMax rejects with `(2013)` otherwise.
                if let Some(tool_calls) = m.tool_calls.as_ref() {
                    let mut blocks: Vec<serde_json::Value> = Vec::new();
                    let text = message_content_to_text(&m.content);
                    if !text.is_empty() {
                        blocks.push(json!({"type": "text", "text": text}));
                    }
                    for tc in tool_calls {
                        let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let function = tc.get("function");
                        let name = function
                            .and_then(|f| f.get("name"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let arguments_str = function
                            .and_then(|f| f.get("arguments"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        // Parse arguments string to a JSON object for
                        // Anthropic's `input` field. Empty or invalid
                        // arguments become an empty object (Anthropic
                        // requires `input` to be present, even if empty).
                        let input: serde_json::Value = if arguments_str.is_empty() {
                            json!({})
                        } else {
                            serde_json::from_str(arguments_str).unwrap_or(json!({}))
                        };
                        // Skip tool_calls with empty name — they would
                        // trigger MiniMax's `(2013)` rejection.
                        if name.is_empty() {
                            continue;
                        }
                        blocks.push(json!({
                            "type": "tool_use",
                            "id": id,
                            "name": name,
                            "input": input,
                        }));
                    }
                    // If we ended up with no blocks (e.g. all tool_calls
                    // had empty names), fall back to a single text block
                    // so the message is non-empty (Anthropic rejects
                    // empty content arrays).
                    if blocks.is_empty() {
                        blocks.push(json!({"type": "text", "text": ""}));
                    }
                    conversation.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: serde_json::Value::Array(blocks),
                    });
                } else {
                    // Plain text assistant message — use the string
                    // form of `content` (cheaper than a single-element
                    // array, and matches Anthropic's canonical shape
                    // for text-only messages).
                    let text = message_content_to_text(&m.content);
                    conversation.push(AnthropicMessage {
                        role: "assistant".to_string(),
                        content: serde_json::Value::String(text),
                    });
                }
            }
            "user" => {
                // Plain user message — string form.
                let text = message_content_to_text(&m.content);
                conversation.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: serde_json::Value::String(text),
                });
            }
            "tool" => {
                // OpenAI `tool`-role message: a tool result. Translate
                // to Anthropic's `tool_result` content block under a
                // `user`-role message (Anthropic has no `tool` role).
                // The `tool_call_id` field carries the id of the
                // assistant's tool_use block this result responds to.
                let tool_use_id = m.tool_call_id.as_deref().unwrap_or("");
                let content_text = message_content_to_text(&m.content);
                conversation.push(AnthropicMessage {
                    role: "user".to_string(),
                    content: json!([{
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": content_text,
                    }]),
                });
            }
            // Unknown roles (function, developer, etc.) are ignored
            // at the translation boundary.
            _ => {}
        }
    }

    let system = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    AnthropicRequest {
        model: req.model.clone(),
        messages: conversation,
        max_tokens: req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
        system,
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: req.top_k,
        stop_sequences: req.stop.clone(),
        // Translate OpenAI-shaped `tools` to Anthropic shape. MiniMax
        // (which exposes an Anthropic-compatible API) and real Anthropic
        // both expect `{name, description, input_schema}` — forwarding
        // the OpenAI shape `{type:"function", function:{name, parameters}}`
        // verbatim causes `(2013) function name or parameters is empty`
        // because the upstream looks for `name`/`input_schema` at the
        // top level and finds them missing.
        tools: req.tools.as_ref().map(|tools| {
            tools
                .iter()
                .filter_map(|t| translate_openai_tool_to_anthropic(t))
                .collect::<Vec<_>>()
                .into()
        }).filter(|t: &Vec<serde_json::Value>| !t.is_empty()),
        // Translate OpenAI `tool_choice` to Anthropic shape.
        tool_choice: req.tool_choice.as_ref().and_then(translate_openai_tool_choice_to_anthropic),
        // OpenAI's `user` field maps to Anthropic's `metadata.user_id`
        // (Anthropic reserves metadata for traceability, not for
        // function-calling). When the caller didn't set `user`, we
        // leave metadata None rather than synthesise an empty object.
        metadata: req.user.as_ref().map(|u| {
            serde_json::json!({ "user_id": u })
        }),
        stream: req.stream,
    }
}

/// Translate a single OpenAI-shaped tool definition to Anthropic shape.
///
/// OpenAI: `{"type":"function","function":{"name":"X","description":"Y","parameters":{...}}}`
/// Anthropic: `{"name":"X","description":"Y","input_schema":{...}}`
///
/// Returns `None` when the tool has no `name` or no `function` block —
/// MiniMax rejects tools with empty names with `(2013)`.
fn translate_openai_tool_to_anthropic(tool: &serde_json::Value) -> Option<serde_json::Value> {
    let function = tool.get("function")?;
    let name = function.get("name").and_then(|v| v.as_str())?;
    if name.is_empty() {
        return None;
    }
    let description = function.get("description").and_then(|v| v.as_str());
    // `parameters` (OpenAI) → `input_schema` (Anthropic). Default to
    // an empty object when absent — Anthropic requires `input_schema`
    // to be present and a valid JSON schema object.
    let input_schema = function.get("parameters").cloned().unwrap_or(json!({}));
    Some(json!({
        "name": name,
        "description": description,
        "input_schema": input_schema,
    }))
}

/// Translate OpenAI `tool_choice` to Anthropic `tool_choice`.
///
/// OpenAI shapes:
///   - `"auto"` / `"none"` / `"required"` (string)
///   - `{"type":"function","function":{"name":"X"}}` (object)
///   - `{"type":"auto"}` / `{"type":"none"}` (object form of the strings)
///
/// Anthropic shapes:
///   - `{"type":"auto"}` (let model decide)
///   - `{"type":"none"}` (don't use tools)
///   - `{"type":"any"}` (force a tool call — OpenAI's "required")
///   - `{"type":"tool","name":"X"}` (force a specific tool)
///
/// Returns `None` for unrecognized shapes (which means the field is
/// omitted from the Anthropic request, defaulting to `auto` upstream).
fn translate_openai_tool_choice_to_anthropic(tc: &serde_json::Value) -> Option<serde_json::Value> {
    match tc {
        serde_json::Value::String(s) => match s.as_str() {
            "auto" => Some(json!({"type": "auto"})),
            "none" => Some(json!({"type": "none"})),
            "required" => Some(json!({"type": "any"})),
            _ => None,
        },
        serde_json::Value::Object(obj) => {
            let ty = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match ty {
                "auto" => Some(json!({"type": "auto"})),
                "none" => Some(json!({"type": "none"})),
                "required" => Some(json!({"type": "any"})),
                "function" => {
                    // OpenAI object form: {"type":"function","function":{"name":"X"}}
                    // → Anthropic: {"type":"tool","name":"X"}
                    let name = obj.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())?;
                    if name.is_empty() {
                        return None;
                    }
                    Some(json!({"type": "tool", "name": name}))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

/// Convert Anthropic response to OpenAI response.
///
/// - `choices[0].message.content` = concatenation of all text content blocks.
/// - `usage`: `prompt_tokens=input_tokens`, `completion_tokens=output_tokens`,
///   `total_tokens=sum`.
/// - `finish_reason` mapped from `stop_reason` using Anthropic -> OpenAI semantics.
pub fn anthropic_to_openai(resp: &AnthropicResponse) -> OpenAIResponse {
    let combined: String = resp
        .content
        .iter()
        .filter(|b| b.block_type == "text")
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("");

    let prompt_tokens = resp.usage.input_tokens;
    let completion_tokens = resp.usage.output_tokens;
    let total_tokens = prompt_tokens.saturating_add(completion_tokens);

    let message = OpenAIMessage {
        role: "assistant".to_string(),
        content: Some(Value::String(combined)),
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extra: serde_json::Map::new(),
    };

    let choice = OpenAIChoice {
        index: 0,
        message,
        finish_reason: resp.stop_reason.as_deref().map(map_finish_reason),
    };

    OpenAIResponse {
        id: resp.id.clone(),
        object: "chat.completion".to_string(),
        created: 0,
        model: resp.model.clone(),
        choices: vec![choice],
        usage: Some(OpenAIUsage {
            prompt_tokens,
            completion_tokens,
            total_tokens,
        }),
    }
}

/// Map an Anthropic stop_reason value to an OpenAI finish_reason value.
fn map_finish_reason(stop_reason: &str) -> String {
    match stop_reason {
        "end_turn" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "tool_use" => "tool_calls".to_string(),
        // stop_sequence and unknown values fall back to "stop".
        other => {
            // Treat anything unknown as "stop" to stay close to OpenAI's vocabulary.
            let _ = other;
            "stop".to_string()
        }
    }
}

// =====================
// Gemini conversion functions
// =====================

/// Convert OpenAI request to Gemini request.
///
/// - System messages → `system_instruction` (Gemini 1.5+).
/// - User messages → `contents` with role "user".
/// - Assistant messages → `contents` with role "model".
/// - `max_tokens` → `generation_config.max_output_tokens`.
/// - `temperature` → `generation_config.temperature`.
/// - `top_p` → `generation_config.top_p`.
/// - `stop` → `generation_config.stop_sequences`.
pub fn openai_to_gemini(req: &OpenAIRequest) -> GeminiRequest {
    let mut system_parts: Vec<String> = Vec::new();
    let mut contents: Vec<GeminiContent> = Vec::with_capacity(req.messages.len());

    for m in &req.messages {
        match m.role.as_str() {
            "system" => system_parts.push(message_content_to_text(&m.content)),
            "user" => {
                contents.push(GeminiContent {
                    role: "user".to_string(),
                    parts: message_content_to_gemini_parts(&m.content),
                });
            }
            "assistant" => {
                contents.push(GeminiContent {
                    role: "model".to_string(),
                    parts: message_content_to_gemini_parts(&m.content),
                });
            }
            // Unknown roles are ignored at the translation boundary.
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
            }],
        })
    };

    let generation_config = GeminiGenerationConfig {
        max_output_tokens: req.max_tokens.or(Some(DEFAULT_GEMINI_MAX_OUTPUT_TOKENS)),
        temperature: req.temperature,
        top_p: req.top_p,
        stop_sequences: req.stop.clone(),
    };

    GeminiRequest {
        contents,
        system_instruction,
        generation_config: Some(generation_config),
    }
}

/// Map a Gemini finish_reason to an OpenAI finish_reason.
fn map_gemini_finish_reason(reason: &str) -> String {
    match reason {
        "STOP" => "stop".to_string(),
        "MAX_TOKENS" => "length".to_string(),
        "SAFETY" | "RECITATION" | "BLOCKLIST" => "content_filter".to_string(),
        other => {
            let _ = other;
            "stop".to_string()
        }
    }
}

/// Convert Gemini response to OpenAI response.
///
/// - `candidates[0].content.parts[0].text` → `choices[0].message.content`.
/// - `usage_metadata.prompt_token_count` → `usage.prompt_tokens`.
/// - `usage_metadata.candidates_token_count` → `usage.completion_tokens`.
/// - `usage_metadata.total_token_count` → `usage.total_tokens`.
pub fn gemini_to_openai(resp: &GeminiResponse) -> OpenAIResponse {
    let candidate = resp.candidates.first();

    let content = candidate
        .and_then(|c| c.content.as_ref())
        .and_then(|c| {
            let text: String = c
                .parts
                .iter()
                .filter_map(|p| p.text.as_deref())
                .collect();
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        })
    .unwrap_or_default();

    let finish_reason = candidate
        .and_then(|c| c.finish_reason.as_deref())
        .map(map_gemini_finish_reason);

    let usage = resp.usage_metadata.as_ref().map(|u| OpenAIUsage {
        prompt_tokens: u.prompt_token_count,
        completion_tokens: u.candidates_token_count,
        total_tokens: u.total_token_count,
    });

    OpenAIResponse {
        id: format!("gemini-{}", chrono::Utc::now().timestamp_millis()),
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: String::new(),
        choices: vec![OpenAIChoice {
            index: 0,
                message: OpenAIMessage {
                    role: "assistant".to_string(),
                    content: Some(Value::String(content)),
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

// =====================
// Anthropic SSE event types
// =====================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AnthropicSseEvent {
    MessageStart {
        message: AnthropicResponse,
    },
    ContentBlockStart {
        index: u32,
        content_block: AnthropicContentBlock,
    },
    ContentBlockDelta {
        index: u32,
        /// {type: "text_delta", text: "..."}
        delta: serde_json::Value,
    },
    ContentBlockStop {
        index: u32,
    },
    MessageDelta {
        /// Contains stop_reason.
        delta: serde_json::Value,
        usage: Option<AnthropicUsage>,
    },
    MessageStop,
    Ping,
}

/// Convert a single Anthropic SSE event to zero or more OpenAI SSE chunks.
///
/// Each returned string is a full SSE frame: `data: {json}\n\n`. Returns an
/// empty `Vec` if the event should be skipped (e.g. `ping`).
///
/// `chunk_id`, `created`, and `model` are taken from the outer response context
/// since Anthropic events don't repeat them on every frame.
pub fn anthropic_sse_to_openai_chunks(
    event: &AnthropicSseEvent,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Vec<String> {
    match event {
        AnthropicSseEvent::Ping => Vec::new(),

        AnthropicSseEvent::MessageStart { .. } => {
            // Emit a role-only chunk so clients can start streaming immediately.
            let chunk = serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": { "role": "assistant" },
                    "finish_reason": serde_json::Value::Null
                }]
            });
            vec![format_sse_data(&chunk)]
        }

        AnthropicSseEvent::ContentBlockStart { .. } | AnthropicSseEvent::ContentBlockStop { .. } => {
            // No-op boundaries: text is delivered through deltas only.
            Vec::new()
        }

        AnthropicSseEvent::ContentBlockDelta { delta, .. } => {
            // Extract the text fragment if the delta is a text_delta.
            let text = delta
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default();

            let chunk = serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": { "content": text },
                    "finish_reason": serde_json::Value::Null
                }]
            });
            vec![format_sse_data(&chunk)]
        }

        AnthropicSseEvent::MessageDelta { delta, .. } => {
            // The delta carries stop_reason. Forward it as a finish_reason chunk.
            let stop_reason = delta
                .get("stop_reason")
                .and_then(|v| v.as_str())
                .map(map_finish_reason);

            let choice = match stop_reason {
                Some(reason) => json!({
                    "index": 0,
                    "delta": {},
                    "finish_reason": reason,
                }),
                None => json!({
                    "index": 0,
                    "delta": {},
                    "finish_reason": serde_json::Value::Null,
                }),
            };

            let chunk = serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [choice],
            });
            vec![format_sse_data(&chunk)]
        }

        AnthropicSseEvent::MessageStop => {
            // Final terminator. We send a chunk with finish_reason=stop and the
            // [DONE] sentinel so both common client patterns work.
            let chunk = serde_json::json!({
                "id": chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {},
                    "finish_reason": "stop"
                }]
            });
            vec![format_sse_data(&chunk), "data: [DONE]\n\n".to_string()]
        }
    }
}

/// Parse a raw SSE `data:` line (with or without the `data: ` prefix) into an
/// [`AnthropicSseEvent`]. Returns:
///
/// - `Ok(Some(event))` for valid event payloads.
/// - `Ok(None)` for lines that should be ignored (ping, comments, empty payload,
///   non-`data:` lines, `[DONE]` sentinel).
/// - `Err(CoreError::Parse(_))` for malformed JSON or event payload that should
///   be a valid event.
pub fn parse_anthropic_sse_line(line: &str) -> Result<Option<AnthropicSseEvent>> {
    let trimmed = line.trim_end_matches(['\r', '\n']);

    // Empty / comment lines are ignored.
    if trimmed.is_empty() || trimmed.starts_with(':') {
        return Ok(None);
    }

    // We're only interested in the `data:` field. Other SSE fields (event:, id:, ...)
    // are accepted for forward compatibility but ignored here.
    let payload = match trimmed.strip_prefix("data:") {
        Some(rest) => rest.trim_start(),
        None => return Ok(None),
    };

    if payload.is_empty() {
        return Ok(None);
    }

    // The OpenAI-style [DONE] sentinel is sometimes emitted by intermediate proxies.
    if payload == "[DONE]" {
        return Ok(None);
    }

    // Probe the JSON for the discriminator. A "ping" event from Anthropic
    // must be ignored, not surfaced as a parse error.
    let probe: serde_json::Value = serde_json::from_str(payload)
        .map_err(|e| CoreError::Parse(format!("invalid SSE JSON: {e}")))?;

    if let Some(t) = probe.get("type").and_then(|v| v.as_str()) {
        if t == "ping" {
            return Ok(None);
        }
    }

    let event: AnthropicSseEvent = serde_json::from_value(probe)
        .map_err(|e| CoreError::Parse(format!("invalid Anthropic SSE event: {e}")))?;

    Ok(Some(event))
}

fn format_sse_data(payload: &serde_json::Value) -> String {
    format!("data: {}\n\n", payload)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn openai_req_with(messages: Vec<(&str, &str)>) -> OpenAIRequest {
        OpenAIRequest {
            model: "claude-test".to_string(),
            messages: messages
                .into_iter()
                .map(|(role, content)| OpenAIMessage {
                    role: role.to_string(),
                    content: Some(Value::String(content.to_string())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: serde_json::Map::new(),
                })
                .collect(),
            stream: false,
            temperature: Some(0.5),
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn openai_to_anthropic_extracts_system() {
        let req = openai_req_with(vec![
            ("system", "You are helpful."),
            ("system", "Be concise."),
            ("user", "Hi"),
            ("assistant", "Hello!"),
        ]);

        let out = openai_to_anthropic(&req);
        assert_eq!(out.system.as_deref(), Some("You are helpful.\n\nBe concise."));
        assert_eq!(out.messages.len(), 2);
        assert_eq!(out.messages[0].role, "user");
        assert_eq!(out.messages[0].content, "Hi");
        assert_eq!(out.messages[1].role, "assistant");
        assert_eq!(out.messages[1].content, "Hello!");
    }

    #[test]
    fn openai_to_anthropic_no_system() {
        let req = openai_req_with(vec![("user", "Hi"), ("assistant", "Hello!")]);
        let out = openai_to_anthropic(&req);
        assert!(out.system.is_none());
        assert_eq!(out.messages.len(), 2);
    }

    #[test]
    fn openai_to_anthropic_default_max_tokens() {
        let mut req = openai_req_with(vec![("user", "Hi")]);
        req.max_tokens = None;
        let out = openai_to_anthropic(&req);
        assert_eq!(out.max_tokens, DEFAULT_MAX_TOKENS);

        // When the client does provide max_tokens, it's preserved.
        let mut req = openai_req_with(vec![("user", "Hi")]);
        req.max_tokens = Some(123);
        let out = openai_to_anthropic(&req);
        assert_eq!(out.max_tokens, 123);
    }

    #[test]
    fn anthropic_to_openai_concat_text_blocks() {
        let resp = AnthropicResponse {
            id: "msg_1".to_string(),
            response_type: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![
                AnthropicContentBlock {
                    block_type: "text".to_string(),
                    text: "Hello, ".to_string(),
                },
                AnthropicContentBlock {
                    block_type: "text".to_string(),
                    text: "world!".to_string(),
                },
            ],
            model: "claude-test".to_string(),
            stop_reason: Some("end_turn".to_string()),
            usage: AnthropicUsage {
                input_tokens: 10,
                output_tokens: 5,
            },
        };

        let out = anthropic_to_openai(&resp);
        assert_eq!(out.id, "msg_1");
        assert_eq!(out.object, "chat.completion");
        assert_eq!(out.model, "claude-test");
        assert_eq!(out.choices.len(), 1);
        assert_eq!(out.choices[0].index, 0);
        assert_eq!(out.choices[0].message.role, "assistant");
        assert_eq!(out.choices[0].message.content.as_ref().and_then(Value::as_str), Some("Hello, world!"));
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("stop"));
    }

    #[test]
    fn anthropic_to_openai_maps_usage() {
        let resp = AnthropicResponse {
            id: "msg_1".to_string(),
            response_type: "message".to_string(),
            role: "assistant".to_string(),
            content: vec![AnthropicContentBlock {
                block_type: "text".to_string(),
                text: "ok".to_string(),
            }],
            model: "claude-test".to_string(),
            stop_reason: Some("max_tokens".to_string()),
            usage: AnthropicUsage {
                input_tokens: 7,
                output_tokens: 11,
            },
        };

        let out = anthropic_to_openai(&resp);
        let usage = out.usage.expect("usage should be present");
        assert_eq!(usage.prompt_tokens, 7);
        assert_eq!(usage.completion_tokens, 11);
        assert_eq!(usage.total_tokens, 18);
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn parse_anthropic_sse_line_text_delta() {
        let line = r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#;
        let event = parse_anthropic_sse_line(line)
            .expect("parse ok")
            .expect("event present");
        match event {
            AnthropicSseEvent::ContentBlockDelta { index, delta } => {
                assert_eq!(index, 0);
                assert_eq!(delta.get("text").and_then(|v| v.as_str()), Some("hi"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn parse_anthropic_sse_line_message_start() {
        let line = r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","content":[],"model":"claude-test","stop_reason":null,"usage":{"input_tokens":1,"output_tokens":0}}}"#;
        let event = parse_anthropic_sse_line(line)
            .expect("parse ok")
            .expect("event present");
        match event {
            AnthropicSseEvent::MessageStart { message } => {
                assert_eq!(message.id, "msg_1");
                assert_eq!(message.model, "claude-test");
                assert_eq!(message.usage.input_tokens, 1);
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn parse_anthropic_sse_line_ignores_ping() {
        let line = r#"data: {"type":"ping"}"#;
        let res = parse_anthropic_sse_line(line).expect("parse ok");
        assert!(res.is_none());
    }

    #[test]
    fn anthropic_sse_to_openai_text_delta_produces_chunk() {
        let event = AnthropicSseEvent::ContentBlockDelta {
            index: 0,
            delta: json!({ "type": "text_delta", "text": "hello" }),
        };
        let chunks = anthropic_sse_to_openai_chunks(&event, "chunk-1", 1700000000, "claude-test");
        assert_eq!(chunks.len(), 1);

        let payload = chunks[0]
            .trim_start_matches("data: ")
            .trim_end()
            .trim_end_matches('\n');
        let v: serde_json::Value = serde_json::from_str(payload).expect("valid json");
        assert_eq!(v["id"], "chunk-1");
        assert_eq!(v["object"], "chat.completion.chunk");
        assert_eq!(v["created"], 1700000000u64);
        assert_eq!(v["model"], "claude-test");
        assert_eq!(v["choices"][0]["index"], 0);
        assert_eq!(v["choices"][0]["delta"]["content"], "hello");
        assert!(v["choices"][0]["finish_reason"].is_null());
    }

    #[test]
    fn anthropic_sse_to_openai_message_stop_produces_done() {
        let event = AnthropicSseEvent::MessageStop;
        let chunks = anthropic_sse_to_openai_chunks(&event, "chunk-1", 1700000000, "claude-test");

        // Last frame is the [DONE] sentinel.
        assert_eq!(chunks.last().map(String::as_str), Some("data: [DONE]\n\n"));

        // Some preceding chunk carries finish_reason=stop.
        let has_stop = chunks.iter().any(|c| {
            let payload = c
                .trim_start_matches("data: ")
                .trim_end()
                .trim_end_matches('\n');
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                v["choices"][0]["finish_reason"] == "stop"
            } else {
                false
            }
        });
        assert!(has_stop, "expected a chunk with finish_reason=stop");
    }

    // ---- Gemini -------------------------------------------------------

    #[test]
    fn openai_to_gemini_extracts_system() {
        let req = openai_req_with(vec![
            ("system", "You are helpful."),
            ("system", "Be concise."),
            ("user", "Hi"),
            ("assistant", "Hello!"),
        ]);

        let out = openai_to_gemini(&req);
        let sys = out.system_instruction.as_ref().unwrap();
        assert_eq!(sys.role, "system");
        let text = sys.parts[0].text.as_ref().unwrap();
        assert_eq!(text, "You are helpful.\n\nBe concise.");
        assert_eq!(out.contents.len(), 2);
        assert_eq!(out.contents[0].role, "user");
        assert_eq!(out.contents[1].role, "model");
    }

    #[test]
    fn openai_to_gemini_no_system() {
        let req = openai_req_with(vec![("user", "Hi"), ("assistant", "Hello!")]);
        let out = openai_to_gemini(&req);
        assert!(out.system_instruction.is_none());
        assert_eq!(out.contents.len(), 2);
    }

    #[test]
    fn openai_to_gemini_default_max_output_tokens() {
        let mut req = openai_req_with(vec![("user", "Hi")]);
        req.max_tokens = None;
        let out = openai_to_gemini(&req);
        let gen = out.generation_config.as_ref().unwrap();
        assert_eq!(gen.max_output_tokens, Some(DEFAULT_GEMINI_MAX_OUTPUT_TOKENS));

        // When the client does provide max_tokens, it's preserved.
        let mut req = openai_req_with(vec![("user", "Hi")]);
        req.max_tokens = Some(123);
        let out = openai_to_gemini(&req);
        let gen = out.generation_config.as_ref().unwrap();
        assert_eq!(gen.max_output_tokens, Some(123));
    }

    #[test]
    fn openai_to_gemini_temperature_and_top_p() {
        let mut req = openai_req_with(vec![("user", "Hi")]);
        req.temperature = Some(0.7);
        req.top_p = Some(0.9);
        let out = openai_to_gemini(&req);
        let gen = out.generation_config.as_ref().unwrap();
        assert_eq!(gen.temperature, Some(0.7));
        assert_eq!(gen.top_p, Some(0.9));
    }

    #[test]
    fn gemini_to_openai_extracts_content() {
        let resp = GeminiResponse {
            candidates: vec![GeminiCandidate {
                content: Some(GeminiContent {
                    role: "model".to_string(),
                    parts: vec![GeminiPart {
                        text: Some("Hello, world!".to_string()),
                    }],
                }),
                finish_reason: Some("STOP".to_string()),
            }],
            usage_metadata: Some(GeminiUsageMetadata {
                prompt_token_count: 10,
                candidates_token_count: 5,
                total_token_count: 15,
            }),
        };

        let out = gemini_to_openai(&resp);
        assert_eq!(out.choices.len(), 1);
        assert_eq!(out.choices[0].message.role, "assistant");
        assert_eq!(out.choices[0].message.content.as_ref().and_then(Value::as_str), Some("Hello, world!"));
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("stop"));
        let usage = out.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 15);
    }

    #[test]
    fn gemini_to_openai_maps_finish_reason() {
        let resp = GeminiResponse {
            candidates: vec![GeminiCandidate {
                content: Some(GeminiContent {
                    role: "model".to_string(),
                    parts: vec![GeminiPart {
                        text: Some("ok".to_string()),
                    }],
                }),
                finish_reason: Some("MAX_TOKENS".to_string()),
            }],
            usage_metadata: None,
        };

        let out = gemini_to_openai(&resp);
        assert_eq!(out.choices[0].finish_reason.as_deref(), Some("length"));
    }

    #[test]
    fn gemini_to_openai_empty_response() {
        let resp = GeminiResponse {
            candidates: vec![],
            usage_metadata: None,
        };

        let out = gemini_to_openai(&resp);
        assert_eq!(out.choices.len(), 1);
        assert_eq!(out.choices[0].message.content.as_ref().and_then(Value::as_str), Some(""));
    }

    #[test]
    fn openai_message_preserves_tool_call_id() {
        let raw = r#"{"model":"test","messages":[{"role":"user","content":"call tool"},{"role":"tool","tool_call_id":"call_abc","content":"result"}],"stream":false}"#;
        let req: OpenAIRequest = serde_json::from_str(raw).unwrap();
        assert_eq!(req.messages[1].tool_call_id.as_deref(), Some("call_abc"));
        let serialized = serde_json::to_value(&req).unwrap();
        assert_eq!(serialized["messages"][1]["tool_call_id"], "call_abc");
    }

    #[test]
    fn openai_message_preserves_null_content_and_tool_calls() {
        let raw = r#"{"model":"test","messages":[{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"foo","arguments":"{}"}}]}],"stream":false}"#;
        let req: OpenAIRequest = serde_json::from_str(raw).unwrap();
        let msg = &req.messages[0];
        assert!(msg.content.as_ref().map(|v| v.is_null()).unwrap_or(false));
        assert_eq!(msg.tool_calls.as_ref().map(|v| v.len()), Some(1));
        let serialized = serde_json::to_value(&req).unwrap();
        assert_eq!(serialized["messages"][0]["content"], serde_json::Value::Null);
        assert_eq!(
            serialized["messages"][0]["tool_calls"],
            serde_json::json!([{"id":"call_1","type":"function","function":{"name":"foo","arguments":"{}"}}])
        );
    }

    #[test]
    fn openai_message_preserves_content_array() {
        let raw = r#"{"model":"test","messages":[{"role":"user","content":[{"type":"text","text":"hello"},{"type":"image_url","image_url":{"url":"https://example.com/img.png"}}]}],"stream":false}"#;
        let req: OpenAIRequest = serde_json::from_str(raw).unwrap();
        let msg = &req.messages[0];
        assert!(msg.content.as_ref().map(|v| v.is_array()).unwrap_or(false));
        let serialized = serde_json::to_value(&req).unwrap();
        let arr = &serialized["messages"][0]["content"];
        assert!(arr.is_array());
        assert_eq!(arr[0]["type"], "text");
        assert_eq!(arr[1]["type"], "image_url");
    }

    // ---- H4 fix: function-calling fields TRANSLATED to Anthropic shape ----
    //
    // The original H4 fix passed `tools` and `tool_choice` through verbatim
    // in OpenAI shape. That was wrong: Anthropic (and MiniMax's Anthropic-
    // compatible API) expect a different shape, and reject OpenAI-shaped
    // tools with `(2013) function name or parameters is empty`. These
    // tests now assert the translation.

    #[test]
    fn h4_tools_array_translated_to_anthropic_shape() {
        // OpenAI shape: {type:"function", function:{name, description, parameters}}
        // Anthropic shape: {name, description, input_schema}
        let mut req = openai_req_with(vec![("user", "What is the weather in SF?")]);
        req.tools = Some(vec![json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Look up weather",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"}
                    },
                    "required": ["location"]
                }
            }
        })]);
        let out = openai_to_anthropic(&req);
        let tools = out.tools.as_ref().expect("tools should be translated");
        assert_eq!(tools.len(), 1);
        // Top-level keys are Anthropic shape, NOT OpenAI shape.
        assert_eq!(tools[0]["name"], "get_weather");
        assert_eq!(tools[0]["description"], "Look up weather");
        assert_eq!(tools[0]["input_schema"]["type"], "object");
        assert_eq!(tools[0]["input_schema"]["required"][0], "location");
        // The OpenAI `function` wrapper must NOT be present.
        assert!(tools[0].get("function").is_none());
        assert!(tools[0].get("type").is_none());
    }

    #[test]
    fn h4_tool_choice_translated_to_anthropic_shape() {
        let mut req = openai_req_with(vec![("user", "go")]);

        // String "auto" → {"type":"auto"}
        req.tool_choice = Some(json!("auto"));
        let out = openai_to_anthropic(&req);
        assert_eq!(out.tool_choice.as_ref().unwrap(), &json!({"type": "auto"}));

        // String "none" → {"type":"none"}
        req.tool_choice = Some(json!("none"));
        let out = openai_to_anthropic(&req);
        assert_eq!(out.tool_choice.as_ref().unwrap(), &json!({"type": "none"}));

        // String "required" → {"type":"any"} (Anthropic's name for "force a tool call")
        req.tool_choice = Some(json!("required"));
        let out = openai_to_anthropic(&req);
        assert_eq!(out.tool_choice.as_ref().unwrap(), &json!({"type": "any"}));

        // Object form {type:"function", function:{name:"X"}}
        // → Anthropic {type:"tool", name:"X"}
        req.tool_choice = Some(json!({
            "type": "function",
            "function": {"name": "search"}
        }));
        let out = openai_to_anthropic(&req);
        let tc = out.tool_choice.as_ref().unwrap();
        assert_eq!(tc["type"], "tool");
        assert_eq!(tc["name"], "search");
        // The OpenAI `function` wrapper must NOT be present.
        assert!(tc.get("function").is_none());
    }

    #[test]
    fn minimax_tools_with_empty_name_are_filtered_out() {
        // MiniMax rejects tools with empty `name` with `(2013)`.
        // The translator must filter them out before sending.
        let mut req = openai_req_with(vec![("user", "go")]);
        req.tools = Some(vec![
            json!({
                "type": "function",
                "function": {
                    "name": "valid_tool",
                    "description": "This one is fine",
                    "parameters": {"type": "object"}
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "",
                    "description": "This one has an empty name",
                    "parameters": {"type": "object"}
                }
            }),
            json!({
                "type": "function",
                "function": {
                    "description": "This one has no name at all",
                    "parameters": {"type": "object"}
                }
            }),
        ]);
        let out = openai_to_anthropic(&req);
        let tools = out.tools.as_ref().expect("tools should be present");
        assert_eq!(tools.len(), 1, "only the tool with a non-empty name should survive");
        assert_eq!(tools[0]["name"], "valid_tool");
    }

    #[test]
    fn minimax_assistant_tool_calls_translated_to_tool_use_blocks() {
        // OpenAI assistant message with `tool_calls` must be translated
        // to Anthropic `tool_use` content blocks. The `arguments` string
        // must be parsed to a JSON object for Anthropic's `input` field.
        let mut req = openai_req_with(vec![]);
        req.messages = vec![
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("What's the weather in Paris?")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::Null),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_abc123",
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "arguments": "{\"city\": \"Paris\"}"
                    }
                })]),
                extra: serde_json::Map::new(),
            },
        ];
        let out = openai_to_anthropic(&req);
        assert_eq!(out.messages.len(), 2);
        // The assistant message should have an array content with a tool_use block.
        let assistant_msg = &out.messages[1];
        assert_eq!(assistant_msg.role, "assistant");
        let blocks = assistant_msg.content.as_array().expect("content should be an array");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["id"], "call_abc123");
        assert_eq!(blocks[0]["name"], "get_weather");
        // `arguments` string was parsed to a JSON object for `input`.
        assert_eq!(blocks[0]["input"]["city"], "Paris");
    }

    #[test]
    fn minimax_tool_role_message_translated_to_tool_result_block() {
        // OpenAI `tool`-role message must be translated to Anthropic
        // `tool_result` content block under a `user`-role message.
        let mut req = openai_req_with(vec![]);
        req.messages = vec![
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("What's the weather?")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::Null),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_xyz",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                })]),
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "tool".to_string(),
                content: Some(json!("{\"temp\": 18}")),
                name: None,
                tool_call_id: Some("call_xyz".to_string()),
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
        ];
        let out = openai_to_anthropic(&req);
        assert_eq!(out.messages.len(), 3);
        // The third message (OpenAI `tool`-role) should become a
        // `user`-role message with a `tool_result` content block.
        let tool_result_msg = &out.messages[2];
        assert_eq!(tool_result_msg.role, "user");
        let blocks = tool_result_msg.content.as_array().expect("content should be an array");
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], "tool_result");
        assert_eq!(blocks[0]["tool_use_id"], "call_xyz");
        assert_eq!(blocks[0]["content"], "{\"temp\": 18}");
    }

    #[test]
    fn minimax_assistant_tool_calls_with_empty_name_are_skipped() {
        // A tool_call with an empty `name` would trigger MiniMax's
        // `(2013)` rejection. The translator must skip it.
        let mut req = openai_req_with(vec![]);
        req.messages = vec![
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("go")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(json!("Thinking...")),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![
                    json!({
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "", "arguments": "{}"}
                    }),
                    json!({
                        "id": "call_2",
                        "type": "function",
                        "function": {"name": "valid_tool", "arguments": "{\"x\":1}"}
                    }),
                ]),
                extra: serde_json::Map::new(),
            },
        ];
        let out = openai_to_anthropic(&req);
        let assistant_msg = &out.messages[1];
        let blocks = assistant_msg.content.as_array().expect("content should be an array");
        // text block + 1 valid tool_use block (the empty-name one is skipped)
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["type"], "text");
        assert_eq!(blocks[0]["text"], "Thinking...");
        assert_eq!(blocks[1]["type"], "tool_use");
        assert_eq!(blocks[1]["name"], "valid_tool");
    }

    #[test]
    fn minimax_tool_calls_with_empty_arguments_become_empty_object() {
        // Anthropic requires `input` to be present (even if empty).
        // Empty `arguments` string → `input: {}`.
        let mut req = openai_req_with(vec![]);
        req.messages = vec![OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(serde_json::Value::Null),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![json!({
                "id": "call_1",
                "type": "function",
                "function": {"name": "no_args_tool", "arguments": ""}
            })]),
            extra: serde_json::Map::new(),
        }];
        let out = openai_to_anthropic(&req);
        let assistant_msg = &out.messages[0];
        let blocks = assistant_msg.content.as_array().expect("content should be an array");
        assert_eq!(blocks[0]["type"], "tool_use");
        assert_eq!(blocks[0]["input"], json!({}));
    }

    #[test]
    fn minimax_full_tool_round_trip_request_shape() {
        // End-to-end: a complete OpenAI tool-calling conversation
        // translated to the Anthropic shape MiniMax expects. This is
        // the exact scenario that was failing with `(2013)`.
        let mut req = openai_req_with(vec![]);
        req.model = "MiniMax-M3".to_string();
        req.tools = Some(vec![json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather for a city",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        })]);
        req.tool_choice = Some(json!("auto"));
        req.messages = vec![
            OpenAIMessage {
                role: "user".to_string(),
                content: Some(json!("What's the weather in Paris?")),
                name: None, tool_call_id: None, tool_calls: None,
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::Null),
                name: None, tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                })]),
                extra: serde_json::Map::new(),
            },
            OpenAIMessage {
                role: "tool".to_string(),
                content: Some(json!("{\"temp\":18,\"unit\":\"c\"}")),
                name: None,
                tool_call_id: Some("call_1".to_string()),
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
        ];
        let out = openai_to_anthropic(&req);

        // Tools: Anthropic shape with name/description/input_schema.
        let tools = out.tools.as_ref().expect("tools present");
        assert_eq!(tools[0]["name"], "get_weather");
        assert_eq!(tools[0]["input_schema"]["properties"]["city"]["type"], "string");

        // tool_choice: {"type":"auto"}
        assert_eq!(out.tool_choice.as_ref().unwrap(), &json!({"type": "auto"}));

        // Messages: 3 entries (user, assistant with tool_use, user with tool_result)
        assert_eq!(out.messages.len(), 3);
        assert_eq!(out.messages[0].role, "user");
        assert_eq!(out.messages[0].content, json!("What's the weather in Paris?"));

        let asst_blocks = out.messages[1].content.as_array().unwrap();
        assert_eq!(asst_blocks[0]["type"], "tool_use");
        assert_eq!(asst_blocks[0]["name"], "get_weather");
        assert_eq!(asst_blocks[0]["input"]["city"], "Paris");

        let tool_blocks = out.messages[2].content.as_array().unwrap();
        assert_eq!(tool_blocks[0]["type"], "tool_result");
        assert_eq!(tool_blocks[0]["tool_use_id"], "call_1");
        assert_eq!(tool_blocks[0]["content"], "{\"temp\":18,\"unit\":\"c\"}");

        // Serialize and verify the JSON shape matches what MiniMax expects.
        let serialized = serde_json::to_value(&out).unwrap();
        // No `function` wrapper anywhere in tools.
        assert!(serialized["tools"][0].get("function").is_none());
        // No `type:"function"` in tools.
        assert!(serialized["tools"][0].get("type").is_none());
        // tool_choice is the Anthropic object form.
        assert_eq!(serialized["tool_choice"]["type"], "auto");
    }

    #[test]
    fn h4_top_k_passes_through_to_anthropic() {
        let mut req = openai_req_with(vec![("user", "go")]);
        req.top_k = Some(40);
        let out = openai_to_anthropic(&req);
        assert_eq!(out.top_k, Some(40));
    }

    #[test]
    fn h4_user_field_maps_to_anthropic_metadata_user_id() {
        // OpenAI's `user` field is documented as an opaque end-user
        // identifier for abuse detection. Anthropic has no direct
        // equivalent but reserves `metadata.user_id` for the same
        // purpose. The translator should produce exactly that shape.
        let mut req = openai_req_with(vec![("user", "go")]);
        req.user = Some("user-abc-123".to_string());
        let out = openai_to_anthropic(&req);
        let metadata = out.metadata.as_ref().expect("metadata set when user is set");
        assert_eq!(metadata["user_id"], "user-abc-123");
    }

    #[test]
    fn h4_absent_optional_fields_default_to_none() {
        // The fix must not regress existing behaviour: a request
        // that does not set tools / tool_choice / top_k / user must
        // still serialise to a valid Anthropic request with those
        // fields absent (serde skip_serializing_if = "Option::is_none").
        let req = openai_req_with(vec![("user", "hi")]);
        let out = openai_to_anthropic(&req);
        assert!(out.tools.is_none());
        assert!(out.tool_choice.is_none());
        assert!(out.top_k.is_none());
        assert!(out.metadata.is_none());
    }
}
