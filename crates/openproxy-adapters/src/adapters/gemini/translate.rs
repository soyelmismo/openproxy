use super::media::parse_media_part_to_inline_data;
use super::types::{
    DEFAULT_GEMINI_MAX_OUTPUT_TOKENS, GeminiContent, GeminiFunctionCall,
    GeminiFunctionCallingConfig, GeminiFunctionCallingMode, GeminiFunctionDeclaration,
    GeminiFunctionResponse, GeminiGenerationConfig, GeminiPart, GeminiRequest, GeminiResponse,
    GeminiSafetySetting, GeminiThinkingConfig, GeminiTool, GeminiToolConfig,
};

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
            "assistant" => {
                let mut parts: Vec<GeminiPart> = match m.content.as_ref() {
                    Some(serde_json::Value::Array(arr)) => {
                        arr.iter().map(map_single_content_part).collect()
                    }
                    Some(serde_json::Value::String(s)) if !s.is_empty() => {
                        vec![GeminiPart {
                            text: Some(s.clone()),
                            ..Default::default()
                        }]
                    }
                    Some(val) if !val.is_null() => vec![map_single_content_part(val)],
                    _ => Vec::new(),
                };
                if let Some(tool_calls) = &m.tool_calls {
                    for tc in tool_calls {
                        let name = tc
                            .get("function")
                            .and_then(|f| f.get("name"))
                            .and_then(|n| n.as_str())
                            .unwrap_or("")
                            .to_string();
                        let args: serde_json::Value = tc
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(|a| {
                                if let Some(s) = a.as_str() {
                                    serde_json::from_str(s).ok()
                                } else {
                                    Some(a.clone())
                                }
                            })
                            .unwrap_or_else(|| serde_json::json!({}));
                        let id = tc.get("id").and_then(|id| id.as_str()).map(String::from);
                        parts.push(GeminiPart {
                            function_call: Some(GeminiFunctionCall { name, args, id }),
                            ..Default::default()
                        });
                    }
                }
                if parts.is_empty() {
                    parts.push(GeminiPart {
                        text: Some(String::new()),
                        ..Default::default()
                    });
                }
                contents.push(GeminiContent {
                    role: "model".to_string(),
                    parts,
                });
            }
            "tool" => {
                let name = m.name.clone().unwrap_or_else(|| {
                    m.tool_call_id
                        .clone()
                        .unwrap_or_else(|| "function".to_string())
                });
                let response_val = match &m.content {
                    Some(serde_json::Value::Object(map)) => serde_json::Value::Object(map.clone()),
                    Some(serde_json::Value::Array(arr)) => serde_json::json!({ "output": arr }),
                    Some(serde_json::Value::String(s)) => {
                        if let Ok(val) = serde_json::from_str::<serde_json::Value>(s) {
                            if val.is_object() {
                                val
                            } else {
                                serde_json::json!({ "output": val })
                            }
                        } else {
                            serde_json::json!({ "output": s })
                        }
                    }
                    Some(serde_json::Value::Null) | None => serde_json::json!({ "output": "" }),
                    Some(other) => serde_json::json!({ "output": other }),
                };
                contents.push(GeminiContent {
                    role: "function".to_string(),
                    parts: vec![GeminiPart {
                        function_response: Some(GeminiFunctionResponse {
                            name: name.clone(),
                            response: serde_json::json!({
                                "name": name,
                                "content": response_val,
                            }),
                        }),
                        ..Default::default()
                    }],
                });
            }
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
    let Some(tools) = tools else {
        return (None, None);
    };
    if tools.is_empty() {
        return (None, None);
    }

    let declarations: Vec<GeminiFunctionDeclaration> = tools
        .iter()
        .filter_map(map_openai_tool_to_declaration)
        .collect();

    if declarations.is_empty() {
        return (None, None);
    }

    let tool_config = Some(tool_choice_to_config(tool_choice));

    (
        Some(vec![GeminiTool {
            function_declarations: declarations,
        }]),
        tool_config,
    )
}

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
                crate::schema_cleaner::clean_json_schema(&mut p);
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
            crate::schema_cleaner::clean_json_schema(&mut p);
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

