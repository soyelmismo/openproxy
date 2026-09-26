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

use openproxy_adapters::spoofer::{generate_request_id, has_valid_opencode_version};

pub mod session;
pub use session::*;

#[inline]
fn starts_with_ignore_ascii_case(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len())
        .is_some_and(|sub| sub.eq_ignore_ascii_case(prefix))
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

    // 1. Session affinity: downstream session or canonical derivation translated for OpenCode
    let canonical = resolve_canonical_session(request_headers, openai_req);
    OpenCodeSessionTranslator.apply_session(headers, &canonical);

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
    if let Some(parent_session) = get_header("x-parent-session-id").filter(|s| !s.trim().is_empty())
    {
        upsert_header(
            headers,
            "x-parent-session-id",
            parent_session.trim().to_string(),
        );
    }

    // 2c. Anthropic beta: forward if downstream supplied it
    if let Some(beta) = get_header("anthropic-beta").filter(|s| !s.trim().is_empty()) {
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
    let session_val = get_header_val(request_headers, "x-conversation-id")
        .or_else(|| get_header_val(request_headers, "x-session-id"))
        .or_else(|| get_header_val(request_headers, "session-id"));

    if let Some(session_id) = session_val.filter(|s| !s.trim().is_empty()) {
        MiniMaxSessionTranslator.apply_session(headers, session_id.trim());
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
/// Also extracts or synthesizes session affinity (`session-id`, `x-session-id`,
/// `x-conversation-id`, `x-codex-session`) so upstream load balancers route
/// requests for the same conversation to the same GPU worker node for prompt caching.
pub fn propagate_codex_headers(
    headers: &mut Vec<(String, String)>,
    request_headers: &std::collections::BTreeMap<String, String>,
    openai_req: &openproxy_types::OpenAIRequest,
) {
    propagate_matching_headers(headers, request_headers, |k| {
        starts_with_ignore_ascii_case(k, "x-codex-")
            || starts_with_ignore_ascii_case(k, "codex-")
            || k.eq_ignore_ascii_case("chatgpt-account-id")
            || k.eq_ignore_ascii_case("originator")
            || k.eq_ignore_ascii_case("version")
            || k.eq_ignore_ascii_case("origin")
            || k.eq_ignore_ascii_case("session-id")
            || k.eq_ignore_ascii_case("session_id")
            || k.eq_ignore_ascii_case("thread-id")
            || k.eq_ignore_ascii_case("thread_id")
            || k.eq_ignore_ascii_case("x-client-request-id")
            || k.eq_ignore_ascii_case("x-conversation-id")
            || k.eq_ignore_ascii_case("conversation-id")
            || k.eq_ignore_ascii_case("conversation_id")
            || k.eq_ignore_ascii_case("x-session-id")
            || k.eq_ignore_ascii_case("x-openai-internal-codex-residency")
    });

    let canonical = resolve_canonical_session(request_headers, openai_req);
    CodexSessionTranslator.apply_session(headers, &canonical);
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

    let session_val = get_header_val(request_headers, "x-conversation-id")
        .or_else(|| get_header_val(request_headers, "x-session-id"))
        .or_else(|| get_header_val(request_headers, "session-id"));

    if let Some(session_id) = session_val.filter(|s| !s.trim().is_empty()) {
        KiroSessionTranslator.apply_session(headers, session_id.trim());
    }
}

/// Propagate downstream client headers for Command Code Go.
///
/// Forwards `x-command-code-*`, `command-code-*`, `cmd-*`, `x-cli-environment`,
/// `x-project-slug`, `x-taste-learning`, and `x-command-code-version`. Extracts
/// session continuity from `x-conversation-id`, `x-session-id`, or `session-id`.
pub fn propagate_commandcode_headers(
    headers: &mut Vec<(String, String)>,
    request_headers: &std::collections::BTreeMap<String, String>,
) {
    propagate_matching_headers(headers, request_headers, |k| {
        starts_with_ignore_ascii_case(k, "x-command-code-")
            || starts_with_ignore_ascii_case(k, "command-code-")
            || starts_with_ignore_ascii_case(k, "cmd-")
            || k.eq_ignore_ascii_case("x-command-code-version")
            || k.eq_ignore_ascii_case("x-cli-environment")
            || k.eq_ignore_ascii_case("x-project-slug")
            || k.eq_ignore_ascii_case("x-taste-learning")
            || k.eq_ignore_ascii_case("x-session-id")
            || k.eq_ignore_ascii_case("x-session-affinity")
    });

    let session_val = get_header_val(request_headers, "x-conversation-id")
        .or_else(|| get_header_val(request_headers, "x-session-id"))
        .or_else(|| get_header_val(request_headers, "session-id"))
        .or_else(|| get_header_val(request_headers, "x-command-code-session-id"))
        .or_else(|| get_header_val(request_headers, "x-commandcode-session-id"));

    let session_id = session_val.filter(|s| !s.trim().is_empty()).map_or_else(
        || derive_conversation_affinity(&openproxy_types::OpenAIRequest::default()),
        |s| s.trim().to_string(),
    );

    CommandCodeSessionTranslator.apply_session(headers, &session_id);
}

/// Dispatches downstream client header propagation to the appropriate provider adapter.
///
/// Centralizes all provider-specific header translation and session affinity mapping
/// so callers do not duplicate cascading checks.
pub fn propagate_provider_target_headers(
    headers: &mut Vec<(String, String)>,
    provider_id: &str,
    adapter_id: &str,
    req_headers: &std::collections::BTreeMap<String, String>,
    openai_req: &openproxy_types::OpenAIRequest,
    codex_workspace_id: Option<&str>,
) {
    let matches = |prefix: &str| provider_id.starts_with(prefix) || adapter_id.starts_with(prefix);

    if matches("opencode") {
        propagate_opencode_headers(headers, req_headers, openai_req);
    } else if matches("antigravity") || provider_id == "agy" || adapter_id == "agy" {
        propagate_antigravity_headers(headers, req_headers);
    } else if matches("minimax") {
        propagate_minimax_headers(headers, req_headers);
    } else if matches("cline") {
        propagate_cline_headers(headers, req_headers);
    } else if matches("kilocode") {
        propagate_kilocode_headers(headers, req_headers);
    } else if matches("codex") {
        propagate_codex_headers(headers, req_headers, openai_req);
        if let Some(ws) = codex_workspace_id
            && !headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("chatgpt-account-id"))
        {
            headers.push(("chatgpt-account-id".to_string(), ws.to_string()));
        }
    } else if matches("kiro") {
        propagate_kiro_headers(headers, req_headers);
    } else if matches("commandcode") || provider_id == "cmd" || adapter_id == "cmd" {
        propagate_commandcode_headers(headers, req_headers);
    } else if matches("codebuddy") {
        propagate_codebuddy_headers(headers, req_headers, openai_req);
    }

    apply_provider_session_affinity(headers, provider_id, adapter_id, req_headers, openai_req);
}

/// Propagate downstream client headers for CodeBuddy.
///
/// Forwards `x-codebuddy-*`, `codebuddy-*`, and client identity headers
/// (`x-ide-type`, `x-ide-name`, `x-ide-version`, `x-product`, `x-agent-intent`, `x-codebuddy-request`).
/// Preserves downstream CodeBuddy User-Agent and extracts or derives session continuity
/// (`X-Conversation-ID`) to ensure Tencent Cloud load balances multi-turn conversations
/// to the same GPU worker node for automatic prefix KV cache hits.
pub fn propagate_codebuddy_headers(
    headers: &mut Vec<(String, String)>,
    request_headers: &std::collections::BTreeMap<String, String>,
    openai_req: &openproxy_types::OpenAIRequest,
) {
    propagate_matching_headers(headers, request_headers, |k| {
        starts_with_ignore_ascii_case(k, "x-codebuddy-")
            || starts_with_ignore_ascii_case(k, "codebuddy-")
            || starts_with_ignore_ascii_case(k, "x-ide-")
            || k.eq_ignore_ascii_case("x-product")
            || k.eq_ignore_ascii_case("x-agent-intent")
            || k.eq_ignore_ascii_case("x-codebuddy-request")
    });

    if let Some(ua) = get_header_val(request_headers, "user-agent")
        && ua.to_ascii_lowercase().contains("codebuddy")
    {
        upsert_header(headers, "User-Agent", ua.to_string());
    }

    let canonical = resolve_canonical_session(request_headers, openai_req);
    CodeBuddySessionTranslator.apply_session(headers, &canonical);
}

#[cfg(test)]
mod tests;
