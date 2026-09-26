//! Provider Session Affinity and Session ID Translation.
//!
//! Provides the [`ProviderSessionTranslator`] trait and centralized affinity resolution
//! to guarantee that multi-turn LLM agent conversations consistently route to the same
//! upstream GPU worker node for optimal prefix prompt KV cache hits across all providers
//! (OpenAI Codex, OpenCode, CodeBuddy / Tencent Cloud, CommandCode, MiniMax / Mavis,
//! Kiro / AWS Bedrock, Google Antigravity, and generic providers).

use openproxy_adapters::spoofer::translate_session_id;
use openproxy_types::OpenAIRequest;
use std::collections::BTreeMap;
use std::hash::{DefaultHasher, Hash, Hasher};

/// Trait implemented by provider adapters to translate canonical session affinity
/// into upstream-specific HTTP headers, key conventions, and protocol requirements.
pub trait ProviderSessionTranslator: Send + Sync {
    /// Format and inject provider-specific session affinity headers into `headers`.
    fn apply_session(&self, headers: &mut Vec<(String, String)>, canonical_session: &str);
}

/// Case-insensitively insert or update a header in a list of `(String, String)` pairs.
#[inline]
pub fn upsert_header(headers: &mut Vec<(String, String)>, key: &str, val: impl Into<String>) {
    if let Some(pos) = headers
        .iter()
        .position(|(hk, _)| hk.eq_ignore_ascii_case(key))
    {
        headers[pos].1 = val.into();
    } else {
        headers.push((key.to_string(), val.into()));
    }
}

/// Case-insensitively retrieve header value from a BTreeMap.
#[inline]
pub fn get_header_val<'a>(headers: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

/// Helper to convert any session string deterministically into a standard UUID v4 string.
pub fn format_as_uuid(input: &str) -> String {
    let trimmed = input.trim();
    if let Ok(parsed) = uuid::Uuid::parse_str(trimmed) {
        return parsed.to_string();
    }
    let mut hasher = DefaultHasher::new();
    trimmed.hash(&mut hasher);
    let h = hasher.finish();
    let u128_val = ((h as u128) << 64) | (h as u128 ^ 0xa5a5_a5a5_a5a5_a5a5);
    uuid::Uuid::from_u128(u128_val).to_string()
}

/// Derives a deterministic conversation affinity identifier from the root prompt invariant.
///
/// In multi-turn chat/agent conversations, the system prompt and the initial user prompt
/// remain identical across turn 1, 2, ... N. By hashing these invariant root messages,
/// requests without explicit client session headers still achieve stable upstream session
/// affinity and maximum prefix prompt KV cache reuse.
pub fn derive_conversation_affinity(openai_req: &OpenAIRequest) -> String {
    let mut hasher = DefaultHasher::new();

    // 1. Hash system prompt if present (establishes system instructions baseline)
    if let Some(sys) = openai_req.messages.iter().find(|m| m.role == "system") {
        sys.role.hash(&mut hasher);
        openproxy_types::extract_content_text(&sys.content).hash(&mut hasher);
    }

    // 2. Hash first user message (the root prompt invariant across all conversation turns)
    if let Some(user) = openai_req.messages.iter().find(|m| m.role == "user") {
        user.role.hash(&mut hasher);
        openproxy_types::extract_content_text(&user.content).hash(&mut hasher);
    } else if let Some(first) = openai_req.messages.first() {
        first.role.hash(&mut hasher);
        openproxy_types::extract_content_text(&first.content).hash(&mut hasher);
    }

    format!("sess-openproxy-{:016x}", hasher.finish())
}

