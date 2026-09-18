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
    translate_session_id, upsert_header,
};

#[inline]
fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len()).is_some_and(|sub| sub.eq_ignore_ascii_case(prefix))
}

fn propagate_matching_headers<P>(
    headers: &mut Vec<(String, String)>,
    request_headers: &std::collections::BTreeMap<String, String>,
    predicate: P,
) where
    P: Fn(&str) -> bool,
{
    for (k, v) in request_headers {
        if predicate(k) {
            upsert_header(headers, k, v.clone());
        }
    }
}

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
    upsert_header(headers, "x-opencode-session", session_val);

    // 2. Request ID: downstream request or ensure msg_... is present
    let downstream_req = get_header("x-opencode-request")
        .or_else(|| get_header("x-request-id"))
        .filter(|s| !s.trim().is_empty());
    if let Some(req_id) = downstream_req {
        if openproxy_adapters::spoofer::is_valid_opencode_request_id(req_id.trim()) {
            upsert_header(headers, "x-opencode-request", req_id.trim().to_string());
        }
    } else if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("x-opencode-request"))
    {
        upsert_header(headers, "x-opencode-request", generate_request_id());
    }

    // 2b. Parent session: forward if downstream supplied it
    if let Some(parent_session) = get_header("x-parent-session-id")
        .filter(|s| !s.trim().is_empty())
    {
        upsert_header(headers, "x-parent-session-id", parent_session.trim().to_string());
    }

    // 2c. Anthropic beta: forward if downstream supplied it
    if let Some(beta) = get_header("anthropic-beta")
        .filter(|s| !s.trim().is_empty())
    {
        upsert_header(headers, "anthropic-beta", beta.trim().to_string());
    }

    // 3. Client: preserve downstream if non-empty, else ensure "cli"
    let client = get_header("x-opencode-client")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("cli");
    upsert_header(headers, "x-opencode-client", client.to_string());

    // 4. Project: preserve downstream if non-empty, else ensure "global"
    let project = get_header("x-opencode-project")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("global");
    upsert_header(headers, "x-opencode-project", project.to_string());

    // 5. User-Agent: preserve downstream only if valid opencode version (>= 1.17.0),
    // else ensure current dynamic OpenCode UA
    let cur_ua = openproxy_adapters::spoofer::current_opencode_ua();
    let ua = get_header("user-agent")
        .filter(|u| has_valid_opencode_version(u))
        .unwrap_or(&cur_ua);
    upsert_header(headers, "User-Agent", ua.to_string());

    // 6. Forward custom x-opencode-* headers (extensions, debugging, dynamic flags)
    for (k, v) in request_headers {
        if starts_with_ignore_ascii_case(k, "x-opencode-")
            && !k.eq_ignore_ascii_case("x-opencode-session")
            && !k.eq_ignore_ascii_case("x-opencode-request")
            && !k.eq_ignore_ascii_case("x-opencode-client")
            && !k.eq_ignore_ascii_case("x-opencode-project")
        {
            upsert_header(headers, k, v.clone());
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
    propagate_matching_headers(headers, request_headers, |k| {
        starts_with_ignore_ascii_case(k, "x-cloudaicompanion-")
            || starts_with_ignore_ascii_case(k, "x-antigravity-")
            || (starts_with_ignore_ascii_case(k, "x-client-")
                && !k.eq_ignore_ascii_case("x-client-name")
                && !k.eq_ignore_ascii_case("x-client-version"))
            || (starts_with_ignore_ascii_case(k, "x-goog-")
                && !k.eq_ignore_ascii_case("x-goog-api-client")
                && !k.eq_ignore_ascii_case("x-goog-user-project"))
    });
}

/// Propagate downstream client headers for MiniMax Coding / Mavis.
///
/// Forwards `anthropic-beta` (for prompt caching & extended output), `x-mavis-*`,
/// `minimax-*`, and custom client headers while strictly preserving auth credentials.
/// If downstream passes session or conversation IDs, formats and preserves `x-mavis-session-id`.
pub fn propagate_minimax_headers(
    headers: &mut Vec<(String, String)>,
    request_headers: &std::collections::BTreeMap<String, String>,
) {
    propagate_matching_headers(headers, request_headers, |k| {
        k.eq_ignore_ascii_case("anthropic-beta")
            || starts_with_ignore_ascii_case(k, "x-mavis-")
            || starts_with_ignore_ascii_case(k, "x-minimax-")
            || starts_with_ignore_ascii_case(k, "minimax-")
    });

    // Dynamic session continuity: if downstream supplies session or conversation ID, bind to x-mavis-session-id
    if let Some((_, v)) = request_headers.iter().find(|(k, _)| {
        k.eq_ignore_ascii_case("x-session-id")
            || k.eq_ignore_ascii_case("session-id")
            || k.eq_ignore_ascii_case("x-conversation-id")
    }) && !v.trim().is_empty()
    {
        let clean = v.trim();
        let session_val = if clean.starts_with("session_") {
            clean.to_string()
        } else {
            format!("session_{clean}")
        };
        upsert_header(headers, "x-mavis-session-id", session_val);
    }
}

/// Propagate downstream client headers for Cline.
///
/// Forwards `x-cline-*`, `cline-*`, and canonical IDE context headers (`x-platform`,
/// `x-platform-version`, `x-client-version`, `x-client-type`, `x-core-version`, `x-is-multiroot`)
/// so live Cline extensions dynamically override defaults while strictly preserving auth credentials.
pub fn propagate_cline_headers(
    headers: &mut Vec<(String, String)>,
    request_headers: &std::collections::BTreeMap<String, String>,
) {
    propagate_matching_headers(headers, request_headers, |k| {
        starts_with_ignore_ascii_case(k, "x-cline-")
            || starts_with_ignore_ascii_case(k, "cline-")
            || k.eq_ignore_ascii_case("x-platform")
            || k.eq_ignore_ascii_case("x-platform-version")
            || k.eq_ignore_ascii_case("x-client-version")
            || k.eq_ignore_ascii_case("x-client-type")
            || k.eq_ignore_ascii_case("x-core-version")
            || k.eq_ignore_ascii_case("x-is-multiroot")
    });
}

/// Propagate downstream client headers for Kilocode.
///
/// Forwards `x-kilocode-*`, `kilocode-*`, and client identity headers (`x-client-version`,
/// `x-client-type`, `x-title`, `http-referer`) so live Kilocode tools dynamically override defaults.
pub fn propagate_kilocode_headers(
    headers: &mut Vec<(String, String)>,
    request_headers: &std::collections::BTreeMap<String, String>,
) {
    propagate_matching_headers(headers, request_headers, |k| {
        starts_with_ignore_ascii_case(k, "x-kilocode-")
            || starts_with_ignore_ascii_case(k, "kilocode-")
            || k.eq_ignore_ascii_case("x-client-version")
            || k.eq_ignore_ascii_case("x-client-type")
            || k.eq_ignore_ascii_case("x-title")
            || k.eq_ignore_ascii_case("http-referer")
    });
}

/// Propagate downstream client headers for Codex.
///
/// Forwards `x-codex-*`, `codex-*`, `chatgpt-account-id`, and CLI identity headers
/// (`originator`, `version`, `origin`) while strictly preserving auth credentials.
pub fn propagate_codex_headers(
    headers: &mut Vec<(String, String)>,
    request_headers: &std::collections::BTreeMap<String, String>,
) {
    propagate_matching_headers(headers, request_headers, |k| {
        starts_with_ignore_ascii_case(k, "x-codex-")
            || starts_with_ignore_ascii_case(k, "codex-")
            || k.eq_ignore_ascii_case("chatgpt-account-id")
            || k.eq_ignore_ascii_case("originator")
            || k.eq_ignore_ascii_case("version")
            || k.eq_ignore_ascii_case("origin")
    });
}

/// Propagate downstream client headers for Kiro AI (AWS CodeWhisperer).
///
/// Forwards `x-kiro-*`, `kiro-*`, `anthropic-beta`, `x-amzn-bedrock-cache-control`,
/// `amz-sdk-invocation-id`, `amz-sdk-request`, `tokentype`, and client identity headers
/// (`x-amz-user-agent`). Extracts session affinity from `x-conversation-id`, `x-session-id`,
/// or `session-id`.
pub fn propagate_kiro_headers(
    headers: &mut Vec<(String, String)>,
    request_headers: &std::collections::BTreeMap<String, String>,
) {
    propagate_matching_headers(headers, request_headers, |k| {
        starts_with_ignore_ascii_case(k, "x-kiro-")
            || starts_with_ignore_ascii_case(k, "kiro-")
            || k.eq_ignore_ascii_case("anthropic-beta")
            || k.eq_ignore_ascii_case("x-amzn-bedrock-cache-control")
            || k.eq_ignore_ascii_case("amz-sdk-invocation-id")
            || k.eq_ignore_ascii_case("amz-sdk-request")
            || k.eq_ignore_ascii_case("tokentype")
            || k.eq_ignore_ascii_case("x-amz-user-agent")
    });

    let session_val = request_headers
        .iter()
        .find(|(k, _)| {
            k.eq_ignore_ascii_case("x-conversation-id")
                || k.eq_ignore_ascii_case("x-session-id")
                || k.eq_ignore_ascii_case("session-id")
        })
        .map(|(_, v)| v.as_str());

    if let Some(session_id) = session_val
        && !session_id.trim().is_empty()
    {
        upsert_header(
            headers,
            "x-conversation-id",
            session_id.trim().to_string(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_adapters::spoofer::OPENCODE_UA;

    #[test]
    fn test_propagate_cline_headers() {
        let mut headers = vec![
            ("User-Agent".into(), "Cline/4.1.3".into()),
            ("x-title".into(), "Cline".into()),
            ("Authorization".into(), "Bearer secret".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("x-cline-task-id".into(), "task-999".into());
        req_headers.insert("cline-mode".into(), "act".into());
        req_headers.insert("x-platform".into(), "Visual Studio Code".into());
        req_headers.insert("x-client-version".into(), "4.2.0".into());
        req_headers.insert("x-is-multiroot".into(), "true".into());
        req_headers.insert("authorization".into(), "override-hack".into());

        propagate_cline_headers(&mut headers, &req_headers);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("x-cline-task-id"), Some("task-999"));
        assert_eq!(find("cline-mode"), Some("act"));
        assert_eq!(find("x-platform"), Some("Visual Studio Code"));
        assert_eq!(find("x-client-version"), Some("4.2.0"));
        assert_eq!(find("x-is-multiroot"), Some("true"));
        assert_eq!(find("Authorization"), Some("Bearer secret"));
    }

    #[test]
    fn test_propagate_kilocode_headers() {
        let mut headers = vec![
            ("User-Agent".into(), "Kilo-Code/4.108.0".into()),
            ("x-title".into(), "Kilo Code".into()),
            ("Authorization".into(), "Bearer kl-secret".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("x-kilocode-taskid".into(), "task-kilo-123".into());
        req_headers.insert("x-kilocode-feature".into(), "openclaw".into());
        req_headers.insert("kilocode-org".into(), "kilo-team".into());
        req_headers.insert("x-client-version".into(), "5.0.1".into());
        req_headers.insert("authorization".into(), "override-hack".into());

        propagate_kilocode_headers(&mut headers, &req_headers);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("x-kilocode-taskid"), Some("task-kilo-123"));
        assert_eq!(find("x-kilocode-feature"), Some("openclaw"));
        assert_eq!(find("kilocode-org"), Some("kilo-team"));
        assert_eq!(find("x-client-version"), Some("5.0.1"));
        assert_eq!(find("Authorization"), Some("Bearer kl-secret"));
    }

    #[test]
    fn test_propagate_codex_headers() {
        let mut headers = vec![
            ("User-Agent".into(), "codex-cli/0.144.0 (Windows 10.0.26200; x64)".into()),
            ("origin".into(), "https://chatgpt.com".into()),
            ("originator".into(), "codex_cli_rs".into()),
            ("Authorization".into(), "Bearer codex-tok".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("x-codex-session".into(), "ses-codex-1".into());
        req_headers.insert("chatgpt-account-id".into(), "ws-team-456".into());
        req_headers.insert("codex-subaction".into(), "lint".into());
        req_headers.insert("originator".into(), "codex_exec".into());
        req_headers.insert("version".into(), "0.150.0".into());
        req_headers.insert("authorization".into(), "override-hack".into());

        propagate_codex_headers(&mut headers, &req_headers);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("x-codex-session"), Some("ses-codex-1"));
        assert_eq!(find("chatgpt-account-id"), Some("ws-team-456"));
        assert_eq!(find("codex-subaction"), Some("lint"));
        assert_eq!(find("originator"), Some("codex_exec"));
        assert_eq!(find("version"), Some("0.150.0"));
        assert_eq!(find("Authorization"), Some("Bearer codex-tok"));
    }

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
        req_headers.insert("x-conversation-id".into(), "conv-abc-789".into());
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
        assert_eq!(find("x-mavis-session-id"), Some("session_conv-abc-789"));
        assert_eq!(find("x-api-key"), Some("secret"));
        assert_eq!(find("authorization"), None);
    }

    #[test]
    fn test_propagate_kiro_headers() {
        let mut headers = vec![
            ("Content-Type".into(), "application/json".into()),
            ("x-amz-user-agent".into(), "aws-sdk-js/3.0.0 kiro/0.1".into()),
            ("Authorization".into(), "Bearer kiro-tok".into()),
        ];
        let mut req_headers = std::collections::BTreeMap::new();
        req_headers.insert("tokentype".into(), "API_KEY".into());
        req_headers.insert("x-kiro-profile".into(), "enterprise-1".into());
        req_headers.insert("kiro-task".into(), "analyze".into());
        req_headers.insert("x-conversation-id".into(), "conv-kiro-999".into());
        req_headers.insert("anthropic-beta".into(), "prompt-caching-2024-07-31".into());
        req_headers.insert("x-amzn-bedrock-cache-control".into(), "enable".into());
        req_headers.insert("authorization".into(), "hack".into());

        propagate_kiro_headers(&mut headers, &req_headers);

        let find = |k: &str| {
            headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(k))
                .map(|(_, v)| v.as_str())
        };

        assert_eq!(find("tokentype"), Some("API_KEY"));
        assert_eq!(find("x-kiro-profile"), Some("enterprise-1"));
        assert_eq!(find("kiro-task"), Some("analyze"));
        assert_eq!(find("x-conversation-id"), Some("conv-kiro-999"));
        assert_eq!(find("anthropic-beta"), Some("prompt-caching-2024-07-31"));
        assert_eq!(find("x-amzn-bedrock-cache-control"), Some("enable"));
        assert_eq!(find("Authorization"), Some("Bearer kiro-tok"));
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
