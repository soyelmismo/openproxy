//! Codex / ChatGPT Responses executor.

use crate::error::CoreError;
use crate::race_sink::StreamSink;
use crate::translation::{OpenAIChoice, OpenAIMessage, OpenAIRequest, OpenAIResponse, OpenAIUsage};
use crate::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest,
};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const DEFAULT_CODEX_CLIENT_VERSION: &str = "0.142.0";

pub fn codex_client_version() -> String {
    safe_env_value("OPENPROXY_CODEX_CLIENT_VERSION")
        .or_else(|| safe_env_value("CODEX_CLIENT_VERSION"))
        .unwrap_or_else(|| DEFAULT_CODEX_CLIENT_VERSION.to_string())
}

pub fn codex_user_agent() -> String {
    safe_env_value("OPENPROXY_CODEX_USER_AGENT")
        .or_else(|| safe_env_value("CODEX_USER_AGENT"))
        .unwrap_or_else(|| {
            format!(
                "codex-cli/{} (Windows 10.0.26200; x64)",
                codex_client_version()
            )
        })
}

fn safe_env_value(name: &str) -> Option<String> {
    let value = std::env::var(name).ok()?.trim().to_string();
    if value.is_empty() || value.len() > 200 || !value.bytes().all(|b| (0x20..=0x7e).contains(&b)) {
        return None;
    }
    Some(value)
}

fn extract_chatgpt_account_id(access_token: &str) -> Option<String> {
    let parts: Vec<&str> = access_token.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    use base64::Engine as _;
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(parts[1]))
        .ok()?;
    let json: Value = serde_json::from_slice(&payload).ok()?;
    json.get("aud")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub async fn execute_codex(
    upstream_client: &Arc<UpstreamClient>,
    access_token: &str,
    workspace_id: Option<&str>,
    openai: &OpenAIRequest,
    client_disconnected: watch::Receiver<bool>,
    stream_sink: Option<&StreamSink>,
    proxy: Option<String>,
    ctx: Option<crate::adapters::CustomExecutionContext>,
    account_id: Option<crate::ids::AccountId>,
) -> Result<OpenAIResponse, CoreError> {
    let session_id = extract_session_id(openai).unwrap_or_else(|| Uuid::new_v4().to_string());
    let url_with_session = format!("{}?session_id={}", CODEX_RESPONSES_URL, session_id);
    let body = build_codex_request(openai)?;
    let body_bytes = serde_json::to_vec(&body)
        .map_err(|e| CoreError::Internal(format!("failed to serialize codex request: {e}")))?;

    let mut req = UpstreamRequest::post_json(&url_with_session, bytes::Bytes::from(body_bytes));
    req.proxy = proxy;
    insert_header(
        &mut req.headers,
        http::header::AUTHORIZATION,
        &format!("Bearer {access_token}"),
    );
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("text/event-stream"),
    );
    insert_header(
        &mut req.headers,
        http::header::USER_AGENT,
        &codex_user_agent(),
    );
    insert_header(
        &mut req.headers,
        http::HeaderName::from_static("version"),
        &codex_client_version(),
    );
    req.headers.insert(
        http::HeaderName::from_static("originator"),
        http::HeaderValue::from_static("codex_cli_rs"),
    );
    req.headers.insert(
        http::HeaderName::from_static("openai-beta"),
        http::HeaderValue::from_static("responses=experimental"),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-codex-beta-features"),
        http::HeaderValue::from_static("responses_websockets"),
    );
    req.headers.insert(
        http::header::ORIGIN,
        http::HeaderValue::from_static("https://chatgpt.com"),
    );
    if let Some(portal) = safe_env_value("OPENPROXY_CODEX_PORTAL") {
        insert_header(&mut req.headers, http::HeaderName::from_static("x-openai-portal"), &portal);
    }
    if let Some(org) = safe_env_value("OPENPROXY_CODEX_ORGANIZATION") {
        insert_header(&mut req.headers, http::HeaderName::from_static("openai-organization"), &org);
    }
    let resolved_account_id = workspace_id
        .filter(|v| !v.is_empty())
        .map(|s| s.to_string())
        .or_else(|| extract_chatgpt_account_id(access_token))
        .unwrap_or_default();
    
    if !resolved_account_id.is_empty() {
        insert_header(
            &mut req.headers,
            http::HeaderName::from_static("chatgpt-account-id"),
            &resolved_account_id,
        );
    }

    let mut timeout = TimeoutProfile::Chat;
    if openai.model.contains("gpt-5") || openai.model.contains("o4") {
        let mut custom = TimeoutProfile::Chat.resolve();
        custom.headers_ms = 150_000;
        custom.body_chunk_ms = 150_000;
        timeout = TimeoutProfile::Custom(custom);
    }

    let cancel = CancellationToken::from_watch(client_disconnected);
    let response = upstream_client
        .call(req, timeout, cancel)
        .await
        .map_err(|e| match e {
            UpstreamError::Cancel => CoreError::ClientDisconnected,
            other => CoreError::UpstreamConnection(format!("codex request failed: {other}")),
        })?;

    let status = response.status.as_u16();

    if let (Some(ctx_ref), Some(account_id_ref)) = (&ctx, &account_id) {
        let mut should_cooldown = status == 429;
        let mut reset_secs = ctx_ref.cooldown_base_secs;

        if let Some(remaining_reqs) = response.headers.get("x-ratelimit-remaining-requests").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()) {
            if remaining_reqs == 0 {
                should_cooldown = true;
            }
        }
        if let Some(remaining_tokens) = response.headers.get("x-ratelimit-remaining-tokens").and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok()) {
            if remaining_tokens == 0 {
                should_cooldown = true;
            }
        }

        if should_cooldown {
            if let Some(header_reset) = response.headers.get("x-ratelimit-reset-requests")
                .or_else(|| response.headers.get("x-ratelimit-reset-tokens"))
                .or_else(|| response.headers.get("x-ratelimit-reset"))
                .and_then(|v| v.to_str().ok())
                .and_then(|s| crate::quota::parse_reset_time(s))
            {
                reset_secs = header_reset;
            }
            
            let reset_secs = reset_secs.clamp(1, ctx_ref.cooldown_max_secs);
            let lock = ctx_ref.conn.lock();
            let _ = crate::cooldown::record_failure_with_mode(
                &lock,
                crate::ids::ComboTargetId(account_id_ref.0),
                "codex ratelimit exhausted",
                ctx_ref.cooldown_mode,
                reset_secs,
                ctx_ref.cooldown_max_secs,
                ctx_ref.cooldown_factor,
            );
        }
    }

    if !(200..300).contains(&status) {
        let body_bytes = response.collect().await.unwrap_or_default();
        return Err(CoreError::UpstreamError {
            status,
            provider: "codex".into(),
            model: openai.model.clone(),
            body: String::from_utf8_lossy(&body_bytes).to_string(),
        });
    }

    parse_codex_sse_response(response, openai, stream_sink, ctx, account_id).await
}

