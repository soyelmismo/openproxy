//! Antigravity (Cloud Code) chat executor.
//!
//! Translates an OpenAI-shaped request into a Gemini request, wraps it
//! in a Cloud Code envelope, POSTs it to
//! `daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse`,
//! and parses the SSE stream back into an `OpenAIResponse`.
//!
//! # Protocol
//!
//! ```text
//! POST https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse
//! Authorization: Bearer <access_token>
//! Content-Type: application/json
//! Accept: text/event-stream
//!
//! {
//!   "project": "<project_id>",
//!   "model": "gemini-2.5-pro",
//!   "requestType": "agent",
//!   "requestId": "<uuid>",
//!   "userAgent": "antigravity",
//!   "request": <GeminiRequest JSON>,
//!   "enabledCreditTypes": ["GOOGLE_ONE_AI"]
//! }
//! ```
//!
//! The response is an SSE stream where each event can carry either a
//! markdown chunk, a Gemini-format chunk, a credits update, or a
//! `[DONE]` sentinel.

use openproxy_types::error::CoreError;
use openproxy_types::ids::AccountId;
use openproxy_types::{OpenAIMessage, OpenAIRequest};
use crate::translation::{OpenAIChoice, OpenAIResponse, OpenAIUsage};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest,
    UpstreamResponse,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

// ---------------------------------------------------------------------------
// Signature Cache implementation (aligned with Antigravity-Manager)
// ---------------------------------------------------------------------------

const SIGNATURE_TTL: Duration = Duration::from_secs(2 * 60 * 60);
const MIN_SIGNATURE_LENGTH: usize = 50;
const SESSION_CACHE_LIMIT: usize = 1000;

#[derive(Clone, Debug)]
struct CacheEntry<T> {
    data: T,
    timestamp: SystemTime,
}

impl<T> CacheEntry<T> {
    fn new(data: T) -> Self {
        Self {
            data,
            timestamp: SystemTime::now(),
        }
    }

    fn is_expired(&self) -> bool {
        self.timestamp.elapsed().unwrap_or(Duration::ZERO) > SIGNATURE_TTL
    }
}

#[derive(Clone, Debug)]
struct SessionSignatureEntry {
    signature: String,
    message_count: usize,
}

pub struct SignatureCache {
    session_signatures: Mutex<HashMap<String, CacheEntry<SessionSignatureEntry>>>,
    session_reasonings: Mutex<HashMap<String, CacheEntry<Vec<String>>>>,
}

impl SignatureCache {
    fn new() -> Self {
        Self {
            session_signatures: Mutex::new(HashMap::new()),
            session_reasonings: Mutex::new(HashMap::new()),
        }
    }

    pub fn global() -> &'static SignatureCache {
        static INSTANCE: OnceLock<SignatureCache> = OnceLock::new();
        INSTANCE.get_or_init(SignatureCache::new)
    }

    pub fn cache_session_signature(
        &self,
        session_id: &str,
        signature: String,
        message_count: usize,
    ) {
        if signature.len() < MIN_SIGNATURE_LENGTH {
            return;
        }

        if let Ok(mut cache) = self.session_signatures.lock() {
            let should_store = match cache.get(session_id) {
                None => true,
                Some(existing) => {
                    if existing.is_expired() {
                        true
                    } else if message_count < existing.data.message_count {
                        // Rewind detected
                        tracing::info!(
                            "[SignatureCache] Rewind detected for {}: {} -> {} messages. Forcing signature update.",
                            session_id,
                            existing.data.message_count,
                            message_count
                        );
                        true
                    } else if message_count == existing.data.message_count {
                        signature.len() > existing.data.signature.len()
                    } else {
                        true
                    }
                }
            };

            if should_store {
                tracing::debug!(
                    "[SignatureCache] Session {} (msg_count={}) -> storing signature (len={})",
                    session_id,
                    message_count,
                    signature.len()
                );
                cache.insert(
                    session_id.to_string(),
                    CacheEntry::new(SessionSignatureEntry {
                        signature,
                        message_count,
                    }),
                );
            }

            if cache.len() > SESSION_CACHE_LIMIT {
                cache.retain(|_, v| !v.is_expired());
            }
        }
    }

    pub fn get_session_signature(&self, session_id: &str) -> Option<String> {
        if let Ok(cache) = self.session_signatures.lock()
            && let Some(entry) = cache.get(session_id)
            && !entry.is_expired()
        {
            return Some(entry.data.signature.clone());
        }
        None
    }

    pub fn cache_session_reasoning(&self, session_id: &str, reasoning: String, turn_index: usize) {
        if reasoning.trim().is_empty() {
            return;
        }

        if let Ok(mut cache) = self.session_reasonings.lock() {
            let entry = cache
                .entry(session_id.to_string())
                .or_insert_with(|| CacheEntry::new(Vec::new()));

            entry.timestamp = SystemTime::now();

            if turn_index >= entry.data.len() {
                entry.data.resize(turn_index + 1, String::new());
            }

            let old_len = entry.data[turn_index].len();
            if reasoning.len() > old_len {
                tracing::debug!(
                    "[SignatureCache] Session {} (turn={}) -> caching reasoning text (len: {} -> {})",
                    session_id,
                    turn_index,
                    old_len,
                    reasoning.len()
                );
                entry.data[turn_index] = reasoning;
            }

            if cache.len() > SESSION_CACHE_LIMIT {
                cache.retain(|_, v| !v.is_expired());
            }
        }
    }

    pub fn get_session_reasoning(&self, session_id: &str, turn_index: usize) -> Option<String> {
        if let Ok(cache) = self.session_reasonings.lock()
            && let Some(entry) = cache.get(session_id)
            && !entry.is_expired()
            && turn_index < entry.data.len()
        {
            let text = &entry.data[turn_index];
            if !text.trim().is_empty() {
                return Some(text.clone());
            }
        }
        None
    }

    #[cfg(test)]
    pub fn clear(&self) {
        if let Ok(mut cache) = self.session_signatures.lock() {
            cache.clear();
        }
        if let Ok(mut cache) = self.session_reasonings.lock() {
            cache.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// Session fingerprint and FNV-1a helpers
// ---------------------------------------------------------------------------

pub fn derive_session_id(session_fingerprint: &str) -> String {
    let mut hash: i64 = -3750763034362895579_i64; // FNV offset basis
    for byte in session_fingerprint.bytes() {
        hash = hash.wrapping_mul(1099511628211_i64);
        hash ^= byte as i64;
    }
    hash.to_string()
}

pub fn extract_openai_session_id(request: &OpenAIRequest) -> String {
    let mut hasher = Sha256::new();
    let mut content_found = false;

    for msg in &request.messages {
        if msg.role != "user" {
            continue;
        }
        if let Some(content_val) = &msg.content {
            let text = match content_val {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Array(arr) => {
                    let mut parts = Vec::new();
                    for part in arr {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            parts.push(t);
                        }
                    }
                    parts.join(" ")
                }
                other => other.to_string(),
            };

            let clean_text = text.trim();
            if clean_text.len() > 10 && !clean_text.contains("<system-reminder>") {
                hasher.update(clean_text.as_bytes());
                content_found = true;
                break;
            }
        }
    }

    if !content_found && let Some(last_msg) = request.messages.last() {
        hasher.update(format!("{:?}", last_msg.content).as_bytes());
    }

    let result = hasher.finalize();
    let hash = result
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect::<String>();
    let sid = format!("sid-{}", &hash[..16]);
    tracing::debug!("[Antigravity] Generated session fingerprint: {}", sid);
    sid
}

fn deep_clean_cache_control(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(map) => {
            map.remove("cache_control");
            for (_, v) in map.iter_mut() {
                deep_clean_cache_control(v);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr.iter_mut() {
                deep_clean_cache_control(item);
            }
        }
        _ => {}
    }
}

fn message_content_to_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(parts) => {
            let mut texts = Vec::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                    texts.push(text);
                }
            }
            texts.join("")
        }
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Cloud Code envelope types
// ---------------------------------------------------------------------------