/// Extracts an explicit session ID from request headers or request body,
/// or derives a deterministic conversation affinity string from conversation history.
pub fn resolve_canonical_session(
    request_headers: &BTreeMap<String, String>,
    openai_req: &OpenAIRequest,
) -> String {
    const CANDIDATE_HEADERS: &[&str] = &[
        "x-session-id",
        "session-id",
        "session_id",
        "x-conversation-id",
        "conversation-id",
        "conversation_id",
        "x-session-affinity",
        "x-opencode-session",
        "x-codex-session",
        "x-codebuddy-session-id",
        "x-command-code-session-id",
        "x-commandcode-session-id",
        "x-mavis-session-id",
        "thread-id",
        "thread_id",
    ];

    // 1. Check explicit downstream request headers
    for &header_name in CANDIDATE_HEADERS {
        if let Some(val) = get_header_val(request_headers, header_name) {
            let clean = val.trim().trim_matches('"');
            if !clean.is_empty() {
                return clean.to_string();
            }
        }
    }

    // 2. Check OpenAIRequest explicit metadata (user or extra fields)
    if let Some(ref user) = openai_req.user {
        let clean = user.trim().trim_matches('"');
        if !clean.is_empty() {
            return clean.to_string();
        }
    }

    if let Some(extra_session) = openai_req
        .extra
        .get("session_id")
        .or_else(|| openai_req.extra.get("conversation_id"))
        .and_then(|v| v.as_str())
    {
        let clean = extra_session.trim().trim_matches('"');
        if !clean.is_empty() {
            return clean.to_string();
        }
    }

    // 3. Fallback: derive deterministic conversation affinity
    derive_conversation_affinity(openai_req)
}

// ---------------------------------------------------------------------------
// Concrete Provider Session Translators
// ---------------------------------------------------------------------------

/// Session translator for OpenAI Codex (`chatgpt.com`).
///
/// Propagates all known session affinity headers used by OpenAI and Cloudflare ingress.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodexSessionTranslator;

impl ProviderSessionTranslator for CodexSessionTranslator {
    fn apply_session(&self, headers: &mut Vec<(String, String)>, canonical_session: &str) {
        const CODEX_SESSION_HEADERS: &[&str] = &[
            "session-id",
            "x-session-id",
            "x-conversation-id",
            "x-codex-session",
            "x-session-affinity",
        ];
        for key in CODEX_SESSION_HEADERS {
            if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(key)) {
                headers.push((key.to_string(), canonical_session.to_string()));
            }
        }
    }
}

/// Session translator for OpenCode.
///
/// Converts arbitrary session tokens into valid OpenCode `ses_{hex24}` identifiers.
#[derive(Debug, Clone, Copy, Default)]
pub struct OpenCodeSessionTranslator;

impl ProviderSessionTranslator for OpenCodeSessionTranslator {
    fn apply_session(&self, headers: &mut Vec<(String, String)>, canonical_session: &str) {
        let translated = translate_session_id(canonical_session, None);
        upsert_header(headers, "x-opencode-session", translated);
    }
}

/// Session translator for CodeBuddy (Tencent Cloud).
///
/// Tencent Cloud requires UUID formatting for `x-conversation-id`. Explicit downstream
/// session headers are preserved, while derived sessions are mapped to deterministic UUIDs.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodeBuddySessionTranslator;

impl ProviderSessionTranslator for CodeBuddySessionTranslator {
    fn apply_session(&self, headers: &mut Vec<(String, String)>, canonical_session: &str) {
        let session_val = if !canonical_session.is_empty()
            && !canonical_session.starts_with("sess-openproxy-")
        {
            canonical_session.to_string()
        } else {
            format_as_uuid(canonical_session)
        };
        upsert_header(headers, "x-conversation-id", session_val);
    }
}

/// Session translator for Command Code Go.
///
/// Binds session affinity to `x-session-id`, `x-conversation-id`, and `x-session-affinity`.
#[derive(Debug, Clone, Copy, Default)]
pub struct CommandCodeSessionTranslator;

impl ProviderSessionTranslator for CommandCodeSessionTranslator {
    fn apply_session(&self, headers: &mut Vec<(String, String)>, canonical_session: &str) {
        upsert_header(headers, "x-session-id", canonical_session.to_string());
        upsert_header(headers, "x-conversation-id", canonical_session.to_string());
        upsert_header(headers, "x-session-affinity", canonical_session.to_string());
    }
}