fn insert_header(headers: &mut http::HeaderMap, name: http::HeaderName, value: &str) {
    if let Ok(value) = http::HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn build_codex_request(openai: &OpenAIRequest) -> Result<Value, CoreError> {
    let (model, effort_from_model) = normalize_model_and_effort(&openai.model);
    let mut obj = openai.extra.clone();
    obj.insert("model".to_string(), Value::String(model));
    
    let mut system_instructions = None;
    let mut messages_without_system = Vec::new();
    for msg in &openai.messages {
        if msg.role == "system" && system_instructions.is_none() {
            system_instructions = Some(content_to_text(msg.content.as_ref()));
        } else {
            messages_without_system.push(msg);
        }
    }
    
    obj.insert("input".to_string(), messages_to_responses_input(&messages_without_system));
    obj.insert("stream".to_string(), Value::Bool(true));
    obj.insert("store".to_string(), Value::Bool(false));
    
    let default_instructions = "Follow the developer instructions in the conversation.".to_string();
    obj.entry("instructions".to_string()).or_insert_with(|| {
        Value::String(system_instructions.unwrap_or(default_instructions))
    });

    if let Some(max_tokens) = openai.max_tokens {
        obj.insert("max_output_tokens".to_string(), json!(max_tokens));
    }
    if let Some(temperature) = openai.temperature {
        obj.insert("temperature".to_string(), json!(temperature));
    }
    if let Some(top_p) = openai.top_p {
        obj.insert("top_p".to_string(), json!(top_p));
    }
    if let Some(tools) = &openai.tools {
        obj.insert("tools".to_string(), Value::Array(tools.clone()));
    }
    if let Some(tool_choice) = &openai.tool_choice {
        obj.insert("tool_choice".to_string(), tool_choice.clone());
    }

    let effort = openai
        .extra
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .map(normalize_effort)
        .or(effort_from_model);
    if let Some(effort) = effort.filter(|v| *v != "none") {
        obj.insert(
            "reasoning".to_string(),
            json!({
                "effort": effort,
                "summary": "auto"
            }),
        );
    }
    if matches!(
        obj.get("service_tier").and_then(|v| v.as_str()),
        Some("fast")
    ) {
        obj.insert(
            "service_tier".to_string(),
            Value::String("priority".to_string()),
        );
    }

    let instructions_str = obj
        .get("instructions")
        .and_then(|v| v.as_str())
        .unwrap_or("Follow the developer instructions in the conversation.");
    
    use sha2::{Sha256, Digest};
    let mut hasher = Sha256::new();
    hasher.update(instructions_str.as_bytes());
    if let Some(tools) = &openai.tools {
        if let Ok(tools_str) = serde_json::to_string(tools) {
            hasher.update(tools_str.as_bytes());
        }
    }
    let hash_hex = hex::encode(hasher.finalize());
    obj.insert(
        "prompt_cache_key".to_string(),
        Value::String(format!("pck_{}", &hash_hex[..24]))
    );

    Ok(Value::Object(obj))
}

fn messages_to_responses_input(messages: &[&crate::translation::OpenAIMessage]) -> Value {
    let mut input_items = Vec::new();
    
    for msg in messages {
        if msg.role == "tool" {
            let mut obj = serde_json::Map::new();
            obj.insert("type".to_string(), Value::String("function_call_output".to_string()));
            if let Some(tool_call_id) = &msg.tool_call_id {
                obj.insert("call_id".to_string(), Value::String(tool_call_id.clone()));
            } else {
                obj.insert("call_id".to_string(), Value::String(uuid::Uuid::new_v4().to_string()));
            }
            let content_text = content_to_text(msg.content.as_ref());
            obj.insert("output".to_string(), Value::String(content_text));
            input_items.push(Value::Object(obj));
        } else if msg.role == "assistant" {
            let content_text = content_to_text(msg.content.as_ref());
            if !content_text.is_empty() || msg.tool_calls.is_none() {
                let mut obj = serde_json::Map::new();
                obj.insert("type".to_string(), Value::String("message".to_string()));
                obj.insert("role".to_string(), Value::String("assistant".to_string()));
                obj.insert("content".to_string(), message_content_parts(msg.content.as_ref(), "assistant"));
                input_items.push(Value::Object(obj));
            }
            if let Some(tool_calls) = &msg.tool_calls {
                for tc in tool_calls {
                    let mut obj = serde_json::Map::new();
                    obj.insert("type".to_string(), Value::String("function_call".to_string()));
                    
                    let call_id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    obj.insert("call_id".to_string(), Value::String(call_id));
                    
                    if let Some(func) = tc.get("function") {
                        let name = func.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                        // Serialize arguments back to string if it's an object, or keep it if string
                        let arguments = match func.get("arguments") {
                            Some(Value::String(s)) => s.clone(),
                            Some(v) => v.to_string(),
                            None => "{}".to_string(),
                        };
                        obj.insert("name".to_string(), Value::String(name));
                        obj.insert("arguments".to_string(), Value::String(arguments));
                    }
                    input_items.push(Value::Object(obj));
                }
            }
        } else {
            let mut obj = serde_json::Map::new();
            obj.insert("type".to_string(), Value::String("message".to_string()));
            obj.insert("role".to_string(), Value::String(codex_role(&msg.role).to_string()));
            obj.insert("content".to_string(), message_content_parts(msg.content.as_ref(), &msg.role));
            input_items.push(Value::Object(obj));
        }
    }
    
    Value::Array(input_items)
}

fn codex_role(role: &str) -> &str {
    match role {
        "system" => "developer",
        "assistant" => "assistant",
        "developer" => "developer",
        _ => "user",
    }
}

fn message_content_parts(content: Option<&Value>, role: &str) -> Value {
    let text_type = if role == "assistant" { "output_text" } else { "input_text" };
    match content {
        Some(Value::String(text)) => json!([{ "type": text_type, "text": text }]),
        Some(Value::Array(parts)) => {
            let mapped: Vec<Value> = parts
                .iter()
                .filter_map(|part| {
                    if let Some(t) = part.get("type").and_then(|v| v.as_str()) {
                        if t == "image_url" {
                            let mut img_part = serde_json::Map::new();
                            img_part.insert("type".to_string(), Value::String("input_image".to_string()));
                            
                            let mut url = String::new();
                            let mut detail = None;
                            
                            if let Some(image_url) = part.get("image_url") {
                                if let Some(obj) = image_url.as_object() {
                                    url = obj.get("url").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    detail = obj.get("detail").and_then(|v| v.as_str()).map(|s| s.to_string());
                                } else if let Some(s) = image_url.as_str() {
                                    url = s.to_string();
                                }
                            }
                            
                            img_part.insert("image_url".to_string(), Value::String(url));
                            if let Some(d) = detail {
                                img_part.insert("detail".to_string(), Value::String(d));
                            }
                            
                            return Some(Value::Object(img_part));
                        }
                    }
                    let text = part
                        .get("text")
                        .or_else(|| part.get("content"))
                        .and_then(|v| v.as_str())?;
                    Some(json!({ "type": text_type, "text": text }))
                })
                .collect();
            if mapped.is_empty() {
                json!([{ "type": text_type, "text": content_to_text(content) }])
            } else {
                Value::Array(mapped)
            }
        }
        Some(_) => json!([{ "type": text_type, "text": content_to_text(content) }]),
        None => json!([{ "type": text_type, "text": "" }]),
    }
}

fn content_to_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn normalize_model_and_effort(model: &str) -> (String, Option<&'static str>) {
    for (suffix, effort) in [
        ("-xhigh", "xhigh"),
        ("-high", "high"),
        ("-medium", "medium"),
        ("-low", "low"),
        ("-none", "none"),
    ] {
        if let Some(base) = model.strip_suffix(suffix) {
            return (base.to_string(), Some(effort));
        }
    }
    (model.to_string(), None)
}

fn normalize_effort(value: &str) -> &'static str {
    match value {
        "max" | "xhigh" => "xhigh",
        "high" => "high",
        "medium" => "medium",
        "low" => "low",
        "none" => "none",
        _ => "medium",
    }
}

