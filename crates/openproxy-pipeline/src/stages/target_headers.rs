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
    OPENCODE_UA, generate_request_id, generate_session_id, has_valid_opencode_version,
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

    // 2. Request ID: ensure msg_... is present
    if !headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("x-opencode-request"))
    {
        set_header(headers, "x-opencode-request", generate_request_id());
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
    // else ensure OPENCODE_UA
    let ua = get_header("user-agent")
        .filter(|u| has_valid_opencode_version(u))
        .unwrap_or(OPENCODE_UA);
    set_header(headers, "User-Agent", ua.to_string());
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
