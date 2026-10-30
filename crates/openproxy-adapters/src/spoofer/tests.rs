use super::*;

mod opencode_tests;

fn find_header<'a>(headers: &'a [(String, String)], key: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

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
    let find = |key: &str| find_header(&headers, key);

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
    let find = |key: &str| find_header(&headers, key);

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

    assert_eq!(current_codex_version(), "0.159.0");
    assert_eq!(
        current_codex_ua(),
        "codex-cli/0.159.0 (Windows 10.0.26200; x64)"
    );

    set_dynamic_codex_version("0.160.0");
    assert_eq!(current_codex_version(), "0.160.0");
    assert_eq!(
        current_codex_ua(),
        "codex-cli/0.160.0 (Windows 10.0.26200; x64)"
    );

    set_dynamic_codex_extra_header("x-codex-feature", "subzero");

    let headers = CodexSpoofer.headers();
    let find = |key: &str| find_header(&headers, key);

    assert_eq!(
        find("user-agent"),
        Some("codex-cli/0.160.0 (Windows 10.0.26200; x64)")
    );
    assert_eq!(find("version"), Some("0.160.0"));
    assert_eq!(find("x-codex-feature"), Some("subzero"));

    reset_dynamic_codex_overrides();
    assert_eq!(current_codex_version(), "0.159.0");
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
    let find = |key: &str| find_header(&headers, key);

    assert_eq!(find("User-Agent"), Some(current_minimax_ua().as_str()));
    assert_eq!(
        find("Anthropic-Version"),
        Some(current_minimax_anthropic_version().as_str())
    );
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
    let find = |key: &str| find_header(&headers, key);

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
    let find = |key: &str| find_header(&headers, key);

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
    let find = |key: &str| find_header(&headers, key);

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
    let find = |key: &str| find_header(&headers, key);

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
    let find = |key: &str| find_header(&headers, key);

    assert_eq!(find("x-command-code-version"), Some("1.60.0"));
    assert_eq!(find("user-agent"), Some("command-code-cli/1.60.0"));
    assert_eq!(find("x-custom-pipeline"), Some("fast"));

    reset_dynamic_commandcode_overrides();
    assert_eq!(current_commandcode_version(), "1.54.0");
    assert_eq!(current_commandcode_ua(), "cli");
}

#[test]
fn test_codebuddy_spoofer() {
    let _guard = CODEBUDDY_TEST_LOCK.lock().unwrap();
    reset_dynamic_codebuddy_overrides();
    let spoofer = CodeBuddySpoofer;
    let mut req = UpstreamRequest::get("https://dummy.url");
    spoofer.apply_to_request(&mut req);

    for &(k, v) in CODEBUDDY_SPOOFING_HEADERS {
        let header_val = req.headers.get(k).expect("header missing");
        if k.eq_ignore_ascii_case("user-agent") {
            assert_eq!(header_val, current_codebuddy_ua().as_str());
        } else if k.eq_ignore_ascii_case("x-ide-version") {
            assert_eq!(header_val, current_codebuddy_version().as_str());
        } else {
            assert_eq!(header_val, HeaderValue::from_str(v).unwrap());
        }
    }
}

#[test]
fn test_codebuddy_dynamic_version_and_extra_headers() {
    let _guard = CODEBUDDY_TEST_LOCK.lock().unwrap();
    reset_dynamic_codebuddy_overrides();

    assert_eq!(current_codebuddy_version(), "2.168.2");
    assert_eq!(current_codebuddy_ua(), "CLI/2.168.2 CodeBuddy/2.168.2");

    set_dynamic_codebuddy_version("3.0.0");
    assert_eq!(current_codebuddy_version(), "3.0.0");
    assert_eq!(current_codebuddy_ua(), "CLI/3.0.0 CodeBuddy/3.0.0");

    set_dynamic_codebuddy_extra_header("x-custom-tenant", "tencent-corp");

    let headers = CodeBuddySpoofer.headers();
    let find = |key: &str| find_header(&headers, key);

    assert_eq!(find("User-Agent"), Some("CLI/3.0.0 CodeBuddy/3.0.0"));
    assert_eq!(find("X-IDE-Version"), Some("3.0.0"));
    assert_eq!(find("x-custom-tenant"), Some("tencent-corp"));
    assert_eq!(find("X-IDE-Type"), Some("CLI"));
    assert_eq!(find("X-IDE-Name"), Some("CLI"));
    assert_eq!(find("X-Product"), Some("SaaS"));
    assert_eq!(find("X-Agent-Intent"), Some("craft"));
    assert_eq!(find("x-codebuddy-request"), Some("1"));
    assert_eq!(find("X-Domain"), Some("www.workbuddy.ai"));

    reset_dynamic_codebuddy_overrides();
    assert_eq!(current_codebuddy_version(), "2.168.2");
}

#[test]
fn test_parse_env_extra_headers() {
    let test_var = "TEST_SPOOFER_EXTRA_HEADERS";

    // Missing env var -> returns empty vec
    unsafe {
        std::env::remove_var(test_var);
    }
    assert!(parse_env_extra_headers(test_var).is_empty());

    // Invalid JSON -> returns empty vec
    unsafe {
        std::env::set_var(test_var, "not valid json");
    }
    assert!(parse_env_extra_headers(test_var).is_empty());

    // Valid JSON map -> returns sorted/iterated key-value pairs
    unsafe {
        std::env::set_var(test_var, r#"{"x-foo": "bar", "x-baz": "qux"}"#);
    }
    let parsed = parse_env_extra_headers(test_var);
    assert_eq!(
        parsed,
        vec![
            ("x-baz".to_string(), "qux".to_string()),
            ("x-foo".to_string(), "bar".to_string())
        ]
    );

    unsafe {
        std::env::remove_var(test_var);
    }
}

#[test]
fn test_merge_header_refs() {
    let mut headers = vec![
        ("User-Agent".to_string(), "old-ua".to_string()),
        ("x-custom-1".to_string(), "val-1".to_string()),
    ];

    let extra = vec![
        ("user-agent".to_string(), "new-ua".to_string()),
        ("x-custom-2".to_string(), "val-2".to_string()),
        ("x-openproxy-internal".to_string(), "ignored".to_string()),
        ("X-OPENPROXY-SECRET".to_string(), "ignored-case".to_string()),
    ];

    merge_header_refs(&mut headers, &extra);

    // Should update user-agent (case-insensitive), add x-custom-2, and ignore x-openproxy-*
    assert_eq!(headers.len(), 3);
    assert_eq!(headers[0], ("User-Agent".to_string(), "new-ua".to_string()));
    assert_eq!(headers[1], ("x-custom-1".to_string(), "val-1".to_string()));
    assert_eq!(headers[2], ("x-custom-2".to_string(), "val-2".to_string()));
}
