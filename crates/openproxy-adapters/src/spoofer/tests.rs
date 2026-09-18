use super::*;

#[test]
fn test_cline_spoofer() {
    let _guard = CLINE_TEST_LOCK.lock().unwrap();
    reset_dynamic_cline_overrides();
    let spoofer = ClineSpoofer;
    let mut req = UpstreamRequest::get("https://dummy.url");
    spoofer.apply_to_request(&mut req);

    for &(k, v) in CLINE_SPOOFING_HEADERS {
        let header_val = req.headers.get(k).expect("header missing");
        if k == "user-agent" {
            assert_eq!(header_val, current_cline_ua().as_str());
        } else if k == "x-client-version" || k == "x-core-version" {
            assert_eq!(header_val, current_cline_version().as_str());
        } else {
            assert_eq!(header_val, HeaderValue::from_str(v).unwrap());
        }
    }
}

#[test]
fn test_cline_dynamic_version_and_extra_headers() {
    let _guard = CLINE_TEST_LOCK.lock().unwrap();
    reset_dynamic_cline_overrides();

    assert_eq!(current_cline_version(), "4.1.3");
    assert_eq!(current_cline_ua(), "Cline/4.1.3");

    set_dynamic_cline_version("5.0.0");
    assert_eq!(current_cline_version(), "5.0.0");
    assert_eq!(current_cline_ua(), "Cline/5.0.0");

    set_dynamic_cline_extra_header("x-cline-feature", "turbo");

    let headers = ClineSpoofer.headers();
    let find = |key: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());

    assert_eq!(find("user-agent"), Some("Cline/5.0.0"));
    assert_eq!(find("x-client-version"), Some("5.0.0"));
    assert_eq!(find("x-core-version"), Some("5.0.0"));
    assert_eq!(find("x-cline-feature"), Some("turbo"));

    reset_dynamic_cline_overrides();
    assert_eq!(current_cline_version(), "4.1.3");
}

#[test]
fn test_kilocode_spoofer() {
    let _guard = KILOCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_kilocode_overrides();
    let spoofer = KilocodeSpoofer;
    let mut req = UpstreamRequest::get("https://dummy.url");
    spoofer.apply_to_request(&mut req);

    for &(k, v) in KILOCODE_SPOOFING_HEADERS {
        let header_val = req.headers.get(k).expect("header missing");
        if k == "user-agent" {
            assert_eq!(header_val, current_kilocode_ua().as_str());
        } else if k == "x-kilocode-version" || k == "x-client-version" {
            assert_eq!(header_val, current_kilocode_version().as_str());
        } else {
            assert_eq!(header_val, HeaderValue::from_str(v).unwrap());
        }
    }
}

#[test]
fn test_kilocode_dynamic_version_and_extra_headers() {
    let _guard = KILOCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_kilocode_overrides();

    assert_eq!(current_kilocode_version(), "4.108.0");
    assert_eq!(current_kilocode_ua(), "Kilo-Code/4.108.0");

    set_dynamic_kilocode_version("5.0.0");
    assert_eq!(current_kilocode_version(), "5.0.0");
    assert_eq!(current_kilocode_ua(), "Kilo-Code/5.0.0");

    set_dynamic_kilocode_extra_header("x-kilocode-feature", "openclaw");

    let headers = KilocodeSpoofer.headers();
    let find = |key: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());

    assert_eq!(find("user-agent"), Some("Kilo-Code/5.0.0"));
    assert_eq!(find("x-kilocode-version"), Some("5.0.0"));
    assert_eq!(find("x-client-version"), Some("5.0.0"));
    assert_eq!(find("x-kilocode-feature"), Some("openclaw"));

    reset_dynamic_kilocode_overrides();
    assert_eq!(current_kilocode_version(), "4.108.0");
}

#[test]
fn test_codex_spoofer() {
    let _guard = CODEX_TEST_LOCK.lock().unwrap();
    reset_dynamic_codex_overrides();
    let spoofer = CodexSpoofer;
    let mut req = UpstreamRequest::get("https://dummy.url");
    spoofer.apply_to_request(&mut req);

    for &(k, v) in CODEX_SPOOFING_HEADERS {
        let header_val = req.headers.get(k).expect("header missing");
        if k == "user-agent" {
            assert_eq!(header_val, current_codex_ua().as_str());
        } else if k == "version" {
            assert_eq!(header_val, current_codex_version().as_str());
        } else {
            assert_eq!(header_val, HeaderValue::from_str(v).unwrap());
        }
    }
}

