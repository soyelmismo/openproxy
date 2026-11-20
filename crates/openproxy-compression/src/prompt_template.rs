//! Prompt rendering for models whose official chat template is pinned and
//! reproducible.
//!
//! Only the Qwen3-Coder-Next template from the pinned Hugging Face revision
//! `a7fbcb5c0e12d62a448eaa0e260346bf5dcc0feb` is rendered. It is embedded in
//! the repository (6 KB) and its sha256 is recorded in
//! `assets/tokenizers/manifest.json`; the fetch script verifies it. No
//! template supplied by a request or by an upstream is ever executed.
//!
//! When a request contains media, an unparseable tool-call argument or an
//! unsupported content shape, rendering returns an error and the caller falls
//! back to the clearly-estimated per-message heuristic. Base64/media payloads
//! are never converted to text.

use std::sync::OnceLock;

use minijinja::{Environment, Value as JinjaValue};
use openproxy_types::message::OpenAIMessage;
use serde_json::{Value, json};

/// Pinned official Qwen3-Coder-Next chat template (HF revision a7fbcb5c).
pub(crate) const QWEN3_CODER_NEXT_TEMPLATE: &str =
    include_str!("../assets/tokenizers/qwen3-coder-next-a7fbcb5.chat_template.jinja");

/// sha256 of [`QWEN3_CODER_NEXT_TEMPLATE`], also recorded in the manifest.
pub(crate) const QWEN3_CODER_NEXT_TEMPLATE_SHA256: &str =
    "c79a833039a43602150cce0902403d6e376c50930c1b2a139b2964e1f0c322a0";

/// Maximum serialized prompt size we are willing to render. Larger requests
/// fall back to the cheap estimate instead of spending unbounded CPU/memory
/// on a template render.
const MAX_RENDER_INPUT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PromptRenderError {
    /// Media (image/audio/video/file) is present: the official text template
    /// cannot represent it, and base64 must never be counted as text.
    UnsupportedMedia,
    /// A message content shape the template does not understand.
    UnsupportedContent,
    /// A tool call whose `arguments` is not a JSON object string.
    MalformedToolCall,
    /// The request is too large for a template render on the hot path.
    Oversized,
    /// The embedded template failed to compile/render (should be unreachable;
    /// treated as a fallback rather than a request failure).
    Template,
}

/// Returns true only for the model family whose official template is pinned.
/// A provider prefix is ignored (`kiro/qwen3-coder-next`), but the id must be
/// the canonical one: `not-qwen3-coder-next-unknown` must not match.
pub(crate) fn is_qwen3_coder_next(model: &str) -> bool {
    let lower = model.to_ascii_lowercase();
    let path_segment = lower.rsplit('/').next().unwrap_or(&lower);
    let last = crate::token_estimate::model_segment(path_segment);
    crate::token_estimate::starts_with_canonical(last, &["qwen3-coder-next", "qwen3-coder_next"])
}

fn template_environment() -> Result<&'static Environment<'static>, PromptRenderError> {
    static ENV: OnceLock<Result<Environment<'static>, ()>> = OnceLock::new();
    match ENV.get_or_init(|| {
        let mut env = Environment::new();
        env.add_template_owned("qwen3-coder-next", QWEN3_CODER_NEXT_TEMPLATE.to_string())
            .map_err(|_| ())?;
        Ok(env)
    }) {
        Ok(env) => Ok(env),
        Err(()) => Err(PromptRenderError::Template),
    }
}

fn is_media_part(part: &Value) -> bool {
    if let Some(part_type) = part.get("type").and_then(Value::as_str)
        && matches!(
            part_type,
            "image_url"
                | "input_image"
                | "image"
                | "input_audio"
                | "audio"
                | "input_video"
                | "video_url"
                | "video"
                | "file"
                | "input_file"
                | "document"
        )
    {
        return true;
    }
    part.get("image_url").is_some()
        || part.get("video_url").is_some()
        || part.get("input_audio").is_some()
        || part.get("file").is_some()
}

