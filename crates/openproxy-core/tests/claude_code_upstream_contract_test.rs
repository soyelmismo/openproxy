//! Integration test for 1:1 contract parity between OpenProxy and upstream CLIProxyAPI (Claude Code).
//!
//! Validates:
//! 1. Golden contract spec parity (100% offline, runs on every `cargo test`).
//! 2. Live remote upstream contract verification against `github.com/router-for-me/CLIProxyAPI`
//!    via lightweight remote HTTP queries without cloning or caching git history.
//! 3. Background / dynamic auto-update when fetching models or querying upstream profile.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use openproxy_adapters::adapters::claude_code::{
    ClaudeCodeAdapter, get_claude_code_ua, get_claude_code_version,
};
use openproxy_adapters::spoofer::{
    CLAUDE_CODE_ASYNC_TEST_LOCK, CLAUDE_CODE_TEST_LOCK, ClaudeCodeSpoofer, ClientSpoofer,
    DEFAULT_CLAUDE_CODE_ANTHROPIC_VERSION, DEFAULT_CLAUDE_CODE_BETA,
    DEFAULT_CLAUDE_CODE_CLI_VERSION, DEFAULT_CLAUDE_CODE_PACKAGE_VERSION,
    DEFAULT_CLAUDE_CODE_RUNTIME_VERSION, DEFAULT_CLAUDE_CODE_UA, current_claude_code_beta,
    current_claude_code_package_version, current_claude_code_runtime_version,
    current_claude_code_ua, current_claude_code_version, parse_claude_version,
    parse_cliproxyapi_claude_profile, refresh_claude_code_upstream,
    reset_dynamic_claude_code_overrides, set_dynamic_claude_code_beta,
    set_dynamic_claude_code_extra_header, set_dynamic_claude_code_package_version,
    set_dynamic_claude_code_runtime_version, set_dynamic_claude_code_ua,
    set_dynamic_claude_code_version,
};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};

#[test]
fn test_claude_code_golden_contract_spec_parity() {
    let _guard = CLAUDE_CODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_claude_code_overrides();

    assert_eq!(DEFAULT_CLAUDE_CODE_CLI_VERSION, "2.1.280");
    assert_eq!(DEFAULT_CLAUDE_CODE_PACKAGE_VERSION, "0.112.1");
    assert_eq!(DEFAULT_CLAUDE_CODE_RUNTIME_VERSION, "v26.3.0");
    assert_eq!(DEFAULT_CLAUDE_CODE_UA, "claude-cli/2.1.280 (external, cli)");
    assert_eq!(DEFAULT_CLAUDE_CODE_ANTHROPIC_VERSION, "2023-06-01");

    assert_eq!(current_claude_code_version(), "2.1.280");
    assert_eq!(get_claude_code_version(), "2.1.280");
    assert_eq!(
        current_claude_code_ua(),
        "claude-cli/2.1.280 (external, cli)"
    );
    assert_eq!(get_claude_code_ua(), "claude-cli/2.1.280 (external, cli)");

    let spoofer = ClaudeCodeSpoofer::with_session_id("sess-contract-test");
    let headers = spoofer.headers();
    let find_hdr = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find_hdr("User-Agent"), Some(DEFAULT_CLAUDE_CODE_UA));
    assert_eq!(find_hdr("X-App"), Some("cli"));
    assert_eq!(
        find_hdr("Anthropic-Version"),
        Some(DEFAULT_CLAUDE_CODE_ANTHROPIC_VERSION)
    );
    assert_eq!(
        find_hdr("Anthropic-Dangerous-Direct-Browser-Access"),
        Some("true")
    );
    assert_eq!(find_hdr("Anthropic-Beta"), Some(DEFAULT_CLAUDE_CODE_BETA));
    assert_eq!(find_hdr("X-Stainless-Retry-Count"), Some("0"));
    assert_eq!(find_hdr("X-Stainless-Runtime"), Some("node"));
    assert_eq!(find_hdr("X-Stainless-Lang"), Some("js"));
    assert_eq!(find_hdr("X-Stainless-Timeout"), Some("600"));
    assert_eq!(
        find_hdr("X-Stainless-Runtime-Version"),
        Some(DEFAULT_CLAUDE_CODE_RUNTIME_VERSION)
    );
    assert_eq!(
        find_hdr("X-Stainless-Package-Version"),
        Some(DEFAULT_CLAUDE_CODE_PACKAGE_VERSION)
    );
    assert_eq!(
        find_hdr("X-Claude-Code-Session-Id"),
        Some("sess-contract-test")
    );
    assert!(find_hdr("x-client-request-id").is_some());
}