#[test]
fn test_codex_dynamic_version_and_extra_headers() {
    let _guard = CODEX_TEST_LOCK.lock().unwrap();
    reset_dynamic_codex_overrides();

    assert_eq!(current_codex_version(), "0.144.0");
    assert_eq!(
        current_codex_ua(),
        "codex-cli/0.144.0 (Windows 10.0.26200; x64)"
    );

    set_dynamic_codex_version("0.150.0");
    assert_eq!(current_codex_version(), "0.150.0");
    assert_eq!(
        current_codex_ua(),
        "codex-cli/0.150.0 (Windows 10.0.26200; x64)"
    );

    set_dynamic_codex_extra_header("x-codex-feature", "subzero");

    let headers = CodexSpoofer.headers();
    let find = |key: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());

    assert_eq!(
        find("user-agent"),
        Some("codex-cli/0.150.0 (Windows 10.0.26200; x64)")
    );
    assert_eq!(find("version"), Some("0.150.0"));
    assert_eq!(find("x-codex-feature"), Some("subzero"));

    reset_dynamic_codex_overrides();
    assert_eq!(current_codex_version(), "0.144.0");
}

fn assert_opencode_id(prefix: &str, id: &str) {
    assert!(id.starts_with(prefix), "expected prefix {prefix}, got {id}");
    assert_eq!(
        id.len(),
        OPENCODE_ID_TOTAL_LEN,
        "total id length mismatch: {id}"
    );
    let suffix = &id[prefix.len()..];
    assert_eq!(
        suffix.len(),
        OPENCODE_ID_SUFFIX_LEN,
        "id suffix length mismatch"
    );
    let (hex_part, b62_part) = suffix.split_at(12);
    assert!(
        hex_part
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
        "first 12 chars of suffix must be lowercase hex: {id}"
    );
    assert!(
        b62_part.chars().all(|c| c.is_ascii_alphanumeric()),
        "trailing 14 chars must be base62 alphanumeric: {id}"
    );
    if prefix == "ses_" {
        assert!(is_valid_opencode_session_id(id), "invalid session id: {id}");
    } else if prefix == "msg_" {
        assert!(is_valid_opencode_request_id(id), "invalid request id: {id}");
    }
}

#[test]
fn test_opencode_spoofer() {
    let _guard = OPENCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_opencode_overrides();
    let spoofer = OpenCodeSpoofer;
    let headers = spoofer.headers();

    // Static headers preserved as-is.
    for &(k, v) in OPENCODE_SPOOFING_HEADERS {
        if k == "User-Agent" {
            let cur_ua = current_opencode_ua();
            assert!(
                headers.iter().any(|(name, val)| name == k && val == &cur_ua),
                "missing dynamic header {k}={cur_ua}"
            );
        } else {
            assert!(
                headers.iter().any(|(name, val)| name == k && val == v),
                "missing static header {k}={v}"
            );
        }
    }

    // Dynamic session/request IDs follow ses_/msg_ + 12 hex + 14 Base62 chars.
    let session_id = headers
        .iter()
        .find(|(k, _)| k == "x-opencode-session")
        .map(|(_, v)| v.as_str())
        .expect("missing x-opencode-session");
    assert_opencode_id("ses_", session_id);

    let request_id = headers
        .iter()
        .find(|(k, _)| k == "x-opencode-request")
        .map(|(_, v)| v.as_str())
        .expect("missing x-opencode-request");
    assert_opencode_id("msg_", request_id);

    // Client flag is fixed to "cli".
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "x-opencode-client" && v == "cli")
    );

    // Two successive calls produce different session IDs.
    let second_headers = spoofer.headers();
    let second_session = second_headers
        .iter()
        .find(|(k, _)| k == "x-opencode-session")
        .map(|(_, v)| v.as_str())
        .expect("missing x-opencode-session");
    assert_ne!(session_id, second_session);
}

#[test]
fn test_opencode_spoofer_apply_to_request() {
    let _guard = OPENCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_opencode_overrides();
    let spoofer = OpenCodeSpoofer;
    let mut req = UpstreamRequest::get("https://dummy.url");
    spoofer.apply_to_request(&mut req);

    let session_id = req
        .headers
        .get("x-opencode-session")
        .expect("missing dynamic x-opencode-session");
    assert_opencode_id("ses_", session_id.to_str().expect("session id not ascii"));

    let request_id = req
        .headers
        .get("x-opencode-request")
        .expect("missing dynamic x-opencode-request");
    assert_opencode_id("msg_", request_id.to_str().expect("request id not ascii"));

    assert_eq!(
        req.headers.get("x-opencode-client").unwrap(),
        HeaderValue::from_str("cli").unwrap()
    );
    assert_eq!(
        req.headers.get("x-opencode-project").unwrap(),
        HeaderValue::from_str("global").unwrap()
    );
    assert_eq!(
        req.headers.get("User-Agent").unwrap(),
        current_opencode_ua().as_str()
    );
    assert!(req.headers.get("opencode-version").is_none());
    assert!(req.headers.get("openai-beta").is_none());
}

