use serde_json::Value;
use openproxy_types::{OpenAIMessage, OpenAIRequest};
use crate::translation::types::*;
use crate::translation::helpers::*;

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
pub fn openai_to_gemini(req: &OpenAIRequest, override_messages: &[OpenAIMessage]) -> GeminiRequest {
    let mut system_parts: Vec<String> = Vec::new();
    let mut contents: Vec<GeminiContent> = Vec::with_capacity(override_messages.len());

    for m in override_messages {
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
            let text: String = c.parts.iter().filter_map(|p| p.text.as_deref()).collect();
            if text.is_empty() { None } else { Some(text) }
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