#[test]
fn test_claude_version_parser_cases() {
    assert_eq!(
        parse_claude_version("claude-cli/2.1.280 (external, cli)"),
        Some("2.1.280".to_string())
    );
    assert_eq!(
        parse_claude_version("claude-cli/2.1.295.d7b (external, cli)"),
        Some("2.1.295.d7b".to_string())
    );
    assert_eq!(
        parse_claude_version("claude-cli/3.0.0"),
        Some("3.0.0".to_string())
    );
    assert_eq!(parse_claude_version("curl/7.68.0"), None);
}

#[test]
fn test_local_cliproxyapi_profile_parser() {
    let local_path = "/tmp/CLIProxyAPI/internal/runtime/executor/helps/claude_device_profile.go";
    if let Ok(content) = std::fs::read_to_string(local_path) {
        let profile = parse_cliproxyapi_claude_profile(&content).expect("parse local go profile");
        assert_eq!(profile.user_agent, "claude-cli/2.1.280 (external, cli)");
        assert_eq!(profile.cli_version, "2.1.280");
        assert_eq!(profile.package_version, "0.112.1");
        assert_eq!(profile.runtime_version, "v26.3.0");
        assert_eq!(profile.os, "MacOS");
        assert_eq!(profile.arch, "arm64");
    }
}

#[test]
fn test_claude_dynamic_overrides_and_headers_propagation() {
    let _guard = CLAUDE_CODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_claude_code_overrides();

    set_dynamic_claude_code_version("2.1.300");
    set_dynamic_claude_code_ua("claude-cli/2.1.300 (external, custom)");
    set_dynamic_claude_code_package_version("0.115.0");
    set_dynamic_claude_code_runtime_version("v26.5.0");
    set_dynamic_claude_code_beta("custom-beta-2026-01-01");
    set_dynamic_claude_code_extra_header("X-Stainless-Os", "Linux");
    set_dynamic_claude_code_extra_header("X-Stainless-Arch", "x64");

    assert_eq!(current_claude_code_version(), "2.1.300");
    assert_eq!(
        current_claude_code_ua(),
        "claude-cli/2.1.300 (external, custom)"
    );
    assert_eq!(current_claude_code_package_version(), "0.115.0");
    assert_eq!(current_claude_code_runtime_version(), "v26.5.0");
    assert_eq!(current_claude_code_beta(), "custom-beta-2026-01-01");

    let spoofer = ClaudeCodeSpoofer::with_session_id("sess-dynamic-123");
    let headers = spoofer.headers();
    let find_hdr = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(
        find_hdr("User-Agent"),
        Some("claude-cli/2.1.300 (external, custom)")
    );
    assert_eq!(find_hdr("Anthropic-Beta"), Some("custom-beta-2026-01-01"));
    assert_eq!(find_hdr("X-Stainless-Package-Version"), Some("0.115.0"));
    assert_eq!(find_hdr("X-Stainless-Runtime-Version"), Some("v26.5.0"));
    assert_eq!(find_hdr("X-Stainless-Os"), Some("Linux"));
    assert_eq!(find_hdr("X-Stainless-Arch"), Some("x64"));
    assert_eq!(find_hdr("X-Claude-Code-Session-Id"), Some("sess-dynamic-123"));

    reset_dynamic_claude_code_overrides();
    assert_eq!(
        current_claude_code_version(),
        DEFAULT_CLAUDE_CODE_CLI_VERSION
    );
    assert_eq!(current_claude_code_ua(), DEFAULT_CLAUDE_CODE_UA);
}

#[tokio::test]
async fn test_live_remote_cliproxyapi_upstream_profile() {
    let client = UpstreamClient::new();
    let url = "https://raw.githubusercontent.com/router-for-me/CLIProxyAPI/main/internal/runtime/executor/helps/claude_device_profile.go";
    let req = UpstreamRequest::get(url);

    let cancel = CancellationToken::new();
    let resp = match client.call(req, TimeoutProfile::OAuth, cancel).await {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Skipping live remote test due to network connectivity: {e}");
            return;
        }
    };

    if !resp.status.is_success() {
        eprintln!(
            "Skipping live remote test due to upstream HTTP status {}",
            resp.status
        );
        return;
    }

    let body = resp
        .collect()
        .await
        .expect("read live cliproxyapi profile body");
    let text = std::str::from_utf8(&body).expect("utf8 profile text");

    let profile = parse_cliproxyapi_claude_profile(text).expect("parse live cliproxyapi profile");
    assert!(
        profile.user_agent.starts_with("claude-cli/"),
        "User-Agent must start with claude-cli/: {}",
        profile.user_agent
    );
    assert!(
        !profile.cli_version.is_empty(),
        "CLI version must not be empty"
    );
    assert!(
        !profile.package_version.is_empty(),
        "Package version must not be empty"
    );
    assert!(
        !profile.runtime_version.is_empty(),
        "Runtime version must not be empty"
    );
    assert_eq!(profile.os, "MacOS");
    assert_eq!(profile.arch, "arm64");
}