#[test]
fn test_opencode_translate_session_id() {
    // Preserves already-valid OpenCode session IDs.
    let valid = "ses_f534dfae8ffeCy4Ee4tLWNygDc";
    assert_eq!(translate_session_id(valid, None), valid);
    assert_eq!(translate_session_id(&format!("  {valid}  "), None), valid);

    // Translates foreign / UUID identities deterministically into valid canonical IDs.
    let raw_uuid = "claude:550e8400-e29b-41d4-a716-446655440000";
    let translated1 = translate_session_id(raw_uuid, Some("claude"));
    let translated2 = translate_session_id(raw_uuid, Some("claude"));
    assert_eq!(translated1, translated2);
    assert_opencode_id("ses_", &translated1);

    // Different tools or session inputs produce distinct sessions.
    let different_tool = translate_session_id(raw_uuid, Some("cursor"));
    assert_ne!(translated1, different_tool);
    assert_opencode_id("ses_", &different_tool);
}

#[test]
fn test_opencode_version_validation() {
    assert!(has_valid_opencode_version("opencode/1.18.31"));
    assert!(has_valid_opencode_version("opencode/1.17.0"));
    assert!(has_valid_opencode_version("opencode/1.19.0"));
    assert!(has_valid_opencode_version("opencode/2.0.0"));
    assert!(has_valid_opencode_version(
        "opencode/1.18.31 ai-sdk/provider-utils/4.0.40 runtime/bun/1.3.14"
    ));

    // Outdated (< 1.17) or non-OpenCode versions return false.
    assert!(!has_valid_opencode_version("opencode/1.16.9"));
    assert!(!has_valid_opencode_version("opencode/1.15.0"));
    assert!(!has_valid_opencode_version("opencode"));
    assert!(!has_valid_opencode_version("Claude-Code/1.0"));
    assert!(!has_valid_opencode_version("curl/7.68.0"));
}

#[test]
fn test_opencode_apply_to_header_map_upgrades_and_preserves() {
    let _guard = OPENCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_opencode_overrides();
    let spoofer = OpenCodeSpoofer;

    // Upgrades foreign User-Agent and translates foreign session.
    let mut req1 = UpstreamRequest::get("https://dummy.url");
    req1.headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static("Claude-Code/1.0"),
    );
    req1.headers.insert(
        http::header::HeaderName::from_static("x-opencode-session"),
        HeaderValue::from_static("uuid-1234-5678"),
    );
    spoofer.apply_to_request(&mut req1);

    assert_eq!(
        req1.headers.get(http::header::USER_AGENT).unwrap(),
        current_opencode_ua().as_str()
    );
    let session1 = req1
        .headers
        .get("x-opencode-session")
        .unwrap()
        .to_str()
        .unwrap();
    assert_opencode_id("ses_", session1);
    assert_ne!(session1, "uuid-1234-5678");

    // Preserves authentic downstream OpenCode User-Agent and valid session.
    let mut req2 = UpstreamRequest::get("https://dummy.url");
    req2.headers.insert(
        http::header::USER_AGENT,
        HeaderValue::from_static("opencode/1.19.0 custom-flag"),
    );
    let valid_session = "ses_f534dfae8ffeCy4Ee4tLWNygDc";
    req2.headers.insert(
        http::header::HeaderName::from_static("x-opencode-session"),
        HeaderValue::from_static(valid_session),
    );
    spoofer.apply_to_request(&mut req2);

    assert_eq!(
        req2.headers.get(http::header::USER_AGENT).unwrap(),
        HeaderValue::from_static("opencode/1.19.0 custom-flag")
    );
    assert_eq!(
        req2.headers.get("x-opencode-session").unwrap(),
        HeaderValue::from_static(valid_session)
    );

    // Translates candidate x-session-id when x-opencode-session is absent.
    let mut req3 = UpstreamRequest::get("https://dummy.url");
    req3.headers.insert(
        http::header::HeaderName::from_static("x-session-id"),
        HeaderValue::from_static("foreign-session-abc"),
    );
    spoofer.apply_to_request(&mut req3);
    let session3 = req3
        .headers
        .get("x-opencode-session")
        .unwrap()
        .to_str()
        .unwrap();
    assert_opencode_id("ses_", session3);
    assert_eq!(session3, translate_session_id("foreign-session-abc", None));
}