fn extract_session_id(openai: &OpenAIRequest) -> Option<String> {
    for key in ["session_id", "prompt_cache_key", "conversation_id"] {
        if let Some(value) = openai.extra.get(key).and_then(|v| v.as_str())
            && is_safe_session_id(value)
        {
            return Some(value.to_string());
        }
    }
    None
}

fn is_safe_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'-'))
}

async fn parse_codex_sse_response(
    mut response: crate::upstream::UpstreamResponse,
    openai: &OpenAIRequest,
    stream_sink: Option<&StreamSink>,
    _ctx: Option<crate::adapters::CustomExecutionContext>,
    _account_id: Option<crate::ids::AccountId>,
) -> Result<OpenAIResponse, CoreError> {
    let created = chrono::Utc::now().timestamp() as u64;
    let chunk_id = format!("chatcmpl-{}", Uuid::new_v4());
    let mut accumulated_text = String::new();
    let mut accumulated_reasoning = String::new();
    let mut usage: Option<OpenAIUsage> = None;
    let mut line_buffer = String::new();
    let mut accumulated_tool_calls: Vec<Value> = Vec::new();

    if let Some(sink) = stream_sink {
        send_openai_chunk(
            sink,
            &chunk_id,
            created,
            &openai.model,
            json!({ "role": "assistant" }),
            None,
            None,
        )
        .await;
    }

    loop {
        let chunk = match response.body.next_chunk().await {
            Ok(Some(chunk)) => {
                response.body.note_content_chunk();
                chunk
            }
            Ok(None) => break,
            Err(UpstreamError::Cancel) => return Err(CoreError::ClientDisconnected),
            Err(e) => {
                return Err(CoreError::UpstreamConnection(format!(
                    "failed to read codex response chunk: {e}"
                )));
            }
        };

        line_buffer.push_str(&String::from_utf8_lossy(&chunk));
        process_complete_lines(
            &mut line_buffer,
            stream_sink,
            &chunk_id,
            created,
            &openai.model,
            &mut accumulated_text,
            &mut accumulated_reasoning,
            &mut accumulated_tool_calls,
            &mut usage,
        )
        .await?;
    }

    if !line_buffer.trim().is_empty() {
        let line = std::mem::take(&mut line_buffer);
        process_codex_line(
            &line,
            stream_sink,
            &chunk_id,
            created,
            &openai.model,
            &mut accumulated_text,
            &mut accumulated_reasoning,
            &mut accumulated_tool_calls,
            &mut usage,
        )
        .await?;
    }

    if let Some(sink) = stream_sink {
        send_openai_chunk(
            sink,
            &chunk_id,
            created,
            &openai.model,
            json!({}),
            Some("stop"),
            usage.as_ref(),
        )
        .await;
        let _ = sink.send(crate::pipeline::SSE_DONE_BYTES.clone()).await;
    }

    let mut extra = serde_json::Map::new();
    if !accumulated_reasoning.is_empty() {
        extra.insert(
            "reasoning_content".to_string(),
            Value::String(accumulated_reasoning),
        );
    }

    Ok(OpenAIResponse {
        id: format!("chatcmpl-{}", Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created,
        model: openai.model.clone(),
        choices: vec![OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(Value::String(accumulated_text)),
                name: None,
                tool_call_id: None,
                tool_calls: if accumulated_tool_calls.is_empty() { None } else { Some(accumulated_tool_calls) },
                extra,
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage,
    })
}

#[allow(clippy::too_many_arguments)]
async fn process_complete_lines(
    line_buffer: &mut String,
    stream_sink: Option<&StreamSink>,
    chunk_id: &str,
    created: u64,
    model: &str,
    accumulated_text: &mut String,
    accumulated_reasoning: &mut String,
    accumulated_tool_calls: &mut Vec<Value>,
    usage: &mut Option<OpenAIUsage>,
) -> Result<(), CoreError> {
    while let Some(pos) = line_buffer.find('\n') {
        let line = line_buffer[..pos].to_string();
        *line_buffer = line_buffer[pos + 1..].to_string();
        process_codex_line(
            &line,
            stream_sink,
            chunk_id,
            created,
            model,
            accumulated_text,
            accumulated_reasoning,
            accumulated_tool_calls,
            usage,
        )
        .await?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn process_codex_line(
    line: &str,
    stream_sink: Option<&StreamSink>,
    chunk_id: &str,
    created: u64,
    model: &str,
    accumulated_text: &mut String,
    accumulated_reasoning: &mut String,
    accumulated_tool_calls: &mut Vec<Value>,
    usage: &mut Option<OpenAIUsage>,
) -> Result<(), CoreError> {
    let line = line.trim();
    if !line.starts_with("data:") {
        return Ok(());
    }
    let data = line.trim_start_matches("data:").trim();
    if data.is_empty() || data == "[DONE]" {
        return Ok(());
    }
    let value: Value = serde_json::from_str(data)
        .map_err(|e| CoreError::Parse(format!("codex SSE JSON parse: {e}")))?;

    if let Some(error) = value.get("error") {
        return Err(CoreError::UpstreamError {
            status: 500,
            provider: "codex".into(),
            model: model.to_string(),
            body: error.to_string(),
        });
    }

    let event_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
    
    if event_type == "response.output_item.added" {
        if let Some(item) = value.get("item") {
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if item_type == "function_call" {
                let call_id = item.get("call_id").or_else(|| item.get("id")).and_then(|v| v.as_str()).unwrap_or("call_xyz").to_string();
                let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("").to_string();
                
                accumulated_tool_calls.push(json!({
                    "id": call_id,
                    "type": "function",
                    "function": { "name": name, "arguments": "" }
                }));

                if let Some(sink) = stream_sink {
                    send_openai_chunk(
                        sink,
                        chunk_id,
                        created,
                        model,
                        json!({ "tool_calls": [{ "index": accumulated_tool_calls.len() - 1, "id": call_id, "type": "function", "function": { "name": name, "arguments": "" } }] }),
                        None,
                        None,
                    )
                    .await;
                }
                return Ok(());
            }
        }
    }

    if event_type == "response.function_call_arguments.delta" {
        if let Some(delta) = value.get("delta").and_then(|v| v.as_str()) {
            let call_id = value.get("call_id").or_else(|| value.get("id")).and_then(|v| v.as_str()).unwrap_or("");
            let mut index = accumulated_tool_calls.len().saturating_sub(1);
            
            for (i, tc) in accumulated_tool_calls.iter_mut().enumerate().rev() {
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    if id == call_id || call_id.is_empty() {
                        if let Some(func) = tc.get_mut("function").and_then(|v| v.as_object_mut()) {
                            if let Some(args) = func.get_mut("arguments") {
                                if let Some(args_str) = args.as_str() {
                                    let mut new_args = args_str.to_string();
                                    new_args.push_str(delta);
                                    *args = Value::String(new_args);
                                }
                            }
                        }
                        index = i;
                        break;
                    }
                }
            }

            if let Some(sink) = stream_sink {
                send_openai_chunk(
                    sink,
                    chunk_id,
                    created,
                    model,
                    json!({ "tool_calls": [{ "index": index, "function": { "arguments": delta } }] }),
                    None,
                    None,
                )
                .await;
            }
        }
        return Ok(());
    }

    if let Some(delta) = value.get("delta").and_then(|v| v.as_str()) {
        if event_type.contains("reasoning") {
            accumulated_reasoning.push_str(delta);
            if let Some(sink) = stream_sink {
                send_openai_chunk(
                    sink,
                    chunk_id,
                    created,
                    model,
                    json!({ "reasoning_content": delta }),
                    None,
                    None,
                )
                .await;
            }
        } else {
            accumulated_text.push_str(delta);
            if let Some(sink) = stream_sink {
                send_openai_chunk(
                    sink,
                    chunk_id,
                    created,
                    model,
                    json!({ "content": delta }),
                    None,
                    None,
                )
                .await;
            }
        }
    }

    if event_type == "response.completed" || value.get("response").is_some() {
        if let Some(response) = value.get("response").or(Some(&value)) {
            if let Some(u) = extract_usage(response) {
                *usage = Some(u);
            }
            if accumulated_text.is_empty()
                && let Some(text) = extract_completed_text(response)
            {
                accumulated_text.push_str(&text);
            }
        }
    }

    Ok(())
}

async fn send_openai_chunk(
    sink: &StreamSink,
    chunk_id: &str,
    created: u64,
    model: &str,
    delta: Value,
    finish_reason: Option<&str>,
    usage: Option<&OpenAIUsage>,
) {
    let mut chunk = json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": finish_reason
        }]
    });
    if let Some(usage) = usage {
        chunk["usage"] = json!({
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.total_tokens
        });
    }
    let frame = crate::sse::build_sse_frame(&serde_json::to_string(&chunk).unwrap_or_default());
    let _ = sink.send(frame).await;
}