/// Text-only content for the template. Arrays are joined with `\n`; media
/// parts make the render fail so the caller keeps the estimated fallback.
fn content_for_template(
    msg: &OpenAIMessage,
    input_bytes: &mut usize,
) -> Result<Value, PromptRenderError> {
    let text = match &msg.content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => {
            *input_bytes = input_bytes.saturating_add(s.len());
            s.clone()
        }
        Some(Value::Array(parts)) => {
            let mut texts = Vec::with_capacity(parts.len());
            for part in parts {
                if is_media_part(part) {
                    return Err(PromptRenderError::UnsupportedMedia);
                }
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    *input_bytes = input_bytes.saturating_add(text.len());
                    texts.push(text);
                } else if let Some(text) = part.get("content").and_then(Value::as_str) {
                    *input_bytes = input_bytes.saturating_add(text.len());
                    texts.push(text);
                } else {
                    return Err(PromptRenderError::UnsupportedContent);
                }
            }
            texts.join("\n")
        }
        Some(_) => return Err(PromptRenderError::UnsupportedContent),
    };
    Ok(Value::String(text))
}

/// The Qwen template iterates `tool_call.arguments|items`, i.e. it expects a
/// mapping. OpenAI clients send a JSON *string*; parse it once here. Anything
/// that is not a JSON object is rejected instead of being guessed.
fn arguments_for_template(args: Option<&Value>) -> Result<Value, PromptRenderError> {
    match args {
        None | Some(Value::Null) => Ok(json!({})),
        Some(Value::Object(_)) => args.cloned().ok_or(PromptRenderError::MalformedToolCall),
        Some(Value::String(raw)) => {
            let parsed: Value =
                serde_json::from_str(raw).map_err(|_| PromptRenderError::MalformedToolCall)?;
            if parsed.is_object() {
                Ok(parsed)
            } else {
                Err(PromptRenderError::MalformedToolCall)
            }
        }
        Some(_) => Err(PromptRenderError::MalformedToolCall),
    }
}

fn message_for_template(
    msg: &OpenAIMessage,
    input_bytes: &mut usize,
) -> Result<Value, PromptRenderError> {
    let mut object = serde_json::Map::with_capacity(4);
    object.insert("role".to_string(), Value::String(msg.role.clone()));
    object.insert(
        "content".to_string(),
        content_for_template(msg, input_bytes)?,
    );
    if let Some(name) = &msg.name {
        object.insert("name".to_string(), Value::String(name.clone()));
    }
    if let Some(calls) = &msg.tool_calls {
        let mut rendered_calls = Vec::with_capacity(calls.len());
        for call in calls {
            let function = call
                .get("function")
                .ok_or(PromptRenderError::MalformedToolCall)?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or(PromptRenderError::MalformedToolCall)?;
            let arguments = arguments_for_template(function.get("arguments"))?;
            let mut function_obj = serde_json::Map::with_capacity(2);
            function_obj.insert("name".to_string(), Value::String(name.to_string()));
            function_obj.insert("arguments".to_string(), arguments);
            let mut call_obj = serde_json::Map::with_capacity(3);
            if let Some(id) = call.get("id") {
                call_obj.insert("id".to_string(), id.clone());
            }
            call_obj.insert("type".to_string(), Value::String("function".to_string()));
            call_obj.insert("function".to_string(), Value::Object(function_obj));
            rendered_calls.push(Value::Object(call_obj));
        }
        object.insert("tool_calls".to_string(), Value::Array(rendered_calls));
    }
    Ok(Value::Object(object))
}