#[test]
fn test_opencode_dynamic_version_and_extra_headers() {
    let _guard = OPENCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_opencode_overrides();

    assert_eq!(current_opencode_version(), "1.19.0");
    assert_eq!(current_opencode_ua(), "opencode/1.19.0");

    set_dynamic_opencode_version("1.25.4");
    assert_eq!(current_opencode_version(), "1.25.4");
    assert_eq!(current_opencode_ua(), "opencode/1.25.4");

    set_dynamic_opencode_extra_header("x-opencode-canary", "alpha-3");
    let headers = OpenCodeSpoofer.headers();
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "User-Agent" && v == "opencode/1.25.4")
    );
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "x-opencode-canary" && v == "alpha-3")
    );

    let mut req = UpstreamRequest::get("https://dummy.url");
    OpenCodeSpoofer.apply_to_request(&mut req);
    assert_eq!(
        req.headers.get("User-Agent").unwrap(),
        HeaderValue::from_static("opencode/1.25.4")
    );
    assert_eq!(
        req.headers.get("x-opencode-canary").unwrap(),
        HeaderValue::from_static("alpha-3")
    );

    reset_dynamic_opencode_overrides();
    assert_eq!(current_opencode_version(), "1.19.0");
}

#[test]
fn test_antigravity_spoofer() {
    let spoofer = AntigravitySpoofer::with_project("project-xyz");
    let headers = spoofer.headers();
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "x-client-name" && v == "antigravity")
    );
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "x-goog-user-project" && v == "project-xyz")
    );
}

#[test]
fn test_antigravity_spoofer_default() {
    let spoofer = AntigravitySpoofer::new();
    assert!(spoofer.project_id.is_none());
    let headers = spoofer.headers();
    assert!(
        headers
            .iter()
            .any(|(k, v)| k == "x-client-name" && v == "antigravity")
    );
    assert!(!headers.iter().any(|(k, _)| k == "x-goog-user-project"));
}

#[test]
fn test_antigravity_spoofer_invalid_project_skips_header() {
    for invalid_pid in ["", "test-project", "project-id"] {
        let spoofer = AntigravitySpoofer::with_project(invalid_pid);
        let headers = spoofer.headers();
        assert!(
            !headers.iter().any(|(k, _)| k == "x-goog-user-project"),
            "x-goog-user-project header should be omitted for invalid project_id '{invalid_pid}'"
        );

        let mut req = UpstreamRequest::get("https://dummy.url");
        spoofer.apply_to_request(&mut req);
        assert!(
            req.headers.get("x-goog-user-project").is_none(),
            "x-goog-user-project header should be omitted from request for invalid project_id '{invalid_pid}'"
        );
    }
}

#[test]
fn test_antigravity_spoofer_apply_to_request() {
    let spoofer = AntigravitySpoofer::with_project("project-xyz");
    let mut req = UpstreamRequest::get("https://dummy.url");
    spoofer.apply_to_request(&mut req);

    assert_eq!(
        req.headers.get("x-client-name").unwrap(),
        HeaderValue::from_static("antigravity")
    );
    assert_eq!(
        req.headers.get("x-goog-user-project").unwrap(),
        HeaderValue::from_static("project-xyz")
    );
    assert!(req.headers.contains_key("x-client-version"));
    assert!(req.headers.contains_key("x-machine-id"));
    assert!(req.headers.contains_key("x-vscode-sessionid"));
}

#[test]
fn test_minimax_spoofer() {
    let _guard = MINIMAX_TEST_LOCK.lock().unwrap();
    reset_dynamic_minimax_overrides();

    let spoofer = MiniMaxSpoofer;
    let headers = spoofer.headers();
    let find = |key: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());

    assert_eq!(find("User-Agent"), Some(current_minimax_ua().as_str()));
    assert_eq!(find("Anthropic-Version"), Some(current_minimax_anthropic_version().as_str()));
    assert_eq!(find("X-Mavis-Agent-Id"), Some("main"));
    assert_eq!(find("X-Mavis-Timezone-Offset"), Some("0"));
    assert!(find("X-Mavis-Session-Id").is_some_and(|s| s.starts_with("session_")));
}