fn extract_usage(response: &Value) -> Option<OpenAIUsage> {
    let usage = response.get("usage")?;
    let prompt = usage
        .get("input_tokens")
        .or_else(|| usage.get("prompt_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let completion = usage
        .get("output_tokens")
        .or_else(|| usage.get("completion_tokens"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0) as u32;
    let total = usage
        .get("total_tokens")
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or_else(|| prompt.saturating_add(completion));
    Some(OpenAIUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
    })
}

fn extract_completed_text(response: &Value) -> Option<String> {
    let mut out = String::new();
    collect_text_fields(response, &mut out);
    if out.is_empty() { None } else { Some(out) }
}

fn collect_text_fields(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            if matches!(
                map.get("type").and_then(|v| v.as_str()),
                Some("output_text" | "text")
            ) && let Some(text) = map.get("text").and_then(|v| v.as_str())
            {
                out.push_str(text);
            }
            for value in map.values() {
                collect_text_fields(value, out);
            }
        }
        Value::Array(values) => {
            for value in values {
                collect_text_fields(value, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(model: &str) -> OpenAIRequest {
        OpenAIRequest {
            model: model.to_string(),
            messages: vec![OpenAIMessage {
                role: "system".to_string(),
                content: Some(Value::String("Be concise".to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            }],
            stream: false,
            temperature: None,
            max_tokens: Some(200),
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
    fn build_request_strips_effort_suffix() {
        let body = build_codex_request(&req("gpt-5.5-xhigh")).unwrap();
        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
    }

    #[test]
    fn extracts_usage_from_responses_shape() {
        let response = json!({
            "usage": { "input_tokens": 10, "output_tokens": 4, "total_tokens": 14 }
        });
        let usage = extract_usage(&response).unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 4);
        assert_eq!(usage.total_tokens, 14);
    }
}
