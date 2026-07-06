//! Kiro AI (AWS CodeWhisperer) chat executor.
//!
//! Translates an OpenAI-shaped request into Kiro's native
//! `conversationState` envelope, POSTs it to
//! `https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse`,
//! and parses the response.
//!
//! # Protocol
//!
//! ```text
//! POST {base}/generateAssistantResponse
//! Authorization: Bearer <access_token>
//! x-amz-user-agent: aws-sdk-js/3.0.0 kiro/0.1
//! Content-Type: application/json
//!
//! {
//!   "conversationState": {
//!     "currentMessage": {
//!       "userInputMessage": {
//!         "content": "...",
//!         "modelId": "auto",
//!         "origin": "AI_EDITOR"
//!       }
//!     },
//!     "chatTriggerType": "MANUAL"
//!   },
//!   "profileArn": "arn:aws:codewhisperer:us-east-1:...",
//!   "inferenceConfig": { "maxTokens": 4096 }
//! }
//! ```
//!
//! The response is an AWS EventStream binary frame; the MVP parser
//! is a best-effort JSON attempt: if the body parses as JSON, it
//! is treated as the full response; otherwise the raw bytes are
//! surfaced as the body of an `OpenAIResponse` so the caller can
//! at least see *something* on the wire.

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::translation::{OpenAIMessage, OpenAIRequest, OpenAIResponse};
use crate::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest,
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::watch;

/// Default Kiro runtime region. The OAuth provider is hardcoded
/// to `us-east-1`; the chat executor uses the same default until
/// the user re-links a regional profile.
pub const KIRO_DEFAULT_REGION: &str = "us-east-1";

/// Default Kiro model id used when the upstream request does not
/// supply one (or supplies a non-Kiro id like `auto`).
pub const KIRO_DEFAULT_MODEL: &str = "auto";

/// Hardcoded `x-amz-user-agent` value the chat executor sends. The
/// Kiro CLI / IDE plugins ship the same string; matching it
/// avoids an upstream behavior gap.
pub const KIRO_USER_AGENT: &str = "aws-sdk-js/3.0.0 kiro/0.1";

/// Build the upstream URL for the Kiro `generateAssistantResponse`
/// endpoint, picking the regional host for non-us-east-1
/// regions (Amazon Q uses `q.{region}.amazonaws.com` outside
/// us-east-1).
pub fn kiro_runtime_url(region: &str) -> String {
    let region = if region.is_empty() { KIRO_DEFAULT_REGION } else { region };
    let host = if region == KIRO_DEFAULT_REGION {
        format!("https://codewhisperer.{}.amazonaws.com", region)
    } else {
        format!("https://q.{}.amazonaws.com", region)
    };
    format!("{}/generateAssistantResponse", host)
}

/// Read the per-account `profileArn` + `region` persisted by the
/// Kiro OAuth provider's `post_exchange` hook. Returns
/// `Ok(None)` when the row has no `oauth_provider_specific` JSON
/// or the JSON omits `profileArn` (the user is mid-OAuth or
/// re-linking); the executor then uses a placeholder so the
/// request still goes out and the failure surfaces an
/// actionable error message.
pub fn read_account_meta(
    conn: &Connection,
    account_id: AccountId,
) -> Result<Option<crate::oauth_kiro::KiroProviderMeta>> {
    crate::oauth_kiro::read_profile_meta(conn, account_id)
}