#[test]
fn test_minimax_dynamic_version_and_extra_headers() {
    let _guard = MINIMAX_TEST_LOCK.lock().unwrap();
    reset_dynamic_minimax_overrides();

    assert_eq!(current_minimax_ua(), "MiniMaxAgent");
    assert_eq!(current_minimax_anthropic_version(), "2023-06-01");

    set_dynamic_minimax_ua("CustomMiniMax/2.0");
    set_dynamic_minimax_anthropic_version("2024-01-01");
    set_dynamic_minimax_extra_header("x-custom-attr", "attr-val");

    assert_eq!(current_minimax_ua(), "CustomMiniMax/2.0");
    assert_eq!(current_minimax_anthropic_version(), "2024-01-01");

    let headers = MiniMaxSpoofer.headers();
    let find = |key: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());

    assert_eq!(find("User-Agent"), Some("CustomMiniMax/2.0"));
    assert_eq!(find("Anthropic-Version"), Some("2024-01-01"));
    assert_eq!(find("x-custom-attr"), Some("attr-val"));

    reset_dynamic_minimax_overrides();
    assert_eq!(current_minimax_ua(), "MiniMaxAgent");
    assert_eq!(current_minimax_anthropic_version(), "2023-06-01");
}

#[test]
fn test_kiro_spoofer() {
    let _guard = KIRO_TEST_LOCK.lock().unwrap();
    reset_dynamic_kiro_overrides();

    let spoofer = KiroSpoofer;
    let headers = spoofer.headers();
    let find = |key: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());

    assert_eq!(find("Content-Type"), Some("application/json"));
    assert_eq!(find("x-amz-user-agent"), Some("aws-sdk-js/3.0.0 kiro/0.1"));
    assert_eq!(find("Amz-Sdk-Request"), Some("attempt=1; max=3"));
    assert_eq!(find("x-amzn-bedrock-cache-control"), Some("enable"));
    assert_eq!(find("anthropic-beta"), Some("prompt-caching-2024-07-31"));
    assert!(find("Amz-Sdk-Invocation-Id").is_some());
}

#[test]
fn test_kiro_dynamic_version_and_extra_headers() {
    let _guard = KIRO_TEST_LOCK.lock().unwrap();
    reset_dynamic_kiro_overrides();

    assert_eq!(current_kiro_ua(), "aws-sdk-js/3.0.0 kiro/0.1");

    set_dynamic_kiro_ua("aws-sdk-js/3.10.0 kiro/1.0");
    set_dynamic_kiro_extra_header("tokentype", "API_KEY");
    set_dynamic_kiro_extra_header("x-kiro-profile", "enterprise-us-east-1");

    assert_eq!(current_kiro_ua(), "aws-sdk-js/3.10.0 kiro/1.0");

    let headers = KiroSpoofer.headers();
    let find = |key: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());

    assert_eq!(find("x-amz-user-agent"), Some("aws-sdk-js/3.10.0 kiro/1.0"));
    assert_eq!(find("tokentype"), Some("API_KEY"));
    assert_eq!(find("x-kiro-profile"), Some("enterprise-us-east-1"));

    reset_dynamic_kiro_overrides();
    assert_eq!(current_kiro_ua(), "aws-sdk-js/3.0.0 kiro/0.1");
}

#[test]
fn test_commandcode_spoofer() {
    let _guard = COMMANDCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_commandcode_overrides();

    let spoofer = CommandCodeSpoofer;
    let headers = spoofer.headers();
    let find = |key: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());

    assert_eq!(find("Content-Type"), Some("application/json"));
    assert_eq!(find("user-agent"), Some("cli"));
    assert_eq!(find("x-command-code-version"), Some("1.54.0"));
    assert_eq!(find("x-cli-environment"), Some("production"));
    assert_eq!(find("x-project-slug"), Some("project"));
    assert_eq!(find("x-taste-learning"), Some("true"));
}

#[test]
fn test_commandcode_dynamic_version_and_extra_headers() {
    let _guard = COMMANDCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_commandcode_overrides();

    assert_eq!(current_commandcode_version(), "1.54.0");
    assert_eq!(current_commandcode_ua(), "cli");

    set_dynamic_commandcode_version("1.60.0");
    set_dynamic_commandcode_ua("command-code-cli/1.60.0");
    set_dynamic_commandcode_extra_header("x-custom-pipeline", "fast");

    assert_eq!(current_commandcode_version(), "1.60.0");
    assert_eq!(current_commandcode_ua(), "command-code-cli/1.60.0");

    let headers = CommandCodeSpoofer.headers();
    let find = |key: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());

    assert_eq!(find("x-command-code-version"), Some("1.60.0"));
    assert_eq!(find("user-agent"), Some("command-code-cli/1.60.0"));
    assert_eq!(find("x-custom-pipeline"), Some("fast"));

    reset_dynamic_commandcode_overrides();
    assert_eq!(current_commandcode_version(), "1.54.0");
    assert_eq!(current_commandcode_ua(), "cli");
}