/// Cloud Code request envelope for Antigravity.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityRequestEnvelope {
    project: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    request_type: String,
    request_id: String,
    user_agent: String,
    request: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    enabled_credit_types: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// SSE chunk types
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityGeminiResponse {
    candidates: Option<Vec<AntigravityCandidate>>,
    usage_metadata: Option<AntigravityUsageMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityCandidate {
    content: Option<AntigravityContent>,
}

#[derive(Debug, Deserialize)]
struct AntigravityContent {
    parts: Option<Vec<AntigravityPart>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityPart {
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
    #[serde(alias = "thought_signature")]
    thought_signature: Option<String>,
    function_call: Option<AntigravityFunctionCall>,
}

#[derive(Debug, Deserialize)]
struct AntigravityFunctionCall {
    name: String,
    args: serde_json::Value,
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityUsageMetadata {
    prompt_token_count: Option<u32>,
    candidates_token_count: Option<u32>,
    total_token_count: Option<u32>,
}

enum ChunkKind {
    Markdown { text: String },
    Gemini { response: AntigravityGeminiResponse },
    Credits,
    Skip,
}

fn classify_chunk(v: &serde_json::Value) -> ChunkKind {
    if let Some(text) = v.get("markdown").and_then(|m| m.as_str()) {
        return ChunkKind::Markdown {
            text: text.to_string(),
        };
    }

    if let Some(resp) = v.get("response") {
        if let Some(candidates) = resp.get("candidates")
            && candidates.is_array()
            && let Ok(response) = serde_json::from_value::<AntigravityGeminiResponse>(resp.clone())
        {
            return ChunkKind::Gemini { response };
        }
        if let Some(text) = resp.get("markdown").and_then(|m| m.as_str()) {
            return ChunkKind::Markdown {
                text: text.to_string(),
            };
        }
    }

    if v.get("remainingCreditTypes").is_some() || v.get("remaining_credit_types").is_some() {
        return ChunkKind::Credits;
    }

    ChunkKind::Skip
}

// ---------------------------------------------------------------------------
// Account metadata
// ---------------------------------------------------------------------------


// ---------------------------------------------------------------------------
// Helpers for Tool Formatting
// ---------------------------------------------------------------------------

fn qualify_namespace_tool_name(namespace_name: &str, child_name: &str) -> String {
    let child = child_name.trim();
    let ns = namespace_name.trim();
    if child.is_empty() || ns.is_empty() || child.starts_with("mcp__") {
        return child.to_string();
    }
    if child.starts_with(ns) {
        return child.to_string();
    }
    if ns.ends_with("__") {
        return format!("{}{}", ns, child);
    }
    format!("{}__{}", ns, child)
}

fn flatten_tools(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    let mut flat = Vec::new();
    for tool in tools {
        let t = tool.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if t == "namespace" {
            let namespace_name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(sub_tools) = tool.get("tools").and_then(|v| v.as_array()) {
                let sub_flat = flatten_tools(sub_tools);
                for mut sub_tool in sub_flat {
                    if let Some(obj) = sub_tool.as_object_mut() {
                        let mut name = String::new();
                        if let Some(n) = obj.get("name").and_then(|v| v.as_str()) {
                            name = n.to_string();
                        } else if let Some(func) = obj.get("function")
                            && let Some(n) = func.get("name").and_then(|v| v.as_str())
                        {
                            name = n.to_string();
                        }
                        if !name.is_empty() {
                            let qualified = qualify_namespace_tool_name(namespace_name, &name);
                            if obj.contains_key("name") {
                                obj.insert("name".to_string(), serde_json::json!(qualified));
                            }
                            if let Some(func) = obj.get_mut("function")
                                && let Some(func_obj) = func.as_object_mut()
                            {
                                func_obj.insert("name".to_string(), serde_json::json!(qualified));
                            }
                        }
                    }
                    flat.push(sub_tool);
                }
            }
        } else {
            flat.push(tool.clone());
        }
    }
    flat
}

fn enforce_uppercase_types(value: &mut serde_json::Value) {
    if let serde_json::Value::Object(map) = value {
        if let Some(type_val) = map.get_mut("type")
            && let serde_json::Value::String(s) = type_val
        {
            *s = s.to_uppercase();
        }
        if let Some(properties) = map.get_mut("properties")
            && let serde_json::Value::Object(props) = properties
        {
            for v in props.values_mut() {
                enforce_uppercase_types(v);
            }
        }
        if let Some(items) = map.get_mut("items") {
            enforce_uppercase_types(items);
        }
    } else if let serde_json::Value::Array(arr) = value {
        for item in arr {
            enforce_uppercase_types(item);
        }
    }
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

fn openai_to_gemini_antigravity(
    openai: &OpenAIRequest,
    session_id: &str,
    model_name: &str,
) -> serde_json::Value {
    let mut system_parts = Vec::new();
    let mut messages = Vec::new();

    let mut openai_messages = openai.messages.clone();

    // Restore reasoning_content from cache
    let mut assistant_turn_index = 0;
    for msg in &mut openai_messages {
        if msg.role == "assistant" {
            let has_reasoning = msg
                .extra
                .get("reasoning_content")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty() && s != "[undefined]")
                .unwrap_or(false);
            if !has_reasoning
                && let Some(cached_reasoning) =
                    SignatureCache::global().get_session_reasoning(session_id, assistant_turn_index)
            {
                tracing::debug!(
                    "[Antigravity] Restored reasoning for assistant turn {} (len: {})",
                    assistant_turn_index,
                    cached_reasoning.len()
                );
                msg.extra.insert(
                    "reasoning_content".to_string(),
                    serde_json::json!(cached_reasoning),
                );
            }
            assistant_turn_index += 1;
        }
    }

    // Separate system and dialog messages
    for msg in &openai_messages {
        if msg.role == "system" || msg.role == "developer" {
            if let Some(content) = &msg.content {
                system_parts.push(message_content_to_text(content));
            }
        } else {
            messages.push(msg.clone());
        }
    }

    // Deep clean cache_control from all messages
    for msg in &mut messages {
        if let Some(content) = &mut msg.content {
            deep_clean_cache_control(content);
        }
    }

    let has_tool_history = openai_messages
        .iter()
        .any(|msg| msg.role == "tool" || msg.role == "function" || msg.tool_calls.is_some());

    let mut tool_id_to_name = std::collections::HashMap::new();
    for msg in &openai_messages {
        if let Some(tool_calls) = &msg.tool_calls {
            for tc in tool_calls {
                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !name.is_empty() {
                        tool_id_to_name.insert(id.to_string(), name.to_string());
                    }
                }
            }
        }
    }

    // Merge consecutive messages with the same role
    let mut grouped_messages: Vec<(String, Vec<serde_json::Value>)> = Vec::new();

    for msg in &messages {
        let gemini_role = match msg.role.as_str() {
            "assistant" => "model",
            _ => "user", // "user", "tool", "function"
        };

        let mut parts = Vec::new();

        if msg.role == "tool" || msg.role == "function" {
            let tool_call_id = msg.tool_call_id.clone().unwrap_or_default();
            let mut final_name = msg.name.as_deref().unwrap_or("unknown").to_string();
            if final_name == "unknown"
                && !tool_call_id.is_empty()
                && let Some(mapped) = tool_id_to_name.get(&tool_call_id)
            {
                final_name = mapped.clone();
            }

            let content_str = match &msg.content {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => String::new(),
            };
            let mut function_response = serde_json::json!({
                "name": final_name,
                "response": { "result": content_str }
            });
            if !tool_call_id.is_empty() {
                function_response["id"] = serde_json::json!(tool_call_id);
            }
            parts.push(serde_json::json!({
                "functionResponse": function_response
            }));
        } else {
            // Check for assistant's reasoning_content and prepend it as a thinking part
            if msg.role == "assistant"
                && let Some(reasoning) = msg.extra.get("reasoning_content").and_then(|v| v.as_str())
                && !reasoning.is_empty()
                && !has_tool_history
            {
                let mut thinking_part = serde_json::json!({
                    "text": reasoning,
                    "thought": true
                });

                // Inject signature if available
                if let Some(sig) = SignatureCache::global().get_session_signature(session_id) {
                    thinking_part["thoughtSignature"] = serde_json::json!(sig);
                    thinking_part["thought_signature"] = serde_json::json!(sig);
                } else {
                    let model_lower = model_name.to_lowercase();
                    let is_thinking = model_lower.contains("gemini")
                        && (model_lower.contains("-thinking")
                            || model_lower.contains("gemini-2.0-pro")
                            || model_lower.contains("gemini-3-pro")
                            || model_lower.contains("gemini-3.1-pro"))
                        && !model_lower.contains("claude");
                    let is_flash = model_lower.contains("flash");
                    if is_thinking || is_flash {
                        thinking_part["thoughtSignature"] =
                            serde_json::json!("skip_thought_signature_validator");
                        thinking_part["thought_signature"] =
                            serde_json::json!("skip_thought_signature_validator");
                    }
                }

                parts.push(thinking_part);
            }

            // Normal text content part
            if let Some(content_val) = &msg.content {
                match content_val {
                    serde_json::Value::Array(arr) => {
                        for part in arr {
                            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                parts.push(serde_json::json!({ "text": text }));
                            }
                        }
                    }
                    serde_json::Value::String(s) => {
                        if !s.is_empty() {
                            parts.push(serde_json::json!({ "text": s }));
                        }
                    }
                    other => {
                        let s = other.to_string();
                        if !s.is_empty() {
                            parts.push(serde_json::json!({ "text": s }));
                        }
                    }
                }
            }

            // Assistant tool calls (functionCall)
            if msg.role == "assistant"
                && let Some(tool_calls) = &msg.tool_calls
            {
                for tc in tool_calls {
                    let id = tc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = tc
                        .get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    let arguments_str = tc
                        .get("function")
                        .and_then(|f| f.get("arguments"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}");
                    let arguments: serde_json::Value =
                        serde_json::from_str(arguments_str).unwrap_or(serde_json::json!({}));

                    let mut func_call_part = serde_json::json!({
                        "functionCall": {
                            "name": name,
                            "args": arguments
                        }
                    });

                    if !id.is_empty() {
                        func_call_part["functionCall"]["id"] = serde_json::json!(id);
                    }

                    // Inject signature if available
                    if let Some(sig) = SignatureCache::global().get_session_signature(session_id) {
                        func_call_part["thoughtSignature"] = serde_json::json!(sig);
                        func_call_part["thought_signature"] = serde_json::json!(sig);
                    } else {
                        let model_lower = model_name.to_lowercase();
                        let is_thinking = model_lower.contains("gemini")
                            && (model_lower.contains("-thinking")
                                || model_lower.contains("gemini-2.0-pro")
                                || model_lower.contains("gemini-3-pro")
                                || model_lower.contains("gemini-3.1-pro"))
                            && !model_lower.contains("claude");
                        let is_flash = model_lower.contains("flash");
                        if is_thinking || is_flash {
                            func_call_part["thoughtSignature"] =
                                serde_json::json!("skip_thought_signature_validator");
                            func_call_part["thought_signature"] =
                                serde_json::json!("skip_thought_signature_validator");
                        }
                    }

                    parts.push(func_call_part);
                }
            }
        }

        if parts.is_empty() {
            continue;
        }

        // Merge consecutive messages of the same role
        if let Some((prev_role, prev_parts)) = grouped_messages.last_mut()
            && prev_role == gemini_role
        {
            prev_parts.extend(parts);
            continue;
        }

        grouped_messages.push((gemini_role.to_string(), parts));
    }

    let mut contents: Vec<serde_json::Value> = grouped_messages
        .into_iter()
        .map(|(role, parts)| {
            serde_json::json!({
                "role": role,
                "parts": parts
            })
        })
        .collect();

    if contents.is_empty() {
        contents.push(serde_json::json!({
            "role": "user",
            "parts": [{ "text": "Continue" }]
        }));
    }

    // Build systemInstruction
    let system_instruction = if system_parts.is_empty() {
        None
    } else {
        Some(serde_json::json!({
            "role": "user",
            "parts": [
                {
                    "text": system_parts.join("\n\n")
                }
            ]
        }))
    };

    // Build generationConfig
    let mut gen_config = serde_json::json!({});

    let model_lower = model_name.to_lowercase();
    let is_thinking = model_lower.contains("gemini")
        && (model_lower.contains("-thinking")
            || model_lower.contains("gemini-2.0-pro")
            || model_lower.contains("gemini-3-pro")
            || model_lower.contains("gemini-3.1-pro"))
        && !model_lower.contains("claude");

    if is_thinking {
        let budget = 4096;
        gen_config["thinkingConfig"] = serde_json::json!({
            "includeThoughts": true,
            "thinkingBudget": budget
        });

        if let Some(max_tokens) = openai.max_tokens {
            if (max_tokens as i64) <= budget {
                gen_config["maxOutputTokens"] = serde_json::json!(8192);
            } else {
                gen_config["maxOutputTokens"] = serde_json::json!(max_tokens.min(8192));
            }
        } else {
            gen_config["maxOutputTokens"] = serde_json::json!(8192);
        }
    } else {
        gen_config["maxOutputTokens"] =
            serde_json::json!(openai.max_tokens.unwrap_or(8192).min(8192));
    }
    if let Some(temp) = openai.temperature {
        gen_config["temperature"] = serde_json::json!(temp);
    }
    if let Some(top_p) = openai.top_p {
        gen_config["topP"] = serde_json::json!(top_p);
    }
    if let Some(top_k) = openai.top_k {
        gen_config["topK"] = serde_json::json!(top_k);
    }
    if let Some(n) = openai.extra.get("n") {
        gen_config["candidateCount"] = n.clone();
    }
    if let Some(seed) = openai.extra.get("seed") {
        gen_config["seed"] = seed.clone();
    }
    if let Some(stop) = &openai.stop {
        gen_config["stopSequences"] = serde_json::json!(stop);
    }

    let mut request_val = serde_json::json!({
        "contents": contents,
        "generationConfig": gen_config,
        "safetySettings": []
    });

    if let Some(si) = system_instruction {
        request_val["systemInstruction"] = si;
    }

    if let Some(original_tools) = &openai.tools {
        let mut function_declarations = Vec::new();
        let tools = flatten_tools(original_tools);
        for tool in tools.iter() {
            let mut gemini_func = if let Some(func) = tool.get("function") {
                func.clone()
            } else {
                let mut func = tool.clone();
                if func.get("name").is_none() {
                    let tool_type_opt = func
                        .get("type")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    if let Some(tool_type) = tool_type_opt
                        && let Some(obj) = func.as_object_mut()
                    {
                        obj.insert("name".to_string(), serde_json::json!(tool_type));
                    }
                }
                if let Some(obj) = func.as_object_mut() {
                    let mut clean_obj = serde_json::Map::new();
                    if let Some(name) = obj.get("name") {
                        clean_obj.insert("name".to_string(), name.clone());
                    }
                    if let Some(desc) = obj.get("description") {
                        clean_obj.insert("description".to_string(), desc.clone());
                    }
                    if let Some(params) = obj.get("parameters") {
                        clean_obj.insert("parameters".to_string(), params.clone());
                    }
                    *obj = clean_obj;
                }
                func
            };

            let name_opt = gemini_func
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            if let Some(name) = &name_opt {
                if name == "web_search"
                    || name == "google_search"
                    || name == "web_search_20250305"
                    || name == "builtin_web_search"
                {
                    continue;
                }
                if name == "local_shell_call"
                    && let Some(obj) = gemini_func.as_object_mut()
                {
                    obj.insert("name".to_string(), serde_json::json!("shell"));
                }
            } else {
                continue;
            }

            if let Some(obj) = gemini_func.as_object_mut() {
                let mut clean_obj = serde_json::Map::new();
                if let Some(name) = obj.get("name") {
                    clean_obj.insert("name".to_string(), name.clone());
                }
                if let Some(desc) = obj.get("description") {
                    clean_obj.insert("description".to_string(), desc.clone());
                }
                if let Some(params) = obj.get("parameters") {
                    clean_obj.insert("parameters".to_string(), params.clone());
                }
                *obj = clean_obj;
            }

            if let Some(params) = gemini_func.get_mut("parameters") {
                crate::schema_cleaner::clean_json_schema(params);
                if let Some(params_obj) = params.as_object_mut()
                    && !params_obj.contains_key("type")
                {
                    params_obj.insert("type".to_string(), serde_json::json!("OBJECT"));
                }
                enforce_uppercase_types(params);
            } else {
                if gemini_func.get("name").and_then(|v| v.as_str()) == Some("apply_patch") {
                    gemini_func.as_object_mut().unwrap().insert(
                        "parameters".to_string(),
                        serde_json::json!({
                            "type": "OBJECT",
                            "properties": {
                                "command": {
                                    "type": "ARRAY",
                                    "items": {
                                        "type": "STRING"
                                    },
                                    "description": "The command array. First element MUST be 'apply_patch', second element MUST be the exact freeform patch string starting with *** Begin Patch"
                                }
                            },
                            "required": ["command"]
                        }),
                    );
                } else {
                    gemini_func.as_object_mut().unwrap().insert(
                        "parameters".to_string(),
                        serde_json::json!({
                            "type": "OBJECT",
                            "properties": {
                                "content": {
                                    "type": "STRING",
                                    "description": "The raw content or patch to be applied"
                                }
                            },
                            "required": ["content"]
                        }),
                    );
                }
            }
            function_declarations.push(gemini_func);
        }

        if !function_declarations.is_empty() {
            request_val["tools"] =
                serde_json::json!([{ "functionDeclarations": function_declarations }]);

            let mut mode = "VALIDATED";
            if let Some(tool_choice) = &openai.tool_choice {
                if let Some(s) = tool_choice.as_str() {
                    match s {
                        "none" => mode = "NONE",
                        "auto" => mode = "AUTO",
                        "required" => mode = "ANY",
                        _ => mode = "ANY",
                    }
                } else {
                    mode = "ANY";
                }
            }

            request_val["toolConfig"] = serde_json::json!({
                "functionCallingConfig": { "mode": mode }
            });
        }
    }

    request_val
}

fn build_antigravity_request(
    openai: &OpenAIRequest,
    project_id: &str,
    session_id: &str,
) -> Result<AntigravityRequestEnvelope, CoreError> {
    let model_name = openai.model.split('/').next_back().unwrap_or(&openai.model);

    let mut gemini = openai_to_gemini_antigravity(openai, session_id, model_name);

    // Inject stable sessionId
    gemini["sessionId"] = serde_json::json!(derive_session_id(session_id));

    Ok(AntigravityRequestEnvelope {
        project: project_id.to_string(),
        model: Some(model_name.to_string()),
        request_type: "agent".to_string(),
        request_id: Uuid::new_v4().to_string(),
        user_agent: "antigravity".to_string(),
        request: gemini,
        enabled_credit_types: Some(vec!["GOOGLE_ONE_AI".to_string()]),
    })
}

pub(crate) fn build_antigravity_envelope_value(
    openai: &OpenAIRequest,
    project_id: &str,
    session_id: &str,
) -> Result<serde_json::Value, CoreError> {
    let envelope = build_antigravity_request(openai, project_id, session_id)?;
    serde_json::to_value(envelope)
        .map_err(|e| CoreError::Internal(format!("failed to serialize envelope: {e}")))
}

pub(crate) async fn call_antigravity_v1internal(
    upstream_client: &Arc<UpstreamClient>,
    url: &str,
    access_token: &str,
    body_bytes: bytes::Bytes,
    timeout: TimeoutProfile,
    cancel: CancellationToken,
    accept_sse: bool,
    proxy: Option<String>,
) -> std::result::Result<UpstreamResponse, UpstreamError> {
    let build_request = || {
        let mut req = UpstreamRequest::post_json(url.to_string(), body_bytes.clone());
        req.proxy = proxy.clone();
        if let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
            req.headers.insert(http::header::AUTHORIZATION, value);
        }
        if accept_sse {
            req.headers.insert(
                http::header::ACCEPT,
                http::HeaderValue::from_static("text/event-stream"),
            );
        }
        openproxy_adapters::antigravity_headers::inject_antigravity_headers(&mut req.headers, None);
        req
    };

    let response = upstream_client
        .call(build_request(), timeout, cancel)
        .await?;

    Ok(response)
}

// ---------------------------------------------------------------------------
// SSE parsing
// ---------------------------------------------------------------------------

#[cfg(test)]
pub fn parse_antigravity_line(
    line: &str,
    accumulated_text: &mut String,
    usage: &mut Option<OpenAIUsage>,
) -> Result<bool, CoreError> {
    let mut thinking_chunk = String::new();
    let mut tool_calls = Vec::new();
    parse_antigravity_line_with_parts(
        line,
        accumulated_text,
        &mut thinking_chunk,
        &mut tool_calls,
        usage,
        "dummy_session",
        0,
    )
}

pub fn parse_antigravity_sse_line(
    line: &str,
    chunk_id: &str,
    created: u64,
    model: &str,
) -> Result<Option<crate::sse::UpstreamSseChunk>, CoreError> {
    let mut text_chunk = String::new();
    let mut thinking_chunk = String::new();
    let mut tool_calls = Vec::new();
    let mut usage = None;
    
    let is_done = parse_antigravity_line_with_parts(
        line,
        &mut text_chunk,
        &mut thinking_chunk,
        &mut tool_calls,
        &mut usage,
        chunk_id, // we use chunk_id as session_id for simplicity since we don't track true session here
        0,
    )?;

    if is_done {
        return Ok(Some(crate::sse::UpstreamSseChunk {
            raw_payload: None,
            payload: serde_json::Value::Null,
            done: true,
            usage,
            stop_reason: Some("stop".to_string()),
            delta_reasoning: None,
            delta_tool_calls: Vec::new(),
            has_content: true,
        }));
    }

    if text_chunk.is_empty() && thinking_chunk.is_empty() && tool_calls.is_empty() {
        return Ok(None);
    }

    // Map to OpenAI chunk format internally, we just use the payload field so the serializer works.
    let mut delta = serde_json::json!({});
    if !text_chunk.is_empty() {
        delta["content"] = serde_json::Value::String(text_chunk);
    }
    if !tool_calls.is_empty() {
        delta["tool_calls"] = serde_json::Value::Array(tool_calls.clone());
    }

    let payload = serde_json::json!({
        "id": chunk_id,
        "object": "chat.completion.chunk",
        "created": created,
        "model": model,
        "choices": [{
            "index": 0,
            "delta": delta,
            "finish_reason": serde_json::Value::Null
        }]
    });

    Ok(Some(crate::sse::UpstreamSseChunk {
        raw_payload: None,
        payload,
        done: false,
        usage,
        stop_reason: None,
        delta_reasoning: if thinking_chunk.is_empty() { None } else { Some(thinking_chunk) },
        delta_tool_calls: tool_calls,
        has_content: true,
    }))
}

fn parse_antigravity_line_with_parts(
    line: &str,
    text_chunk: &mut String,
    thinking_chunk: &mut String,
    tool_calls: &mut Vec<serde_json::Value>,
    usage: &mut Option<OpenAIUsage>,
    session_id: &str,
    message_count: usize,
) -> Result<bool, CoreError> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with(':') {
        return Ok(false);
    }

    let json_str = if let Some(rest) = trimmed.strip_prefix("data: ") {
        rest
    } else {
        trimmed
    };

    if json_str == "[DONE]" {
        return Ok(true);
    }

    let v: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| CoreError::Internal(format!("failed to parse chunk: {e}")))?;

    if v.get("done").and_then(|d| d.as_bool()) == Some(true) {
        return Ok(true);
    }

    match classify_chunk(&v) {
        ChunkKind::Credits | ChunkKind::Skip => Ok(false),
        ChunkKind::Markdown { text } => {
            text_chunk.push_str(&text);
            Ok(false)
        }
        ChunkKind::Gemini { response } => {
            if let Some(candidates) = response.candidates {
                for candidate in candidates {
                    if let Some(content) = candidate.content
                        && let Some(parts) = content.parts
                    {
                        for part in parts {
                            // Extract signature
                            if let Some(sig) = &part.thought_signature {
                                SignatureCache::global().cache_session_signature(
                                    session_id,
                                    sig.clone(),
                                    message_count,
                                );
                            }

                            // Extract function calls
                            if let Some(fc) = &part.function_call {
                                let arguments_str = serde_json::to_string(&fc.args)
                                    .unwrap_or_else(|_| "{}".to_string());
                                let tc_val = serde_json::json!({
                                    "index": 0,
                                    "id": fc.id.as_deref().unwrap_or(""),
                                    "type": "function",
                                    "function": {
                                        "name": &fc.name,
                                        "arguments": arguments_str
                                    }
                                });
                                tool_calls.push(tc_val);
                            }

                            // Extract text
                            if let Some(text) = &part.text {
                                if part.thought == Some(true) {
                                    thinking_chunk.push_str(text);
                                } else {
                                    text_chunk.push_str(text);
                                }
                            }
                        }
                    }
                }
            }
            if let Some(um) = response.usage_metadata {
                *usage = Some(OpenAIUsage {
                    prompt_tokens: um.prompt_token_count.unwrap_or(0),
                    completion_tokens: um.candidates_token_count.unwrap_or(0),
                    total_tokens: um.total_token_count.unwrap_or(0),
                });
            }
            Ok(false)
        }
    }
}