/// Render the pinned Qwen3-Coder-Next prompt, including the generation prompt.
///
/// `tools` is the OpenAI `tools` array (already validated by the request
/// parser). The rendered string includes roles, tool definitions, tool calls
/// and the final `<|im_start|>assistant\n` generation prompt.
pub(crate) fn render_qwen3_coder_next_prompt(
    messages: &[OpenAIMessage],
    tools: Option<&[Value]>,
) -> Result<String, PromptRenderError> {
    let mut input_bytes = 0usize;
    let mut rendered_messages = Vec::with_capacity(messages.len());
    for msg in messages {
        rendered_messages.push(message_for_template(msg, &mut input_bytes)?);
        if input_bytes > MAX_RENDER_INPUT_BYTES {
            return Err(PromptRenderError::Oversized);
        }
    }
    let tools_value = tools.map_or_else(|| json!([]), |tools| Value::Array(tools.to_vec()));
    if let Ok(serialized) = serde_json::to_string(&tools_value) {
        input_bytes = input_bytes.saturating_add(serialized.len());
    }
    if input_bytes > MAX_RENDER_INPUT_BYTES {
        return Err(PromptRenderError::Oversized);
    }

    let context = json!({
        "messages": rendered_messages,
        "tools": tools_value,
        "add_generation_prompt": true,
    });
    let env = template_environment()?;
    let template = env
        .get_template("qwen3-coder-next")
        .map_err(|_| PromptRenderError::Template)?;
    let rendered = template
        .render(JinjaValue::from(minijinja::value::Serde(&context)))
        .map_err(|_| PromptRenderError::Template)?;
    Ok(rendered)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> OpenAIMessage {
        OpenAIMessage {
            role: role.to_string(),
            content: Some(Value::String(content.to_string())),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        }
    }

    #[test]
    fn detects_only_the_pinned_qwen_coder_model() {
        assert!(is_qwen3_coder_next("qwen3-coder-next"));
        assert!(is_qwen3_coder_next("kiro/Qwen3-Coder-Next"));
        assert!(!is_qwen3_coder_next("qwen3-32b"));
        assert!(!is_qwen3_coder_next("qwen3-coder"));
        assert!(!is_qwen3_coder_next("claude-opus-5.5"));
    }

    #[test]
    fn renders_roles_tools_and_generation_prompt() {
        let messages = vec![
            msg("system", "You are a helpful assistant."),
            msg("user", "What is the weather in Madrid?"),
        ];
        let tools = vec![json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get the current weather for a city.",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        })];
        let rendered = render_qwen3_coder_next_prompt(&messages, Some(&tools)).unwrap();
        assert!(rendered.starts_with("<|im_start|>system\nYou are a helpful assistant."));
        assert!(rendered.contains("<name>get_weather</name>"));
        assert!(rendered.contains("<|im_start|>user\nWhat is the weather in Madrid?"));
        assert!(rendered.ends_with("<|im_start|>assistant\n"));
    }

    #[test]
    fn assistant_tool_call_arguments_are_parsed_as_mapping() {
        let messages = vec![OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(Value::Null),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![json!({
                "id": "call_1",
                "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Madrid\"}"}
            })]),
            extra: serde_json::Map::new(),
        }];
        let rendered = render_qwen3_coder_next_prompt(&messages, None).unwrap();
        assert!(rendered.contains("<function=get_weather>"));
        assert!(rendered.contains("<parameter=city>\nMadrid"));
    }

    #[test]
    fn media_and_malformed_tool_calls_fall_back_instead_of_rendering_base64() {
        let media = OpenAIMessage {
            role: "user".to_string(),
            content: Some(json!([{
                "type": "image_url",
                "image_url": {"url": format!("data:image/png;base64,{}", "A".repeat(100_000))}
            }])),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        };
        assert_eq!(
            render_qwen3_coder_next_prompt(&[media], None),
            Err(PromptRenderError::UnsupportedMedia)
        );

        let bad_call = OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(Value::Null),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![json!({
                "function": {"name": "x", "arguments": "not-json"}
            })]),
            extra: serde_json::Map::new(),
        };
        assert_eq!(
            render_qwen3_coder_next_prompt(&[bad_call], None),
            Err(PromptRenderError::MalformedToolCall)
        );
    }

    #[test]
    fn embedded_template_hash_matches_manifest_constant() {
        let manifest = include_str!("../assets/tokenizers/manifest.json");
        let v: Value = serde_json::from_str(manifest).unwrap();
        let template = v["templates"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == "qwen3-coder-next-chat-template")
            .unwrap();
        assert_eq!(
            template["sha256"].as_str().unwrap(),
            QWEN3_CODER_NEXT_TEMPLATE_SHA256
        );
        assert_eq!(
            template["bytes"].as_u64().unwrap() as usize,
            QWEN3_CODER_NEXT_TEMPLATE.len()
        );
    }
}