/// Request body envelope used by `generateAssistantResponse`.
///
/// Only `conversationState` and (optionally) `profileArn` +
/// `inferenceConfig` are required. The executor builds the
/// `currentMessage` from the most recent `user` message in the
/// OpenAI request, and folds prior turns into
/// `conversationState.history` so multi-turn conversations work.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroRequest {
    #[serde(rename = "conversationState")]
    pub conversation_state: KiroConversationState,
    #[serde(rename = "profileArn", skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    #[serde(rename = "inferenceConfig", skip_serializing_if = "Option::is_none")]
    pub inference_config: Option<KiroInferenceConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroConversationState {
    #[serde(rename = "currentMessage")]
    pub current_message: KiroCurrentMessage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<KiroHistoryItem>>,
    #[serde(rename = "chatTriggerType")]
    pub chat_trigger_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroCurrentMessage {
    #[serde(rename = "userInputMessage")]
    pub user_input_message: KiroUserInputMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroUserInputMessage {
    pub content: String,
    #[serde(rename = "modelId")]
    pub model_id: String,
    pub origin: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroHistoryItem {
    #[serde(rename = "userInputMessage")]
    pub user_input_message: KiroUserInputMessage,
    #[serde(rename = "assistantResponseMessage")]
    pub assistant_response_message: KiroAssistantResponseMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KiroAssistantResponseMessage {
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroInferenceConfig {
    #[serde(rename = "maxTokens", skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(rename = "topP", skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop: Option<Vec<String>>,
}

/// Build a [`KiroRequest`] from an OpenAI [`OpenAIRequest`].
///
/// The conversion rules:
/// - `model` → `conversationState.currentMessage.userInputMessage.modelId`
/// - Last `user` message → `currentMessage.userInputMessage.content`
/// - Preceding `user`/`assistant` turns → `conversationState.history`
/// - `max_tokens` / `temperature` / `top_p` / `stop` → `inferenceConfig`
/// - `stream` is dropped (Kiro is always-on for the protocol
///   variant we use; the streaming variant is the EventStream
///   binary format and is a follow-up)
pub fn build_kiro_request(openai: &OpenAIRequest, profile_arn: Option<&str>) -> KiroRequest {
    let (history_msgs, current_msg) = split_history(openai);

    let history: Vec<KiroHistoryItem> = history_msgs
        .chunks(2)
        .filter_map(|pair| {
            if let [user, assistant] = pair {
                Some(KiroHistoryItem {
                    user_input_message: KiroUserInputMessage {
                        content: user
                            .content
                            .as_ref()
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        model_id: KIRO_DEFAULT_MODEL.to_string(),
                        origin: "AI_EDITOR".to_string(),
                    },
                    assistant_response_message: KiroAssistantResponseMessage {
                        content: assistant
                            .content
                            .as_ref()
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    },
                })
            } else {
                None
            }
        })
        .collect();

    let inference_config = if openai.max_tokens.is_some()
        || openai.temperature.is_some()
        || openai.top_p.is_some()
        || openai.stop.is_some()
    {
        Some(KiroInferenceConfig {
            max_tokens: openai.max_tokens,
            temperature: openai.temperature,
            top_p: openai.top_p,
            stop: openai.stop.clone(),
        })
    } else {
        None
    };

    KiroRequest {
        conversation_state: KiroConversationState {
            current_message: KiroCurrentMessage {
                user_input_message: KiroUserInputMessage {
                    content: current_msg
                        .as_ref()
                        .and_then(|m| m.content.as_ref())
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    model_id: openai.model.clone(),
                    origin: "AI_EDITOR".to_string(),
                },
            },
            history: if history.is_empty() {
                None
            } else {
                Some(history)
            },
            chat_trigger_type: "MANUAL".to_string(),
        },
        profile_arn: profile_arn.map(|s| s.to_string()),
        inference_config,
    }
}

/// Split the OpenAI messages into the (history, current_user_message)
/// pair. Kiro's `currentMessage` is always a single user turn, so
/// we keep the most recent user message out of the history list.
fn split_history(req: &OpenAIRequest) -> (Vec<&OpenAIMessage>, Option<OpenAIMessage>) {
    if req.messages.is_empty() {
        return (Vec::new(), None);
    }
    let last_user_idx = req
        .messages
        .iter()
        .rposition(|m| m.role == "user")
        .unwrap_or(req.messages.len() - 1);
    let history: Vec<&OpenAIMessage> = req.messages[..last_user_idx].iter().collect();
    let current = req.messages[last_user_idx].clone();
    (history, Some(current))
}

/// Parse the Kiro `generateAssistantResponse` body into an
/// [`OpenAIResponse`].
///
/// The upstream returns an AWS EventStream binary frame. We do
/// NOT do full EventStream parsing in the MVP (a CRC + ByteQueue
/// decoder is a follow-up). Instead this function:
/// 1. Tries to parse the body as JSON; if it succeeds we extract
///    any `content` / `message` / `text` field and return it.
/// 2. On parse failure we surface the raw body as a single
///    assistant content block prefixed with an error hint so
///    the operator can see *something* on the wire.
pub fn parse_kiro_response(body: &[u8], model: &str) -> Result<OpenAIResponse> {
    let id = format!("chatcmpl-kiro-{}", chrono::Utc::now().timestamp_millis());
    let created = chrono::Utc::now().timestamp() as u64;

    // Fast path: body is valid JSON.
    if let Ok(v) = serde_json::from_slice::<Value>(body) {
        let content = extract_kiro_content(&v);
        if let Some(content) = content {
            return Ok(OpenAIResponse {
                id,
                object: "chat.completion".to_string(),
                created,
                model: model.to_string(),
                choices: vec![crate::translation::OpenAIChoice {
                    index: 0,
                    message: crate::translation::OpenAIMessage {
                        role: "assistant".to_string(),
                        content: Some(serde_json::Value::String(content)),
                        name: None,
                        tool_call_id: None,
                        tool_calls: None,
                        extra: serde_json::Map::new(),
                    },
                    finish_reason: Some("stop".to_string()),
                }],
                usage: None, // Kiro never reports usage — let the pipeline estimate
            });
        }
    }

    // Slow path: assume AWS EventStream binary. Build a tiny
    // summary string so the operator can see *something* on the
    // wire without us having to ship a full binary parser. The
    // real streaming path is a follow-up.
    let body_str = String::from_utf8_lossy(body);
    let snippet = body_str.chars().take(512).collect::<String>();
    let content = format!(
        "[kiro: unparsed EventStream body ({} bytes); first 512 chars: {}]",
        body.len(),
        snippet
    );
    Ok(OpenAIResponse {
        id,
        object: "chat.completion".to_string(),
        created,
        model: model.to_string(),
        choices: vec![crate::translation::OpenAIChoice {
            index: 0,
            message: crate::translation::OpenAIMessage {
                role: "assistant".to_string(),
                content: Some(serde_json::Value::String(content)),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: serde_json::Map::new(),
            },
            finish_reason: Some("stop".to_string()),
        }],
        usage: None, // Kiro never reports usage — let the pipeline estimate
    })
}

/// Walk the JSON shape Kiro uses and extract the assistant text.
fn extract_kiro_content(v: &Value) -> Option<String> {
    // Most common shapes:
    //   {"content": "..."}
    //   {"message": {"content": "..."}}
    //   {"choices": [{"message": {"content": "..."}}]}
    //   {"output": {"message": {"content": "..."}}}
    if let Some(s) = v.get("content").and_then(|x| x.as_str()) {
        return Some(s.to_string());
    }
    if let Some(s) = v
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|x| x.as_str())
    {
        return Some(s.to_string());
    }
    if let Some(arr) = v.get("choices").and_then(|c| c.as_array())
        && let Some(first) = arr.first()
        && let Some(s) = first
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|x| x.as_str())
    {
        return Some(s.to_string());
    }
    if let Some(s) = v
        .get("output")
        .and_then(|o| o.get("message"))
        .and_then(|m| m.get("content"))
        .and_then(|x| x.as_str())
    {
        return Some(s.to_string());
    }
    None
}

/// Fetch a chat completion from Kiro for a given account + request.
///
/// `region` is the AWS region (from the account's OAuth metadata,
/// or [`KIRO_DEFAULT_REGION`]). `profile_arn` is the optional
/// CodeWhisperer profile ARN. `upstream_client` is the shared
/// hyper-based `UpstreamClient`. `access_token` is the
/// (already-decrypted) bearer token. The returned [`OpenAIResponse`]
/// is the parsed (best-effort) body.
///
/// **Gate 3 migration:** the previous `&reqwest::Client` parameter
/// was replaced with `&Arc<UpstreamClient>`. Call sites in the
/// chat pipeline (`pipeline.rs:951`) were updated to pass
/// `&self.config.upstream_client`. The server-side admin test
/// endpoint at `crates/openproxy-server/src/handlers/admin.rs:1996`
/// is an out-of-scope call site that still passes a `reqwest::Client`
/// and is tracked as a follow-up (see Gate 3 report).
///
/// **C3 fix:** the function now accepts the per-request
/// `client_disconnected: watch::Receiver<bool>` and wires it into
/// the upstream call as a [`CancellationToken::from_watch`]. The
/// previous implementation created a fresh `CancellationToken::new()`
/// that was never flipped by the client's TCP-level disconnect,
/// so a streaming request that the user closed early kept running
/// on the Kiro backend for the full `body_chunk_ms` (90s) and
/// billed tokens the client never saw. Plumbing the watch through
/// here means a real cancel propagates into the upstream call
/// within a few milliseconds.
pub async fn execute_kiro(
    upstream_client: &Arc<UpstreamClient>,
    access_token: &str,
    region: &str,
    profile_arn: Option<&str>,
    openai: &OpenAIRequest,
    client_disconnected: watch::Receiver<bool>,
    proxy: Option<String>,
) -> Result<OpenAIResponse> {
    // 1. Build the request body.
    let req = build_kiro_request(openai, profile_arn);

    // 2. Build the upstream request. The body is JSON; the URL
    //    is the regional Kiro endpoint; the auth header is the
    //    bearer token.
    let url = kiro_runtime_url(region);
    let body_bytes = serde_json::to_vec(&req)
        .map_err(|e| CoreError::Parse(format!("kiro request serialize: {e}")))?;
    let mut upstream_request = UpstreamRequest::post_json(url, bytes::Bytes::from(body_bytes));
    upstream_request.proxy = proxy;

    // `post_json` already sets `Content-Type: application/json`;
    // `Authorization` and `x-amz-user-agent` are added here as
    // they are caller-specific (not generic to all POSTs).
    if let Ok(value) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
        upstream_request
            .headers
            .insert(http::header::AUTHORIZATION, value);
    }
    upstream_request.headers.insert(
        http::header::HeaderName::from_static("x-amz-user-agent"),
        http::HeaderValue::from_static(KIRO_USER_AGENT),
    );

    // 3. Fire the request. The Kiro/Antigravity executors do not
    //    currently receive a per-call `Timeouts` value (only the
    //    chat pipeline does). For backward-compat with the
    //    pre-migration behavior (no enforced request-level
    //    timeouts beyond the client-wide `connect_ms` from
    //    `state.rs`), we use the `Chat` profile: a tight
    //    `headers_ms` (20s) and a generous `body_chunk_ms` (90s)
    //    match what an interactive chat would expect. Future
    //    gates can plumb a `Timeouts` value through and switch
    //    to `TimeoutProfile::Custom(as_resolved())`.
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
                // Map all transport / decode / timeout errors to
                // `UpstreamConnection` with a `kiro upstream:` prefix,
                // matching the pre-migration shape.
                other => CoreError::UpstreamConnection(format!("kiro upstream: {other}")),
            });
        }
    };

    let status = response.status;
    // Collect the body. On read failure we map any `UpstreamError`
    // to `UpstreamConnection` with a `kiro body read:` prefix.
    let body = match response.collect().await {
        Ok(b) => b,
        Err(e) => {
            return Err(match e {
                UpstreamError::Cancel => CoreError::ClientDisconnected,
                other => CoreError::UpstreamConnection(format!("kiro body read: {other}")),
            });
        }
    };

    if !status.is_success() {
        let body_str = String::from_utf8_lossy(&body).to_string();
        return Err(CoreError::UpstreamError {
            status: status.as_u16(),
            provider: "kiro".into(),
            model: openai.model.clone(),
            body: body_str,
        });
    }

    parse_kiro_response(&body, &openai.model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::translation::{OpenAIMessage, OpenAIRequest};
    use serde_json::json;

    fn req(msgs: Vec<(&str, &str)>) -> OpenAIRequest {
        OpenAIRequest {
            model: "auto".into(),
            messages: msgs
                .into_iter()
                .map(|(r, c)| OpenAIMessage {
                    role: r.into(),
                    content: Some(serde_json::Value::String(c.to_string())),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: serde_json::Map::new(),
                })
                .collect(),
            stream: false,
            temperature: None,
            max_tokens: Some(2048),
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
    fn kiro_runtime_url_us_east_1() {
        assert_eq!(
            kiro_runtime_url("us-east-1"),
            "https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse"
        );
    }

    #[test]
    fn kiro_runtime_url_other_region() {
        assert_eq!(
            kiro_runtime_url("eu-central-1"),
            "https://q.eu-central-1.amazonaws.com/generateAssistantResponse"
        );
    }

    #[test]
    fn build_request_includes_profile_arn_when_provided() {
        let req = req(vec![("user", "ping")]);
        let kiro = build_kiro_request(&req, Some("arn:..."));
        let json = serde_json::to_value(&kiro).unwrap();
        assert_eq!(json["profileArn"], "arn:...");
    }

    #[test]
    fn build_request_omits_profile_arn_when_none() {
        let req = req(vec![("user", "ping")]);
        let kiro = build_kiro_request(&req, None);
        let json = serde_json::to_value(&kiro).unwrap();
        assert!(json.get("profileArn").is_none());
    }

    #[test]
    fn build_request_picks_last_user_message_as_current() {
        let req = req(vec![
            ("user", "first"),
            ("assistant", "ack"),
            ("user", "second"),
        ]);
        let kiro = build_kiro_request(&req, None);
        let json = serde_json::to_value(&kiro).unwrap();
        let content = json["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap();
        assert_eq!(content, "second");
    }

    #[test]
    fn build_request_includes_history() {
        let req = req(vec![
            ("user", "first"),
            ("assistant", "ack"),
            ("user", "second"),
        ]);
        let kiro = build_kiro_request(&req, None);
        let json = serde_json::to_value(&kiro).unwrap();
        let history = json["conversationState"]["history"].as_array().unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(
            history[0]["userInputMessage"]["content"].as_str().unwrap(),
            "first"
        );
        assert_eq!(
            history[0]["assistantResponseMessage"]["content"]
                .as_str()
                .unwrap(),
            "ack"
        );
    }

    #[test]
    fn build_request_sets_chat_trigger_type_manual() {
        let req = req(vec![("user", "ping")]);
        let kiro = build_kiro_request(&req, None);
        let json = serde_json::to_value(&kiro).unwrap();
        assert_eq!(json["conversationState"]["chatTriggerType"], "MANUAL");
    }

    #[test]
    fn build_request_sets_origin_ai_editor() {
        let req = req(vec![("user", "ping")]);
        let kiro = build_kiro_request(&req, None);
        let json = serde_json::to_value(&kiro).unwrap();
        assert_eq!(
            json["conversationState"]["currentMessage"]["userInputMessage"]["origin"],
            "AI_EDITOR"
        );
    }

    #[test]
    fn build_request_propagates_model_id() {
        let req = req(vec![("user", "ping")]);
        let kiro = build_kiro_request(&req, None);
        let json = serde_json::to_value(&kiro).unwrap();
        assert_eq!(
            json["conversationState"]["currentMessage"]["userInputMessage"]["modelId"],
            "auto"
        );
    }

    #[test]
    fn build_request_includes_inference_config_when_max_tokens_set() {
        let req = req(vec![("user", "ping")]);
        let kiro = build_kiro_request(&req, None);
        let json = serde_json::to_value(&kiro).unwrap();
        assert_eq!(json["inferenceConfig"]["maxTokens"], 2048);
    }

    #[test]
    fn build_request_omits_inference_config_when_all_none() {
        let mut r = req(vec![("user", "ping")]);
        r.max_tokens = None;
        r.temperature = None;
        r.top_p = None;
        r.stop = None;
        let kiro = build_kiro_request(&r, None);
        let json = serde_json::to_value(&kiro).unwrap();
        assert!(json.get("inferenceConfig").is_none());
    }

    #[test]
    fn parse_response_extracts_top_level_content() {
        let body = serde_json::to_vec(&json!({"content": "hello"})).unwrap();
        let r = parse_kiro_response(&body, "auto").expect("parse");
        assert_eq!(
            r.choices[0]
                .message
                .content
                .as_ref()
                .and_then(serde_json::Value::as_str),
            Some("hello")
        );
    }

    #[test]
    fn parse_response_extracts_message_content() {
        let body = serde_json::to_vec(&json!({
            "message": {"content": "hi from message"}
        }))
        .unwrap();
        let r = parse_kiro_response(&body, "auto").expect("parse");
        assert_eq!(
            r.choices[0]
                .message
                .content
                .as_ref()
                .and_then(serde_json::Value::as_str),
            Some("hi from message")
        );
    }

    #[test]
    fn parse_response_extracts_choices_shape() {
        let body = serde_json::to_vec(&json!({
            "choices": [{"message": {"content": "from choices"}}]
        }))
        .unwrap();
        let r = parse_kiro_response(&body, "auto").expect("parse");
        assert_eq!(
            r.choices[0]
                .message
                .content
                .as_ref()
                .and_then(serde_json::Value::as_str),
            Some("from choices")
        );
    }

    #[test]
    fn parse_response_falls_back_to_eventstream_summary() {
        // 16 bytes of zeros — would not parse as JSON.
        let body = vec![0u8; 16];
        let r = parse_kiro_response(&body, "auto").expect("parse");
        let content = r.choices[0]
            .message
            .content
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(content.contains("unparsed EventStream body"));
        assert!(content.contains("16 bytes"));
    }

    #[test]
    fn parse_response_extracts_output_message_content() {
        let body = serde_json::to_vec(&json!({
            "output": { "message": { "content": "hi from output" } }
        }))
        .unwrap();
        let r = parse_kiro_response(&body, "auto").expect("parse");
        assert_eq!(
            r.choices[0]
                .message
                .content
                .as_ref()
                .and_then(serde_json::Value::as_str),
            Some("hi from output")
        );
    }

    #[test]
    fn parse_response_handles_invalid_json_gracefully() {
        let body = b"{ invalid json }";
        let r = parse_kiro_response(body, "auto").expect("parse");
        let content = r.choices[0]
            .message
            .content
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(content.contains("unparsed EventStream body"));
    }

    #[test]
    fn parse_response_handles_valid_json_without_known_content_keys() {
        let body = serde_json::to_vec(&json!({ "foo": "bar" })).unwrap();
        let r = parse_kiro_response(&body, "auto").expect("parse");
        let content = r.choices[0]
            .message
            .content
            .as_ref()
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(content.contains("unparsed EventStream body"));
    }
}