// ---------------------------------------------------------------------------
// Public executor
// ---------------------------------------------------------------------------

pub async fn execute_antigravity(
    upstream_client: &Arc<UpstreamClient>,
    url: &str,
    access_token: &str,
    project_id: &str,
    openai: &OpenAIRequest,
    client_disconnected: watch::Receiver<bool>,
    stream_sink: Option<&crate::StreamSink>,
    proxy: Option<String>,
) -> Result<OpenAIResponse, CoreError> {
    // 1. Session ID and fingerprint derivation
    let session_id = extract_openai_session_id(openai);
    let message_count = openai.messages.len();
    let assistant_turn_index = openai
        .messages
        .iter()
        .filter(|m| m.role == "assistant")
        .count();

    // 2. Build Cloud Code envelope
    let body = build_antigravity_envelope_value(openai, project_id, &session_id)?;
    let body_bytes = serde_json::to_vec(&body)
        .map_err(|e| CoreError::Internal(format!("failed to serialize envelope: {e}")))?;

    // 3. Build the upstream request

    let cancel = CancellationToken::from_watch(client_disconnected);

    let response = match call_antigravity_v1internal(
        upstream_client,
        url,
        access_token,
        bytes::Bytes::from(body_bytes),
        TimeoutProfile::Chat,
        cancel,
        true,
        proxy,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            return Err(match e {
                UpstreamError::Cancel => CoreError::ClientDisconnected,
                other => {
                    CoreError::UpstreamConnection(format!("antigravity request failed: {other}"))
                }
            });
        }
    };

    let status = response.status.as_u16();
    if !(200..300).contains(&status) {
        let body_bytes = response.collect().await.unwrap_or_default();
        let body_text = String::from_utf8_lossy(&body_bytes);

        if status == 429 && body_text.contains("INSUFFICIENT_G1_CREDITS_BALANCE") {
            return Err(CoreError::RateLimited {
                provider: "antigravity".to_string(),
                // Superar los 15s fuerza la rotación instantánea abortando los reintentos locales.
                // El tiempo real de reinicio de la cuota (y el modelo) es trackeado y gobernado
                // globalmente por el background sync en el sistema de cuotas.
                retry_after_ms: u32::MAX as u64,
                is_proxy_rotated: false,
            });
        }

        return Err(CoreError::UpstreamError {
            status,
            provider: "antigravity".to_string(),
            model: openai.model.clone(),
            body: body_text.to_string(),
            is_proxy_rotated: false,
        });
    }

    // 4. Stream reading and parsing loop
    let mut accumulated_text = String::new();
    let mut accumulated_thinking = String::new();
    let mut accumulated_tool_calls = Vec::new();
    let mut usage: Option<OpenAIUsage> = None;
    let mut line_buffer = String::new();

    let created = chrono::Utc::now().timestamp() as u64;
    let chunk_id = format!("chatcmpl-{}", Uuid::new_v4());

    // Send the initial role: "assistant" chunk if streaming is active
    if let Some(sink) = stream_sink {
        let role_chunk = serde_json::json!({
            "id": &chunk_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": &openai.model,
            "choices": [{
                "index": 0,
                "delta": { "role": "assistant" },
                "finish_reason": serde_json::Value::Null
            }]
        });
        let sse_frame = crate::sse::build_sse_frame(&serde_json::to_string(&role_chunk).unwrap());
        let _ = sink.send(sse_frame).await;
    }

    let mut body_stream = response.body;

    loop {
        let chunk_opt = match body_stream.next_chunk().await {
            Ok(c) => c,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => CoreError::UpstreamConnection(format!(
                        "failed to read antigravity response chunk: {other}"
                    )),
                });
            }
        };

        let chunk = match chunk_opt {
            Some(c) => {
                body_stream.note_content_chunk();
                c
            }
            None => break, // EOF
        };

        let text_part = String::from_utf8_lossy(&chunk);
        line_buffer.push_str(&text_part);

        while let Some(pos) = line_buffer.find('\n') {
            let line = line_buffer[..pos].to_string();
            line_buffer = line_buffer[pos + 1..].to_string();

            let mut text_chunk = String::new();
            let mut thinking_chunk = String::new();
            let mut tool_calls = Vec::new();

            let done = parse_antigravity_line_with_parts(
                &line,
                &mut text_chunk,
                &mut thinking_chunk,
                &mut tool_calls,
                &mut usage,
                &session_id,
                message_count,
            )?;

            if !text_chunk.is_empty() {
                accumulated_text.push_str(&text_chunk);
            }
            if !thinking_chunk.is_empty() {
                accumulated_thinking.push_str(&thinking_chunk);
            }
            if !tool_calls.is_empty() {
                accumulated_tool_calls.extend(tool_calls.clone());
            }

            // Stream chunk updates to the client
            if let Some(sink) = stream_sink {
                if !text_chunk.is_empty() {
                    let text_delta = serde_json::json!({
                        "id": &chunk_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": &openai.model,
                        "choices": [{
                            "index": 0,
                            "delta": { "content": &text_chunk },
                            "finish_reason": serde_json::Value::Null
                        }]
                    });
                    let sse_frame =
                        crate::sse::build_sse_frame(&serde_json::to_string(&text_delta).unwrap());
                    let _ = sink.send(sse_frame).await;
                }

                if !thinking_chunk.is_empty() {
                    let thinking_delta = serde_json::json!({
                        "id": &chunk_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": &openai.model,
                        "choices": [{
                            "index": 0,
                            "delta": { "reasoning_content": &thinking_chunk },
                            "finish_reason": serde_json::Value::Null
                        }]
                    });
                    let sse_frame = crate::sse::build_sse_frame(
                        &serde_json::to_string(&thinking_delta).unwrap(),
                    );
                    let _ = sink.send(sse_frame).await;
                }

                for tc in &tool_calls {
                    let tool_call_delta = serde_json::json!({
                        "id": &chunk_id,
                        "object": "chat.completion.chunk",
                        "created": created,
                        "model": &openai.model,
                        "choices": [{
                            "index": 0,
                            "delta": {
                                "tool_calls": [tc]
                            },
                            "finish_reason": serde_json::Value::Null
                        }]
                    });
                    let sse_frame = crate::sse::build_sse_frame(
                        &serde_json::to_string(&tool_call_delta).unwrap(),
                    );
                    let _ = sink.send(sse_frame).await;
                }
            }

            if done {
                break;
            }
        }
    }

    // Process remainder of buffer
    if !line_buffer.is_empty() {
        let mut text_chunk = String::new();
        let mut thinking_chunk = String::new();
        let mut tool_calls = Vec::new();
        let _ = parse_antigravity_line_with_parts(
            &line_buffer,
            &mut text_chunk,
            &mut thinking_chunk,
            &mut tool_calls,
            &mut usage,
            &session_id,
            message_count,
        )?;

        if !text_chunk.is_empty() {
            accumulated_text.push_str(&text_chunk);
        }
        if !thinking_chunk.is_empty() {
            accumulated_thinking.push_str(&thinking_chunk);
        }
        if !tool_calls.is_empty() {
            accumulated_tool_calls.extend(tool_calls.clone());
        }

        if let Some(sink) = stream_sink {
            if !text_chunk.is_empty() {
                let text_delta = serde_json::json!({
                    "id": &chunk_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": &openai.model,
                    "choices": [{
                        "index": 0,
                        "delta": { "content": &text_chunk },
                        "finish_reason": serde_json::Value::Null
                    }]
                });
                let sse_frame =
                    crate::sse::build_sse_frame(&serde_json::to_string(&text_delta).unwrap());
                let _ = sink.send(sse_frame).await;
            }
            if !thinking_chunk.is_empty() {
                let thinking_delta = serde_json::json!({
                    "id": &chunk_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": &openai.model,
                    "choices": [{
                        "index": 0,
                        "delta": { "reasoning_content": &thinking_chunk },
                        "finish_reason": serde_json::Value::Null
                    }]
                });
                let sse_frame =
                    crate::sse::build_sse_frame(&serde_json::to_string(&thinking_delta).unwrap());
                let _ = sink.send(sse_frame).await;
            }
            for tc in &tool_calls {
                let tool_call_delta = serde_json::json!({
                    "id": &chunk_id,
                    "object": "chat.completion.chunk",
                    "created": created,
                    "model": &openai.model,
                    "choices": [{
                        "index": 0,
                        "delta": {
                            "tool_calls": [tc]
                        },
                        "finish_reason": serde_json::Value::Null
                    }]
                });
                let sse_frame =
                    crate::sse::build_sse_frame(&serde_json::to_string(&tool_call_delta).unwrap());
                let _ = sink.send(sse_frame).await;
            }
        }
    }

    // Cache the reasoning text for next turn
    if !accumulated_thinking.is_empty() {
        SignatureCache::global().cache_session_reasoning(
            &session_id,
            accumulated_thinking.clone(),
            assistant_turn_index,
        );
    }

    // Send final stop and usage chunks if streaming
    if let Some(sink) = stream_sink {
        let final_chunk = serde_json::json!({
            "id": &chunk_id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": &openai.model,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        });
        let sse_frame = crate::sse::build_sse_frame(&serde_json::to_string(&final_chunk).unwrap());
        let _ = sink.send(sse_frame).await;

        if let Some(ref u) = usage {
            let usage_chunk = serde_json::json!({
                "id": &chunk_id,
                "object": "chat.completion.chunk",
                "created": created,
                "model": &openai.model,
                "choices": [],
                "usage": {
                    "prompt_tokens": u.prompt_tokens,
                    "completion_tokens": u.completion_tokens,
                    "total_tokens": u.total_tokens
                }
            });
            let sse_frame =
                crate::sse::build_sse_frame(&serde_json::to_string(&usage_chunk).unwrap());
            let _ = sink.send(sse_frame).await;
        }

        let _ = sink.send(crate::SSE_DONE_BYTES.clone()).await;
    }

    // 5. Assemble final OpenAIResponse
    Ok(OpenAIResponse {
        id: format!("chatcmpl-{}", Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created,
        model: openai.model.clone(),
        choices: vec![OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".to_string(),
                content: if accumulated_text.is_empty() && !accumulated_tool_calls.is_empty() {
                    None
                } else {
                    Some(serde_json::Value::String(accumulated_text))
                },
                name: None,
                tool_call_id: None,
                tool_calls: if accumulated_tool_calls.is_empty() {
                    None
                } else {
                    Some(accumulated_tool_calls)
                },
                extra: {
                    let mut extra = serde_json::Map::new();
                    if !accumulated_thinking.is_empty() {
                        extra.insert(
                            "reasoning_content".to_string(),
                            serde_json::json!(accumulated_thinking),
                        );
                    }
                    extra
                },
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_request(content: &str) -> OpenAIRequest {
        OpenAIRequest {
            model: "antigravity/gemini-2.5-pro".to_string(),
            messages: vec![OpenAIMessage {
                role: "user".to_string(),
                content: Some(serde_json::Value::String(content.to_string())),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            }],
            max_tokens: Some(100),
            temperature: Some(0.7),
            stream: false,
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
    fn test_build_envelope() {
        let req = make_request("hello");
        let envelope = build_antigravity_request(&req, "proj-123", "session1").unwrap();

        assert_eq!(envelope.project, "proj-123");
        assert_eq!(envelope.model.as_deref(), Some("gemini-2.5-pro"));
        assert_eq!(envelope.request_type, "agent");
        assert_eq!(envelope.user_agent, "antigravity");
        assert!(envelope.request.get("contents").is_some());
    }

    #[test]
    fn test_parse_markdown_chunk() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        let done =
            parse_antigravity_line(r#"{"markdown":"Hello world"}"#, &mut text, &mut usage).unwrap();
        assert!(!done);
        assert_eq!(text, "Hello world");
    }

    #[test]
    fn test_parse_markdown_inner_response() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        let done = parse_antigravity_line(
            r#"{"response":{"markdown":"Deep text"}}"#,
            &mut text,
            &mut usage,
        )
        .unwrap();
        assert!(!done);
        assert_eq!(text, "Deep text");
    }

    #[test]
    fn test_parse_gemini_chunk() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        let done = parse_antigravity_line(
            r#"{"response":{"candidates":[{"content":{"parts":[{"text":"Hi"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":10,"totalTokenCount":15}}}"#,
            &mut text,
            &mut usage,
        ).unwrap();
        assert!(!done);
        assert_eq!(text, "Hi");
        assert!(usage.is_some());
        let u = usage.unwrap();
        assert_eq!(u.prompt_tokens, 5);
        assert_eq!(u.completion_tokens, 10);
        assert_eq!(u.total_tokens, 15);
    }

    #[test]
    fn test_parse_thinking_parts_skipped() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        parse_antigravity_line(
            r#"{"response":{"candidates":[{"content":{"parts":[{"text":"thinking...","thought":true},{"text":"actual answer"}]}}]}}"#,
            &mut text,
            &mut usage,
        ).unwrap();
        assert_eq!(text, "actual answer");
    }

    #[test]
    fn test_parse_done_sentinel() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        let done = parse_antigravity_line("data: [DONE]", &mut text, &mut usage).unwrap();
        assert!(done);
    }

    #[test]
    fn test_parse_empty_line() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        let done = parse_antigravity_line("", &mut text, &mut usage).unwrap();
        assert!(!done);
        assert!(text.is_empty());
    }

    #[test]
    fn test_parse_comment_line() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        let done = parse_antigravity_line(": this is a comment", &mut text, &mut usage).unwrap();
        assert!(!done);
    }

    #[test]
    fn test_parse_credits_ignored() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        let done = parse_antigravity_line(
            r#"{"remainingCreditTypes":{"GOOGLE_ONE_AI":"42"}}"#,
            &mut text,
            &mut usage,
        )
        .unwrap();
        assert!(!done);
        assert!(text.is_empty());
    }

    fn test_strip_provider_prefix() {
        let req = make_request("test");
        let envelope = build_antigravity_request(&req, "proj", "session1").unwrap();
        assert_eq!(envelope.model.as_deref(), Some("gemini-2.5-pro"));

        // No prefix — model stays as-is
        let req2 = OpenAIRequest {
            model: "gemini-2.0-flash".to_string(),
            messages: vec![],
            max_tokens: None,
            temperature: None,
            stream: false,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        };
        let env2 = build_antigravity_request(&req2, "proj", "session1").unwrap();
        assert_eq!(env2.model.as_deref(), Some("gemini-2.0-flash"));
    }

    // =====================================================================
    // Additional edge-case tests
    // =====================================================================

    #[test]
    fn test_build_envelope_nested_prefix() {
        // Multiple slashes: "provider/sub/model" → last segment.
        let req = OpenAIRequest {
            model: "antigravity/studio/gemini-2.5-pro".to_string(),
            messages: vec![],
            max_tokens: None,
            temperature: None,
            stream: false,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        };
        let envelope = build_antigravity_request(&req, "proj", "session1").unwrap();
        assert_eq!(envelope.model.as_deref(), Some("gemini-2.5-pro"));
    }

    #[test]
    fn test_build_envelope_no_messages() {
        let req = OpenAIRequest {
            model: "gemini-2.0-flash".to_string(),
            messages: vec![],
            max_tokens: None,
            temperature: None,
            stream: false,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        };
        let envelope = build_antigravity_request(&req, "proj-empty", "session1").unwrap();
        assert_eq!(envelope.project, "proj-empty");
        assert_eq!(envelope.request_type, "agent");
        assert!(envelope.enabled_credit_types.is_some());
        assert_eq!(
            envelope.enabled_credit_types.as_ref().unwrap()[0],
            "GOOGLE_ONE_AI"
        );
    }

    #[test]
    fn test_build_envelope_request_id_is_uuid() {
        let req = make_request("test");
        let envelope = build_antigravity_request(&req, "proj", "session1").unwrap();
        // Should be a valid UUID.
        assert!(uuid::Uuid::parse_str(&envelope.request_id).is_ok());
    }

    #[test]
    fn test_parse_antigravity_line_malformed_json() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        let result = parse_antigravity_line("{not valid json!!!", &mut text, &mut usage);
        assert!(result.is_err(), "malformed JSON should return error");
        match result {
            Err(CoreError::Internal(_)) => {} // expected
            Err(other) => panic!("expected CoreError::Internal, got: {other}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
        // Text should remain empty after error.
        assert!(text.is_empty());
    }

    #[test]
    fn test_parse_antigravity_line_malformed_json_with_data_prefix() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        let result = parse_antigravity_line("data: {bad json}", &mut text, &mut usage);
        assert!(result.is_err());
        match result {
            Err(CoreError::Internal(_)) => {}
            Err(other) => panic!("expected CoreError::Internal, got: {other}"),
            Ok(_) => panic!("expected error, got Ok"),
        }
    }

    #[test]
    fn test_parse_antigravity_line_done_via_json() {
        // Some streams use {"done": true} instead of [DONE].
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        let done = parse_antigravity_line(r#"{"done":true}"#, &mut text, &mut usage).unwrap();
        assert!(done);
    }

    #[test]
    fn test_multiple_chunks_accumulation_markdown_and_gemini() {
        // Simulate a stream with mixed markdown and gemini chunks.
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;

        // Chunk 1: markdown
        let done =
            parse_antigravity_line(r#"{"markdown":"Hello "}"#, &mut text, &mut usage).unwrap();
        assert!(!done);
        assert_eq!(text, "Hello ");

        // Chunk 2: gemini
        let done = parse_antigravity_line(
            r#"{"response":{"candidates":[{"content":{"parts":[{"text":"World"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":10,"totalTokenCount":15}}}"#,
            &mut text,
            &mut usage,
        ).unwrap();
        assert!(!done);
        assert_eq!(text, "Hello World");
        assert!(usage.is_some());
    }

    #[test]
    fn test_multiple_markdown_chunks_accumulate() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;

        parse_antigravity_line(r#"{"markdown":"Line 1. "}"#, &mut text, &mut usage).unwrap();
        parse_antigravity_line(r#"{"markdown":"Line 2. "}"#, &mut text, &mut usage).unwrap();
        parse_antigravity_line(r#"{"markdown":"Line 3."}"#, &mut text, &mut usage).unwrap();

        assert_eq!(text, "Line 1. Line 2. Line 3.");
    }

    #[test]
    fn test_only_credits_chunks() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;

        parse_antigravity_line(
            r#"{"remainingCreditTypes":{"GOOGLE_ONE_AI":"100"}}"#,
            &mut text,
            &mut usage,
        )
        .unwrap();
        parse_antigravity_line(
            r#"{"remainingCreditTypes":{"GOOGLE_ONE_AI":"99"}}"#,
            &mut text,
            &mut usage,
        )
        .unwrap();

        assert!(text.is_empty());
        assert!(usage.is_none());
    }

    #[test]
    fn test_skip_unknown_shape() {
        // A completely unknown JSON shape → Skip, not error.
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        let done = parse_antigravity_line(
            r#"{"unknownKey":"some value","foo":123}"#,
            &mut text,
            &mut usage,
        )
        .unwrap();
        assert!(!done);
        assert!(text.is_empty());
    }

    #[test]
    fn test_gemini_chunk_without_usage_metadata() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        parse_antigravity_line(
            r#"{"response":{"candidates":[{"content":{"parts":[{"text":"no usage"}]}}]}}"#,
            &mut text,
            &mut usage,
        )
        .unwrap();
        assert_eq!(text, "no usage");
        assert!(usage.is_none());
    }

    #[test]
    fn test_gemini_chunk_usage_partial_fields() {
        // Only promptTokenCount present — others default to 0.
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        parse_antigravity_line(
            r#"{"response":{"candidates":[{"content":{"parts":[{"text":"x"}]}}],"usageMetadata":{"promptTokenCount":42}}}"#,
            &mut text,
            &mut usage,
        )
        .unwrap();
        assert!(usage.is_some());
        let u = usage.unwrap();
        assert_eq!(u.prompt_tokens, 42);
        assert_eq!(u.completion_tokens, 0);
        assert_eq!(u.total_tokens, 0);
    }

    #[test]
    fn test_thinking_part_only_think_text_skipped() {
        // If all parts are thinking, accumulated text should be empty.
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        parse_antigravity_line(
            r#"{"response":{"candidates":[{"content":{"parts":[{"text":"reasoning...","thought":true}]}}]}}"#,
            &mut text,
            &mut usage,
        )
        .unwrap();
        assert!(text.is_empty());
    }

    #[test]
    fn test_empty_markdown_chunk() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        let done = parse_antigravity_line(r#"{"markdown":""}"#, &mut text, &mut usage).unwrap();
        assert!(!done);
        // Empty markdown should not append anything.
        assert!(text.is_empty());
    }

    #[test]

    #[test]

    #[test]
    fn test_classify_chunk_gemini_response() {
        let v = serde_json::json!({"response": {"candidates": [{"content": {"parts": [{"text": "hi"}]}}]}});
        match classify_chunk(&v) {
            ChunkKind::Gemini { .. } => {}
            ChunkKind::Skip => panic!("expected Gemini, got Skip"),
            ChunkKind::Markdown { .. } => panic!("expected Gemini, got Markdown"),
            ChunkKind::Credits => panic!("expected Gemini, got Credits"),
        }
    }

    #[test]
    fn test_build_envelope_disabled_credit_types_optional() {
        // Verify enabled_credit_types is serialized (not skipped) when present.
        let req = make_request("hi");
        let envelope = build_antigravity_request(&req, "p", "session1").unwrap();
        let json = serde_json::to_value(&envelope).unwrap();
        assert!(json.get("enabledCreditTypes").is_some());
        assert_eq!(json["enabledCreditTypes"][0], "GOOGLE_ONE_AI");
    }

    #[test]
    fn test_signature_cache() {
        let cache = SignatureCache::global();
        cache.clear();

        // Check empty cache
        assert!(cache.get_session_signature("session1").is_none());
        assert!(cache.get_session_reasoning("session1", 0).is_none());

        // Cache signature
        let sig = "a".repeat(60); // length must be >= 50
        cache.cache_session_signature("session1", sig.clone(), 1);
        assert_eq!(cache.get_session_signature("session1"), Some(sig));

        // Cache reasoning
        cache.cache_session_reasoning("session1", "thinking hard".to_string(), 0);
        assert_eq!(
            cache.get_session_reasoning("session1", 0),
            Some("thinking hard".to_string())
        );
    }

    #[test]
    fn test_derive_session_id() {
        let fp = "sid-abcdef1234567890";
        let sid = derive_session_id(fp);
        assert!(!sid.is_empty());
        // FNV-1a is deterministic
        assert_eq!(derive_session_id(fp), sid);
    }

    #[test]
    fn test_extract_openai_session_id() {
        let req =
            make_request("this is a long prompt that should generate a stable hash fingerprint");
        let sid = extract_openai_session_id(&req);
        assert!(sid.starts_with("sid-"));
        assert_eq!(sid.len(), 20); // "sid-" (4) + 16 hex chars
    }

    #[test]
    fn test_deep_clean_cache_control() {
        let mut val = serde_json::json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "text",
                            "text": "hello",
                            "cache_control": {"type": "ephemeral"}
                        }
                    ]
                }
            ]
        });

        deep_clean_cache_control(&mut val);
        assert!(
            val["messages"][0]["content"][0]
                .get("cache_control")
                .is_none()
        );
        assert_eq!(val["messages"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn test_openai_to_gemini_antigravity_tools_and_roles() {
        let mut req = make_request("user message");
        req.messages.push(OpenAIMessage {
            role: "assistant".to_string(),
            content: None,
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![serde_json::json!({
                "id": "tc-123",
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "arguments": "{\"location\":\"New York\"}"
                }
            })]),
            extra: serde_json::Map::new(),
        });
        req.messages.push(OpenAIMessage {
            role: "tool".to_string(),
            content: Some(serde_json::json!("clear sky")),
            name: Some("get_weather".to_string()),
            tool_call_id: Some("tc-123".to_string()),
            tool_calls: None,
            extra: serde_json::Map::new(),
        });

        let gemini_req = openai_to_gemini_antigravity(&req, "session1", "gemini-2.5-pro");

        let contents = gemini_req["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 3); // user, model (tool_use), user (tool_response)

        // Verify model message has functionCall
        let model_msg = &contents[1];
        assert_eq!(model_msg["role"], "model");
        let fc = &model_msg["parts"][0]["functionCall"];
        assert_eq!(fc["name"], "get_weather");
        assert_eq!(fc["args"]["location"], "New York");

        // Verify tool response is translated to functionResponse
        let tool_resp_msg = &contents[2];
        assert_eq!(tool_resp_msg["role"], "user");
        let fr = &tool_resp_msg["parts"][0]["functionResponse"];
        assert_eq!(fr["name"], "get_weather");
        assert_eq!(fr["response"]["result"], "clear sky");
    }
}