/// Session translator for MiniMax Coding (Mavis).
///
/// Normalizes session affinity to `x-mavis-session-id` with required `session_` prefix.
#[derive(Debug, Clone, Copy, Default)]
pub struct MiniMaxSessionTranslator;

impl ProviderSessionTranslator for MiniMaxSessionTranslator {
    fn apply_session(&self, headers: &mut Vec<(String, String)>, canonical_session: &str) {
        let clean = canonical_session.trim();
        let session_val = if clean.starts_with("session_") {
            clean.to_string()
        } else {
            format!("session_{clean}")
        };
        upsert_header(headers, "x-mavis-session-id", session_val);
    }
}

/// Session translator for Kiro AI (AWS CodeWhisperer Bedrock).
///
/// Maps session continuity to `x-conversation-id`.
#[derive(Debug, Clone, Copy, Default)]
pub struct KiroSessionTranslator;

impl ProviderSessionTranslator for KiroSessionTranslator {
    fn apply_session(&self, headers: &mut Vec<(String, String)>, canonical_session: &str) {
        upsert_header(headers, "x-conversation-id", canonical_session.to_string());
    }
}

/// Session translator for Google Antigravity (Cloud Code).
///
/// Maps session continuity to `x-vscode-sessionid` and `x-session-affinity`.
#[derive(Debug, Clone, Copy, Default)]
pub struct AntigravitySessionTranslator;

impl ProviderSessionTranslator for AntigravitySessionTranslator {
    fn apply_session(&self, headers: &mut Vec<(String, String)>, canonical_session: &str) {
        let uuid_val = format_as_uuid(canonical_session);
        upsert_header(headers, "x-vscode-sessionid", uuid_val);
        upsert_header(headers, "x-session-affinity", canonical_session.to_string());
    }
}

/// Default session translator for standard and generic LLM providers.
#[derive(Debug, Clone, Copy, Default)]
pub struct DefaultSessionTranslator;

impl ProviderSessionTranslator for DefaultSessionTranslator {
    fn apply_session(&self, headers: &mut Vec<(String, String)>, canonical_session: &str) {
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-session-affinity")) {
            headers.push(("x-session-affinity".to_string(), canonical_session.to_string()));
        }
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("x-conversation-id")) {
            headers.push(("x-conversation-id".to_string(), canonical_session.to_string()));
        }
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("session-id")) {
            headers.push(("session-id".to_string(), canonical_session.to_string()));
        }
    }
}

/// Returns the static [`ProviderSessionTranslator`] matching the provider or adapter.
pub fn resolve_session_translator(
    provider_id: &str,
    adapter_id: &str,
) -> &'static dyn ProviderSessionTranslator {
    let matches = |prefix: &str| provider_id.starts_with(prefix) || adapter_id.starts_with(prefix);

    if matches("codex") {
        &CodexSessionTranslator
    } else if matches("opencode") {
        &OpenCodeSessionTranslator
    } else if matches("codebuddy") {
        &CodeBuddySessionTranslator
    } else if matches("commandcode") || provider_id == "cmd" || adapter_id == "cmd" {
        &CommandCodeSessionTranslator
    } else if matches("minimax") {
        &MiniMaxSessionTranslator
    } else if matches("kiro") {
        &KiroSessionTranslator
    } else if matches("antigravity") || provider_id == "agy" || adapter_id == "agy" {
        &AntigravitySessionTranslator
    } else {
        &DefaultSessionTranslator
    }
}

