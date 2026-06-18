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

use crate::error::CoreError;
use crate::ids::AccountId;
use crate::translation::{
    OpenAIChoice, OpenAIMessage, OpenAIRequest, OpenAIResponse, OpenAIUsage, openai_to_gemini,
};
use crate::upstream::{CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::watch;
use uuid::Uuid;

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
// SSE chunk types — the stream can emit different shapes
// ---------------------------------------------------------------------------

/// Antigravity SSE response chunk — can be markdown or Gemini-format.
///
/// We parse via `serde_json::Value` and dispatch on the JSON shape because
/// `#[serde(untagged)]` is unreliable here: both Markdown and Gemini chunks
/// have all-optional fields, so serde picks whichever variant matches first.
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
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AntigravityContent {
    parts: Option<Vec<AntigravityPart>>,
}

#[derive(Debug, Deserialize)]
struct AntigravityPart {
    text: Option<String>,
    #[serde(default)]
    thought: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AntigravityUsageMetadata {
    prompt_token_count: Option<u32>,
    candidates_token_count: Option<u32>,
    total_token_count: Option<u32>,
}

/// Chunk kind returned after inspecting the JSON shape.
#[allow(dead_code)]
enum ChunkKind {
    Markdown { text: String },
    Gemini { response: AntigravityGeminiResponse },
    Credits,
    Skip,
}

/// Inspect a parsed JSON value and determine the chunk kind.
fn classify_chunk(v: &serde_json::Value) -> ChunkKind {
    // Top-level "markdown" key → markdown chunk
    if let Some(text) = v.get("markdown").and_then(|m| m.as_str()) {
        return ChunkKind::Markdown {
            text: text.to_string(),
        };
    }

    // Check for "response" key
    if let Some(resp) = v.get("response") {
        // Gemini: response has "candidates"
        if let Some(candidates) = resp.get("candidates") {
            if candidates.is_array() {
                if let Ok(response) =
                    serde_json::from_value::<AntigravityGeminiResponse>(resp.clone())
                {
                    return ChunkKind::Gemini { response };
                }
            }
        }
        // Markdown inner: response has "markdown"
        if let Some(text) = resp.get("markdown").and_then(|m| m.as_str()) {
            return ChunkKind::Markdown {
                text: text.to_string(),
            };
        }
    }

    // Credits: has "remainingCreditTypes" or "remaining_credit_types"
    if v.get("remainingCreditTypes").is_some() || v.get("remaining_credit_types").is_some() {
        return ChunkKind::Credits;
    }

    ChunkKind::Skip
}

// ---------------------------------------------------------------------------
// Account metadata
// ---------------------------------------------------------------------------

/// Read the project_id for an Antigravity account from the OAuth
/// provider-specific JSON blob.
pub fn read_project_id(conn: &Connection, account_id: AccountId) -> Result<String, CoreError> {
    use rusqlite::OptionalExtension;

    let specific: Option<String> = conn
        .query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
            rusqlite::params![account_id.0],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| CoreError::Internal(format!("failed to read account: {e}")))?;

    let specific = specific
        .ok_or_else(|| CoreError::Internal("antigravity account has no projectId".into()))?;

    if specific.is_empty() {
        return Err(CoreError::Internal(
            "antigravity account has no projectId".into(),
        ));
    }

    let meta: serde_json::Value = serde_json::from_str(&specific)
        .map_err(|e| CoreError::Internal(format!("invalid provider_specific JSON: {e}")))?;

    meta.get("projectId")
        .or_else(|| meta.get("project_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| CoreError::Internal("antigravity account has no projectId".into()))
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

/// Build the Cloud Code envelope from OpenAI request + account metadata.
fn build_antigravity_request(
    openai: &OpenAIRequest,
    project_id: &str,
) -> Result<AntigravityRequestEnvelope, CoreError> {
    let gemini = openai_to_gemini(openai);

    let gemini_json = serde_json::to_value(&gemini)
        .map_err(|e| CoreError::Internal(format!("failed to serialize gemini request: {e}")))?;

    Ok(AntigravityRequestEnvelope {
        project: project_id.to_string(),
        model: Some(
            // Strip provider prefix if present (e.g. "antigravity/gemini-2.5-pro" → "gemini-2.5-pro")
            openai
                .model
                .split('/')
                .last()
                .unwrap_or(&openai.model)
                .to_string(),
        ),
        request_type: "agent".to_string(),
        request_id: Uuid::new_v4().to_string(),
        user_agent: "antigravity".to_string(),
        request: gemini_json,
        enabled_credit_types: Some(vec!["GOOGLE_ONE_AI".to_string()]),
    })
}

// ---------------------------------------------------------------------------
// SSE parsing
// ---------------------------------------------------------------------------

/// Parse a single SSE data line from Antigravity into text chunks and usage.
/// Returns `true` when the stream is complete (`[DONE]` sentinel).
fn parse_antigravity_line(
    line: &str,
    accumulated_text: &mut String,
    usage: &mut Option<OpenAIUsage>,
) -> Result<bool, CoreError> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with(':') {
        return Ok(false);
    }

    // Remove "data: " prefix if present
    let json_str = if let Some(rest) = trimmed.strip_prefix("data: ") {
        rest
    } else {
        trimmed
    };

    if json_str == "[DONE]" {
        return Ok(true);
    }

    let v: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| CoreError::Internal(format!("failed to parse antigravity chunk: {e}")))?;

    // Special case: the sentinel can also be a JSON {"done": true} shape.
    if v.get("done").and_then(|d| d.as_bool()) == Some(true) {
        return Ok(true);
    }

    match classify_chunk(&v) {
        ChunkKind::Credits | ChunkKind::Skip => Ok(false),
        ChunkKind::Markdown { text } => {
            accumulated_text.push_str(&text);
            Ok(false)
        }
        ChunkKind::Gemini { response } => {
            if let Some(candidates) = response.candidates {
                for candidate in candidates {
                    if let Some(content) = candidate.content {
                        if let Some(parts) = content.parts {
                            for part in parts {
                                if let Some(text) = part.text {
                                    // Skip "thinking" parts
                                    if part.thought != Some(true) {
                                        accumulated_text.push_str(&text);
                                    }
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

/// Execute an Antigravity (Cloud Code) request.
///
/// 1. Translates OpenAI → Gemini format
/// 2. Wraps in Cloud Code envelope
/// 3. POSTs to `streamGenerateContent?alt=sse`
/// 4. Parses SSE stream (markdown or Gemini format)
/// 5. Returns assembled `OpenAIResponse`
///
/// **Gate 3 migration:** the previous `&reqwest::Client` parameter
/// was replaced with `&Arc<UpstreamClient>`. Call sites in the
/// chat pipeline (`pipeline.rs:962`) were updated to pass
/// `&self.config.upstream_client`. The server-side admin test
/// endpoint at `crates/openproxy-server/src/handlers/admin.rs:1972`
/// is an out-of-scope call site that still passes a `reqwest::Client`
/// and is tracked as a follow-up (see Gate 3 report).
///
/// **C3 fix:** the function now accepts the per-request
/// `client_disconnected: watch::Receiver<bool>` and wires it into
/// the upstream call as a [`CancellationToken::from_watch`]. The
/// previous implementation created a fresh `CancellationToken::new()`
/// that was never flipped by the client's TCP-level disconnect, so
/// a streaming request the user closed early kept running on the
/// Antigravity backend for the full `body_chunk_ms` and billed
/// tokens the client never saw. Plumbing the watch through here
/// means a real cancel propagates into the upstream call within a
/// few milliseconds.
pub async fn execute_antigravity(
    upstream_client: &Arc<UpstreamClient>,
    access_token: &str,
    project_id: &str,
    openai: &OpenAIRequest,
    client_disconnected: watch::Receiver<bool>,
) -> Result<OpenAIResponse, CoreError> {
    // 1. Build Cloud Code envelope
    let envelope = build_antigravity_request(openai, project_id)?;
    let body_bytes = serde_json::to_vec(&envelope)
        .map_err(|e| CoreError::Internal(format!("failed to serialize envelope: {e}")))?;

    // 2. Build the upstream request. POST to the streamGenerateContent
    //    endpoint with JSON body, bearer auth, and SSE accept.
    let url =
        "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent?alt=sse";
    let mut upstream_request =
        UpstreamRequest::post_json(url.to_string(), bytes::Bytes::from(body_bytes));
    if let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
        upstream_request
            .headers
            .insert(http::header::AUTHORIZATION, value);
    }
    upstream_request.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("text/event-stream"),
    );

    // 3. Fire the request. Same rationale as the kiro executor: use
    //    `TimeoutProfile::Chat` (no per-call `Timeouts` is plumbed
    //    through to the executors today; `state.rs` only sets a
    //    `connect_ms` for the legacy reqwest client).
    //
    //    The cancellation token is sourced from
    //    `client_disconnected` (the per-request watch) so a real
    //    client cancel propagates into the upstream send /
    //    streaming reads via `tokio::select!`. See the function
    //    docstring for the C3 audit context.
    let cancel = CancellationToken::from_watch(client_disconnected);
    let response = match upstream_client
        .call(upstream_request, TimeoutProfile::Chat, cancel)
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
    // 4. Collect the full body. The pre-migration code used
    //    `response.text().await` to slurp the SSE stream into a
    //    `String`; the upstream client exposes `collect()` which
    //    returns `Bytes`. The body is bounded to 32 MiB at the
    //    upstream layer; an Antigravity SSE stream is well under
    //    that in practice.
    let body_bytes = match response.collect().await {
        Ok(b) => b,
        Err(e) => {
            return Err(match e {
                UpstreamError::Cancel => CoreError::ClientDisconnected,
                other => CoreError::UpstreamConnection(format!(
                    "failed to read antigravity response: {other}"
                )),
            });
        }
    };
    let body_text = String::from_utf8_lossy(&body_bytes);

    if !(200..300).contains(&status) {
        return Err(CoreError::UpstreamError {
            status,
            provider: "antigravity".to_string(),
            model: openai.model.clone(),
            body: body_text.to_string(),
        });
    }

    // 5. Parse SSE stream
    let mut accumulated_text = String::new();
    let mut usage: Option<OpenAIUsage> = None;

    for line in body_text.lines() {
        let done = parse_antigravity_line(line, &mut accumulated_text, &mut usage)?;
        if done {
            break;
        }
    }

    // 6. Assemble OpenAIResponse
    Ok(OpenAIResponse {
        id: format!("chatcmpl-{}", Uuid::new_v4()),
        object: "chat.completion".to_string(),
        created: chrono::Utc::now().timestamp() as u64,
        model: openai.model.clone(),
        choices: vec![OpenAIChoice {
            index: 0,
            message: OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::String(accumulated_text)),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: Some(usage.unwrap_or(OpenAIUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
            total_tokens: 0,
        })),
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
        let envelope = build_antigravity_request(&req, "proj-123").unwrap();

        assert_eq!(envelope.project, "proj-123");
        assert_eq!(
            envelope.model.as_deref(),
            Some("gemini-2.5-pro")
        );
        assert_eq!(envelope.request_type, "agent");
        assert_eq!(envelope.user_agent, "antigravity");
        assert!(envelope.request.get("contents").is_some());
    }

    #[test]
    fn test_parse_markdown_chunk() {
        let mut text = String::new();
        let mut usage: Option<OpenAIUsage> = None;
        let done =
            parse_antigravity_line(r#"{"markdown":"Hello world"}"#, &mut text, &mut usage)
                .unwrap();
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
        let done =
            parse_antigravity_line(": this is a comment", &mut text, &mut usage).unwrap();
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

    #[test]
    fn test_read_project_id_missing() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (id INTEGER PRIMARY KEY, oauth_provider_specific TEXT)",
        )
        .unwrap();
        let err = read_project_id(&conn, AccountId(1)).unwrap_err();
        assert!(format!("{err}").contains("no projectId"));
    }

    #[test]
    fn test_read_project_id_present() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (id INTEGER PRIMARY KEY, oauth_provider_specific TEXT)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO accounts (id, oauth_provider_specific) VALUES (1, ?1)",
            rusqlite::params![r#"{"projectId":"proj-abc"}"#],
        )
        .unwrap();
        let pid = read_project_id(&conn, AccountId(1)).unwrap();
        assert_eq!(pid, "proj-abc");
    }

    #[test]
    fn test_strip_provider_prefix() {
        let req = make_request("test");
        let envelope = build_antigravity_request(&req, "proj").unwrap();
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
        let env2 = build_antigravity_request(&req2, "proj").unwrap();
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
        let envelope = build_antigravity_request(&req, "proj").unwrap();
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
        let envelope = build_antigravity_request(&req, "proj-empty").unwrap();
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
        let envelope = build_antigravity_request(&req, "proj").unwrap();
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
    fn test_read_project_id_with_project_id_key() {
        // Both "projectId" and "project_id" should be accepted.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (id INTEGER PRIMARY KEY, oauth_provider_specific TEXT)",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO accounts (id, oauth_provider_specific) VALUES (1, ?1)",
            rusqlite::params![r#"{"project_id":"proj-underscore"}"#],
        )
        .unwrap();
        let pid = read_project_id(&conn, AccountId(1)).unwrap();
        assert_eq!(pid, "proj-underscore");
    }

    #[test]
    fn test_read_project_id_empty_specific() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (id INTEGER PRIMARY KEY, oauth_provider_specific TEXT)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO accounts (id, oauth_provider_specific) VALUES (1, ?1)",
            rusqlite::params![""],
        )
        .unwrap();
        let err = read_project_id(&conn, AccountId(1)).unwrap_err();
        assert!(format!("{err}").contains("no projectId"));
    }

    #[test]
    fn test_read_project_id_null_specific() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (id INTEGER PRIMARY KEY, oauth_provider_specific TEXT)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO accounts (id, oauth_provider_specific) VALUES (1, NULL)",
            [],
        )
        .unwrap();
        let err = read_project_id(&conn, AccountId(1)).unwrap_err();
        // NULL column: row.get::<_, String>(0) fails with a type conversion
        // error which becomes CoreError::Internal("failed to read account: ...").
        match &err {
            CoreError::Internal(msg) => {
                assert!(
                    msg.contains("failed to read account") || msg.contains("no projectId"),
                    "unexpected error message: {msg}"
                );
            }
            other => panic!("expected Internal error, got: {other}"),
        }
    }

    #[test]
    fn test_read_project_id_invalid_json() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE accounts (id INTEGER PRIMARY KEY, oauth_provider_specific TEXT)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO accounts (id, oauth_provider_specific) VALUES (1, ?1)",
            rusqlite::params!["not json at all"],
        )
        .unwrap();
        let err = read_project_id(&conn, AccountId(1)).unwrap_err();
        assert!(format!("{err}").contains("invalid provider_specific JSON"));
    }

    #[test]
    fn test_classify_chunk_unknown_shape() {
        let v = serde_json::json!({"someOtherKey": "value"});
        match classify_chunk(&v) {
            ChunkKind::Skip => {}
            ChunkKind::Markdown { .. } => panic!("expected Skip, got Markdown"),
            ChunkKind::Gemini { .. } => panic!("expected Skip, got Gemini"),
            ChunkKind::Credits => panic!("expected Skip, got Credits"),
        }
    }

    #[test]
    fn test_classify_chunk_credits_snake_case() {
        let v = serde_json::json!({"remaining_credit_types": {"GOOGLE_ONE_AI": "50"}});
        match classify_chunk(&v) {
            ChunkKind::Credits => {}
            ChunkKind::Skip => panic!("expected Credits, got Skip"),
            ChunkKind::Markdown { .. } => panic!("expected Credits, got Markdown"),
            ChunkKind::Gemini { .. } => panic!("expected Credits, got Gemini"),
        }
    }

    #[test]
    fn test_classify_chunk_markdown_inner_response() {
        let v = serde_json::json!({"response": {"markdown": "inner text"}});
        match classify_chunk(&v) {
            ChunkKind::Markdown { text } => assert_eq!(text, "inner text"),
            ChunkKind::Skip => panic!("expected Markdown, got Skip"),
            ChunkKind::Gemini { .. } => panic!("expected Markdown, got Gemini"),
            ChunkKind::Credits => panic!("expected Markdown, got Credits"),
        }
    }

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
        let envelope = build_antigravity_request(&req, "p").unwrap();
        let json = serde_json::to_value(&envelope).unwrap();
        assert!(json.get("enabledCreditTypes").is_some());
        assert_eq!(json["enabledCreditTypes"][0], "GOOGLE_ONE_AI");
    }
}
