//! OpenCode canonical header propagation and synthesis.
//!
//! Propagates and translates downstream client sessions, project, and User-Agent
//! into OpenCode canonical request headers. Ensures the 4 upstream Console
//! free-tier header gates are always satisfied:
//! 1. User-Agent: valid opencode version (>= 1.17.0, defaults to canonical OPENCODE_UA)
//! 2. x-opencode-client: cli
//! 3. x-opencode-project: global
//! 4. x-opencode-session: ses_... (canonical descending format)
//! 5. x-opencode-request: msg_... (canonical ascending format)

use openproxy_adapters::spoofer::{
    generate_request_id, generate_session_id, has_valid_opencode_version,
    translate_session_id,
};

pub fn propagate_opencode_headers(
    headers: &mut Vec<(String, String)>,
    request_headers: &std::collections::BTreeMap<String, String>,
    openai_req: &openproxy_types::OpenAIRequest,
) {
    let get_header = |name: &str| -> Option<&str> {
        request_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    let set_header = |headers: &mut Vec<(String, String)>, name: &str, val: String| {
        if let Some(pos) = headers
            .iter()
            .position(|(k, _)| k.eq_ignore_ascii_case(name))
        {
            headers[pos].1 = val;
        } else {
            headers.push((name.to_string(), val));
        }
    };

    // 1. Session affinity: downstream session, user, extra, or generated canonical session
    let downstream_session = get_header("x-opencode-session")
        .or_else(|| get_header("x-session-affinity"))
        .or_else(|| get_header("x-session-id"))
        .or_else(|| get_header("session-id"))
        .or_else(|| get_header("x-conversation-id"))
        .or(openai_req.user.as_deref())
        .or_else(|| {
            openai_req
                .extra
                .get("session_id")
                .or_else(|| openai_req.extra.get("conversation_id"))
                .and_then(|v| v.as_str())
        });

    let session_val = if let Some(raw) = downstream_session
        && !raw.trim().is_empty()
    {
        translate_session_id(raw.trim(), None)
    } else if let Some(existing) = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-opencode-session"))
        .map(|(_, v)| v.clone())
    {
        existing
    } else {
        generate_session_id()
    };
    set_header(headers, "x-opencode-session", session_val);

    // 2. Request ID: downstream request or ensure msg_... is present
    let downstream_req = get_header("x-opencode-request")
        .or_else(|| get_header("x-request-id"))
        .filter(|s| !s.trim().is_empty());
    if let Some(req_id) = downstream_req {
        if openproxy_adapters::spoofer::is_valid_opencode_request_id(req_id.trim()) {
            set_header(headers, "x-opencode-request", req_id.trim().to_string());
        }
    } else if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("x-opencode-request"))
    {
        set_header(headers, "x-opencode-request", generate_request_id());
    }

    // 2b. Parent session: forward if downstream supplied it
    if let Some(parent_session) = get_header("x-parent-session-id")
        .filter(|s| !s.trim().is_empty())
    {
        set_header(headers, "x-parent-session-id", parent_session.trim().to_string());
    }

    // 2c. Anthropic beta: forward if downstream supplied it
    if let Some(beta) = get_header("anthropic-beta")
        .filter(|s| !s.trim().is_empty())
    {
        set_header(headers, "anthropic-beta", beta.trim().to_string());
    }

    // 3. Client: preserve downstream if non-empty, else ensure "cli"
    let client = get_header("x-opencode-client")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("cli");
    set_header(headers, "x-opencode-client", client.to_string());

    // 4. Project: preserve downstream if non-empty, else ensure "global"
    let project = get_header("x-opencode-project")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("global");
    set_header(headers, "x-opencode-project", project.to_string());

    // 5. User-Agent: preserve downstream only if valid opencode version (>= 1.17.0),
    // else ensure current dynamic OpenCode UA
    let cur_ua = openproxy_adapters::spoofer::current_opencode_ua();
    let ua = get_header("user-agent")
        .filter(|u| has_valid_opencode_version(u))
        .unwrap_or(&cur_ua);
    set_header(headers, "User-Agent", ua.to_string());

    // 6. Forward custom x-opencode-* headers (extensions, debugging, dynamic flags)
    for (k, v) in request_headers {
        let lower = k.to_ascii_lowercase();
        if lower.starts_with("x-opencode-")
            && lower != "x-opencode-session"
            && lower != "x-opencode-request"
            && lower != "x-opencode-client"
            && lower != "x-opencode-project"
        {
            set_header(headers, k, v.clone());
        }
    }
}

/// Propagate downstream client headers for Google Antigravity.
///
/// Forwards trace IDs, custom client extension headers, and safe x-goog-* headers
/// while strictly preserving machine identity, auth, and preventing bot-triggering headers.
pub fn propagate_antigravity_headers(
    headers: &mut Vec<(String, String)>,
    request_headers: &std::collections::BTreeMap<String, String>,
) {
    for (k, v) in request_headers {
        let lower = k.to_ascii_lowercase();
        let is_allowed = lower.starts_with("x-cloudaicompanion-")
            || lower.starts_with("x-antigravity-")
            || (lower.starts_with("x-client-") && lower != "x-client-name" && lower != "x-client-version")
            || (lower.starts_with("x-goog-") && lower != "x-goog-api-client" && lower != "x-goog-user-project");

        if is_allowed {
            if let Some(pos) = headers.iter().position(|(hk, _)| hk.eq_ignore_ascii_case(k)) {
                headers[pos].1 = v.clone();
            } else {
                headers.push((k.clone(), v.clone()));
            }
        }
    }
}