/// Seamlessly resolves canonical session affinity and applies provider-specific headers.
pub fn apply_provider_session_affinity(
    headers: &mut Vec<(String, String)>,
    provider_id: &str,
    adapter_id: &str,
    request_headers: &BTreeMap<String, String>,
    openai_req: &OpenAIRequest,
) {
    let canonical = resolve_canonical_session(request_headers, openai_req);
    let translator = resolve_session_translator(provider_id, adapter_id);
    translator.apply_session(headers, &canonical);
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::OpenAIMessage;

    #[test]
    fn test_derive_conversation_affinity_multi_turn_stability() {
        let req_turn1 = OpenAIRequest {
            messages: vec![OpenAIMessage {
                role: "user".into(),
                content: Some(serde_json::json!("Root prompt question")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            }],
            ..Default::default()
        };

        let h1 = derive_conversation_affinity(&req_turn1);
        assert!(h1.starts_with("sess-openproxy-"));

        // Turn 2 adds assistant and 2nd user prompt
        let mut req_turn2 = req_turn1;
        req_turn2.messages.push(OpenAIMessage {
            role: "assistant".into(),
            content: Some(serde_json::json!("Assistant reply")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        });
        req_turn2.messages.push(OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::json!("Second question")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        });

        let h2 = derive_conversation_affinity(&req_turn2);
        assert_eq!(h1, h2, "Affinity must remain invariant across turn 1 and 2");

        // Turn 3 adds another round
        let mut req_turn3 = req_turn2.clone();
        req_turn3.messages.push(OpenAIMessage {
            role: "assistant".into(),
            content: Some(serde_json::json!("Assistant reply 2")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        });
        req_turn3.messages.push(OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::json!("Third question")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        });

        let h3 = derive_conversation_affinity(&req_turn3);
        assert_eq!(h1, h3, "Affinity must remain invariant across turn 1 and 3");
    }

    #[test]
    fn test_derive_conversation_affinity_with_system_prompt() {
        let req1 = OpenAIRequest {
            messages: vec![
                OpenAIMessage {
                    role: "system".into(),
                    content: Some(serde_json::json!("System instruction")),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: Default::default(),
                },
                OpenAIMessage {
                    role: "user".into(),
                    content: Some(serde_json::json!("First user question")),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    extra: Default::default(),
                },
            ],
            ..Default::default()
        };

        let mut req2 = req1.clone();
        req2.messages.push(OpenAIMessage {
            role: "assistant".into(),
            content: Some(serde_json::json!("Answer 1")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        });

        let h1 = derive_conversation_affinity(&req1);
        let h2 = derive_conversation_affinity(&req2);
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_resolve_canonical_session_priority() {
        let mut headers = BTreeMap::new();
        let mut req = OpenAIRequest::default();

        // 1. Explicit header priority
        headers.insert("x-session-id".into(), "downstream-sess-1".into());
        req.user = Some("user-sess-2".into());
        req.extra.insert("session_id".into(), serde_json::json!("extra-sess-3"));
        assert_eq!(resolve_canonical_session(&headers, &req), "downstream-sess-1");

        // 2. OpenAIRequest user priority when header absent
        headers.clear();
        assert_eq!(resolve_canonical_session(&headers, &req), "user-sess-2");

        // 3. OpenAIRequest extra session_id priority when user absent
        req.user = None;
        assert_eq!(resolve_canonical_session(&headers, &req), "extra-sess-3");

        // 4. Fallback to derived affinity
        req.extra.clear();
        req.messages = vec![OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::json!("Test prompt")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: Default::default(),
        }];
        let derived = resolve_canonical_session(&headers, &req);
        assert!(derived.starts_with("sess-openproxy-"));
    }

    fn find_header<'a>(list: &'a [(String, String)], key: &str) -> Option<&'a str> {
        list.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v.as_str())
    }

    #[test]
    fn test_translators_apply_session() {
        // Codex
        let mut codex_hdrs = vec![];
        CodexSessionTranslator.apply_session(&mut codex_hdrs, "sess-test-123");
        assert_eq!(find_header(&codex_hdrs, "session-id"), Some("sess-test-123"));
        assert_eq!(find_header(&codex_hdrs, "x-session-id"), Some("sess-test-123"));
        assert_eq!(find_header(&codex_hdrs, "x-conversation-id"), Some("sess-test-123"));
        assert_eq!(find_header(&codex_hdrs, "x-codex-session"), Some("sess-test-123"));
        assert_eq!(find_header(&codex_hdrs, "x-session-affinity"), Some("sess-test-123"));

        // OpenCode
        let mut opencode_hdrs = vec![];
        OpenCodeSessionTranslator.apply_session(&mut opencode_hdrs, "sess-test-123");
        let opencode_sess = find_header(&opencode_hdrs, "x-opencode-session").unwrap();
        assert!(opencode_sess.starts_with("ses_"));

        // CodeBuddy with derived session -> formatted as valid UUID
        let mut cb_hdrs = vec![];
        CodeBuddySessionTranslator.apply_session(&mut cb_hdrs, "sess-openproxy-12345");
        let cb_sess = find_header(&cb_hdrs, "x-conversation-id").unwrap();
        assert!(uuid::Uuid::parse_str(cb_sess).is_ok());

        // CommandCode
        let mut cmd_hdrs = vec![];
        CommandCodeSessionTranslator.apply_session(&mut cmd_hdrs, "sess-cmd-456");
        assert_eq!(find_header(&cmd_hdrs, "x-session-id"), Some("sess-cmd-456"));
        assert_eq!(find_header(&cmd_hdrs, "x-conversation-id"), Some("sess-cmd-456"));
        assert_eq!(find_header(&cmd_hdrs, "x-session-affinity"), Some("sess-cmd-456"));

        // MiniMax
        let mut minimax_hdrs = vec![];
        MiniMaxSessionTranslator.apply_session(&mut minimax_hdrs, "abc789");
        assert_eq!(find_header(&minimax_hdrs, "x-mavis-session-id"), Some("session_abc789"));

        // Kiro
        let mut kiro_hdrs = vec![];
        KiroSessionTranslator.apply_session(&mut kiro_hdrs, "kiro-sess-1");
        assert_eq!(find_header(&kiro_hdrs, "x-conversation-id"), Some("kiro-sess-1"));

        // Antigravity
        let mut agy_hdrs = vec![];
        AntigravitySessionTranslator.apply_session(&mut agy_hdrs, "agy-sess-1");
        let agy_sess = find_header(&agy_hdrs, "x-vscode-sessionid").unwrap();
        assert!(uuid::Uuid::parse_str(agy_sess).is_ok());
        assert_eq!(find_header(&agy_hdrs, "x-session-affinity"), Some("agy-sess-1"));

        // Default
        let mut def_hdrs = vec![];
        DefaultSessionTranslator.apply_session(&mut def_hdrs, "def-sess-1");
        assert_eq!(find_header(&def_hdrs, "x-session-affinity"), Some("def-sess-1"));
        assert_eq!(find_header(&def_hdrs, "x-conversation-id"), Some("def-sess-1"));
        assert_eq!(find_header(&def_hdrs, "session-id"), Some("def-sess-1"));
    }

    #[test]
    fn test_apply_provider_session_affinity_dispatch() {
        let req_headers = BTreeMap::new();
        let openai_req = OpenAIRequest {
            messages: vec![OpenAIMessage {
                role: "user".into(),
                content: Some(serde_json::json!("Prompt for session dispatch")),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                extra: Default::default(),
            }],
            ..Default::default()
        };

        // Test Antigravity dispatch
        let mut agy_headers = vec![];
        apply_provider_session_affinity(&mut agy_headers, "antigravity", "antigravity", &req_headers, &openai_req);
        assert!(find_header(&agy_headers, "x-vscode-sessionid").is_some());
        assert!(find_header(&agy_headers, "x-session-affinity").is_some());

        // Test CommandCode dispatch
        let mut cmd_headers = vec![];
        apply_provider_session_affinity(&mut cmd_headers, "commandcode", "commandcode", &req_headers, &openai_req);
        assert!(find_header(&cmd_headers, "x-session-id").is_some());
        assert_eq!(find_header(&cmd_headers, "x-session-id"), find_header(&cmd_headers, "x-conversation-id"));
        assert_eq!(find_header(&cmd_headers, "x-session-id"), find_header(&cmd_headers, "x-session-affinity"));
    }
}