#[tokio::test]
async fn test_claude_code_fetch_models_triggers_upstream_refresh() {
    let _lock = CLAUDE_CODE_ASYNC_TEST_LOCK.lock().await;
    reset_dynamic_claude_code_overrides();

    assert_eq!(current_claude_code_version(), "2.1.280");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("mock local addr");

    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let mock_go = r#"
package helps
const (
	defaultClaudeFingerprintUserAgent      = "claude-cli/2.1.299 (external, cli)"
	defaultClaudeFingerprintPackageVersion = "0.119.0"
	defaultClaudeFingerprintRuntimeVersion = "v26.8.0"
	defaultClaudeFingerprintOS             = "Linux"
	defaultClaudeFingerprintArch           = "arm64"
)
"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                mock_go.len(),
                mock_go
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        }
    });

    let mock_url = format!("http://{addr}/claude_device_profile.go");
    unsafe {
        std::env::set_var("OPENPROXY_CLAUDE_CODE_UPSTREAM_URL", &mock_url);
    }

    let client = std::sync::Arc::new(UpstreamClient::new());
    let adapter = ClaudeCodeAdapter::new();

    let models = adapter
        .fetch_models_pipeline(&client, "")
        .await
        .expect("fetch models");
    assert!(!models.is_empty());

    assert_eq!(current_claude_code_version(), "2.1.299");
    assert_eq!(
        current_claude_code_ua(),
        "claude-cli/2.1.299 (external, cli)"
    );
    assert_eq!(current_claude_code_package_version(), "0.119.0");
    assert_eq!(current_claude_code_runtime_version(), "v26.8.0");

    let headers = ClaudeCodeSpoofer::new().headers();
    let find_hdr = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(
        find_hdr("User-Agent"),
        Some("claude-cli/2.1.299 (external, cli)")
    );
    assert_eq!(find_hdr("X-Stainless-Package-Version"), Some("0.119.0"));
    assert_eq!(find_hdr("X-Stainless-Runtime-Version"), Some("v26.8.0"));
    assert_eq!(find_hdr("X-Stainless-Os"), Some("Linux"));

    unsafe {
        std::env::remove_var("OPENPROXY_CLAUDE_CODE_UPSTREAM_URL");
    }
    reset_dynamic_claude_code_overrides();
}

#[tokio::test]
async fn test_refresh_claude_code_upstream_direct() {
    let _lock = CLAUDE_CODE_ASYNC_TEST_LOCK.lock().await;
    reset_dynamic_claude_code_overrides();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock listener");
    let addr = listener.local_addr().expect("mock local addr");

    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf).await;
            let mock_go = r#"
package helps
const (
	defaultClaudeFingerprintUserAgent      = "claude-cli/2.2.0 (external, cli)"
	defaultClaudeFingerprintPackageVersion = "0.125.0"
	defaultClaudeFingerprintRuntimeVersion = "v28.0.0"
	defaultClaudeFingerprintOS             = "MacOS"
	defaultClaudeFingerprintArch           = "arm64"
)
"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                mock_go.len(),
                mock_go
            );
            let _ = stream.write_all(resp.as_bytes()).await;
        }
    });

    let mock_url = format!("http://{addr}/profile.go");
    unsafe {
        std::env::set_var("OPENPROXY_CLAUDE_CODE_UPSTREAM_URL", &mock_url);
    }

    let client = std::sync::Arc::new(UpstreamClient::new());
    let profile = refresh_claude_code_upstream(&client)
        .await
        .expect("refresh profile");
    assert_eq!(profile.cli_version, "2.2.0");
    assert_eq!(current_claude_code_version(), "2.2.0");
    assert_eq!(current_claude_code_package_version(), "0.125.0");
    assert_eq!(current_claude_code_runtime_version(), "v28.0.0");

    unsafe {
        std::env::remove_var("OPENPROXY_CLAUDE_CODE_UPSTREAM_URL");
    }
    reset_dynamic_claude_code_overrides();
}