/// Propagate downstream client headers for MiniMax Coding / Mavis.
///
/// Forwards `anthropic-beta` (for prompt caching & extended output), `x-mavis-*`,
/// `minimax-*`, and custom client headers while strictly preserving auth credentials.
pub fn propagate_minimax_headers(
    headers: &mut Vec<(String, String)>,
    request_headers: &std::collections::BTreeMap<String, String>,
) {
    for (k, v) in request_headers {
        let lower = k.to_ascii_lowercase();
        let is_allowed = lower == "anthropic-beta"
            || lower.starts_with("x-mavis-")
            || lower.starts_with("x-minimax-")
            || lower.starts_with("minimax-");

        if is_allowed {
            if let Some(pos) = headers.iter().position(|(hk, _)| hk.eq_ignore_ascii_case(k)) {
                headers[pos].1 = v.clone();
            } else {
                headers.push((k.clone(), v.clone()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_adapters::spoofer::OPENCODE_UA;

    #[test]
    fn test_propagate_minimax_headers() {
        let mut headers = vec![
            ("User-Agent".into(), "MiniMaxAgent".into()),
            ("Anthropic-Version".into(), "2023-06-01".into()),
            ("x-api-key".into(), "secret".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("anthropic-beta".into(), "prompt-caching-2024-07-31".into());
        req_headers.insert("x-mavis-agent-id".into(), "custom-agent".into());
        req_headers.insert("x-minimax-feature".into(), "v2".into());
        req_headers.insert("authorization".into(), "override-hack".into());

        propagate_minimax_headers(&mut headers, &req_headers);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("anthropic-beta"), Some("prompt-caching-2024-07-31"));
        assert_eq!(find("x-mavis-agent-id"), Some("custom-agent"));
        assert_eq!(find("x-minimax-feature"), Some("v2"));
        assert_eq!(find("authorization"), None);
    }

    #[test]
    fn test_propagate_opencode_headers_default() {
        let mut headers = vec![("Content-Type".into(), "application/json".into())];
        let req_headers = std::collections::BTreeMap::new();
        let openai_req = openproxy_types::OpenAIRequest {
            model: "big-pickle".into(),
            messages: vec![],
            stream: false,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        };

        propagate_opencode_headers(&mut headers, &req_headers, &openai_req);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("User-Agent"), Some(OPENCODE_UA));
        assert_eq!(find("x-opencode-client"), Some("cli"));
        assert_eq!(find("x-opencode-project"), Some("global"));
        assert!(find("x-opencode-session").unwrap().starts_with("ses_"));
        assert!(find("x-opencode-request").unwrap().starts_with("msg_"));
    }

    #[test]
    fn test_propagate_opencode_headers_custom_user_agent() {
        let mut headers = vec![("User-Agent".into(), OPENCODE_UA.into())];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("user-agent".into(), "opencode/1.19.0".into());
        let openai_req = openproxy_types::OpenAIRequest {
            model: "big-pickle".into(),
            messages: vec![],
            stream: false,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        };

        propagate_opencode_headers(&mut headers, &req_headers, &openai_req);
        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(find("User-Agent"), Some("opencode/1.19.0"));
    }

    #[test]
    fn test_propagate_opencode_dynamic_headers_and_extensions() {
        use openproxy_adapters::spoofer::{
            reset_dynamic_opencode_overrides, set_dynamic_opencode_version,
        };

        reset_dynamic_opencode_overrides();
        set_dynamic_opencode_version("1.30.0");

        let mut headers = vec![("Content-Type".into(), "application/json".into())];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("x-opencode-custom-flag".into(), "speed-mode".into());
        req_headers.insert("x-opencode-debug".into(), "1".into());

        let openai_req = openproxy_types::OpenAIRequest {
            model: "big-pickle".into(),
            messages: vec![],
            stream: false,
            temperature: None,
            max_tokens: None,
            top_p: None,
            stop: None,
            tools: None,
            tool_choice: None,
            top_k: None,
            user: None,
            extra: serde_json::Map::new(),
        };

        propagate_opencode_headers(&mut headers, &req_headers, &openai_req);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("User-Agent"), Some("opencode/1.30.0"));
        assert_eq!(find("x-opencode-custom-flag"), Some("speed-mode"));
        assert_eq!(find("x-opencode-debug"), Some("1"));

        reset_dynamic_opencode_overrides();
    }

    #[test]
    fn test_propagate_antigravity_headers() {
        let mut headers = vec![
            ("User-Agent".into(), "Antigravity/4.3.0".into()),
            ("x-client-name".into(), "antigravity".into()),
            ("x-client-version".into(), "4.3.0".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("x-cloudaicompanion-trace-id".into(), "0x123abc".into());
        req_headers.insert("x-antigravity-custom".into(), "custom-val".into());
        req_headers.insert("x-goog-new-feature".into(), "enabled".into());
        // Prohibited headers must be skipped
        req_headers.insert("x-goog-api-client".into(), "malicious-sdk".into());
        req_headers.insert("x-client-version".into(), "hack".into());

        propagate_antigravity_headers(&mut headers, &req_headers);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("x-cloudaicompanion-trace-id"), Some("0x123abc"));
        assert_eq!(find("x-antigravity-custom"), Some("custom-val"));
        assert_eq!(find("x-goog-new-feature"), Some("enabled"));
        assert_eq!(find("x-goog-api-client"), None);
        assert_eq!(find("x-client-version"), Some("4.3.0"));
    }
}