fn tool_choice_to_config(tool_choice: Option<&serde_json::Value>) -> GeminiToolConfig {
    let Some(tc) = tool_choice else {
        return GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode: GeminiFunctionCallingMode::Auto,
                allowed_function_names: None,
            },
        };
    };

    if let Some(s) = tc.as_str() {
        let mode = match s {
            "none" => GeminiFunctionCallingMode::None,
            "required" | "any" => GeminiFunctionCallingMode::Any,
            _ => GeminiFunctionCallingMode::Auto,
        };
        return GeminiToolConfig {
            function_calling_config: GeminiFunctionCallingConfig {
                mode,
                allowed_function_names: None,
            },
        };
    }

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

    GeminiToolConfig {
        function_calling_config: GeminiFunctionCallingConfig {
            mode: GeminiFunctionCallingMode::Auto,
            allowed_function_names: None,
        },
    }
}

/// Convert an OpenAI-format chat completion request to Gemini format.
pub fn openai_to_gemini(
    req: &openproxy_types::OpenAIRequest,
    override_messages: &[openproxy_types::OpenAIMessage],
) -> GeminiRequest {
    let (system_instruction, contents) = partition_messages_for_gemini(override_messages);

    let thinking_config = req
        .extra
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .or_else(|| req.extra.get("thinking_effort").and_then(|v| v.as_str()))
        .map(|effort| {
            let budget = match effort {
                "none" => 0,
                "low" => 1024,
                "medium" => 8192,
                "high" => 16384,
                "max" | "xhigh" => 32768,
                s => s.parse::<i32>().unwrap_or(8192),
            };
            GeminiThinkingConfig {
                thinking_budget: budget,
            }
        });

    let generation_config = GeminiGenerationConfig {
        temperature: req.temperature,
        top_p: req.top_p,
        max_output_tokens: req.max_tokens.or(Some(DEFAULT_GEMINI_MAX_OUTPUT_TOKENS)),
        stop_sequences: req.stop.clone(),
        thinking_config,
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

    let mut tool_calls = Vec::new();
    if let Some(c) = candidate
        && let Some(content_probe) = &c.content
    {
        for (idx, p) in content_probe.parts.iter().enumerate() {
            if let Some(fc) = &p.function_call {
                let call_id = fc.id.clone().unwrap_or_else(|| {
                    let ts = chrono::Utc::now().timestamp_millis();
                    format!("call_gemini_{ts}_{idx}")
                });
                let args_str = if fc.args.is_null() {
                    "{}".to_string()
                } else {
                    serde_json::to_string(&fc.args).unwrap_or_else(|_| "{}".to_string())
                };
                tool_calls.push(serde_json::json!({
                    "id": call_id,
                    "type": "function",
                    "function": {
                        "name": fc.name,
                        "arguments": args_str,
                    }
                }));
            }
        }
    }

    let tool_calls_opt = if tool_calls.is_empty() {
        None
    } else {
        Some(tool_calls)
    };

    let mut finish_reason = candidate
        .and_then(|c| c.finish_reason.as_deref())
        .map(map_gemini_finish_reason)
        .map(String::from);

    if tool_calls_opt.is_some()
        && (finish_reason.is_none() || finish_reason.as_deref() == Some("stop"))
    {
        finish_reason = Some("tool_calls".to_string());
    }

    let content_val = if content.is_empty() {
        if tool_calls_opt.is_some() {
            None
        } else {
            Some(serde_json::Value::String(String::new()))
        }
    } else {
        Some(serde_json::Value::String(content))
    };

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
                content: content_val,
                name: None,
                tool_call_id: None,
                tool_calls: tool_calls_opt,
                extra: serde_json::Map::new(),
            },
            finish_reason,
        }],
        usage,
    }
}

/// Serialize an OpenAI chat request into Gemini wire-format bytes.
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
