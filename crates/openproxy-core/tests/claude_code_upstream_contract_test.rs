//! Offline integration contract for the captured Claude Code profile runtime.
//!
//! CLIProxyAPI's Go fingerprint and the old network refresh helpers are no
//! longer the source of truth. Exercise the public captured-profile store,
//! pinned adapter and header APIs without network or the host's live cache.

use openproxy_adapters::adapters::claude_code::ClaudeCodeAdapter;
use openproxy_adapters::client_contract::ContractError;
use openproxy_adapters::spoofer::claude_code_profile::{
    ClaudeCodeProfile, ClaudeContractRuntime, capture_candidate, parse_profile,
};
use openproxy_adapters::spoofer::{
    CLAUDE_CODE_TEST_LOCK, ClaudeCodeSpoofer, DEFAULT_CLAUDE_CODE_ANTHROPIC_VERSION,
    DEFAULT_CLAUDE_CODE_BETA, DEFAULT_CLAUDE_CODE_CLI_VERSION, DEFAULT_CLAUDE_CODE_PACKAGE_VERSION,
    DEFAULT_CLAUDE_CODE_RUNTIME_VERSION, DEFAULT_CLAUDE_CODE_UA,
    reset_dynamic_claude_code_overrides, set_dynamic_claude_code_extra_header,
    set_dynamic_claude_code_package_version, set_dynamic_claude_code_runtime_version,
    set_dynamic_claude_code_ua, set_dynamic_claude_code_version,
};
use serde_json::json;

struct TestRuntime {
    runtime: ClaudeContractRuntime,
    root: std::path::PathBuf,
}

impl TestRuntime {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("claude-contract-{}", uuid::Uuid::new_v4()));
        Self {
            runtime: ClaudeContractRuntime::new(&root),
            root,
        }
    }
}

impl Drop for TestRuntime {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct ResetOverrides;

impl Drop for ResetOverrides {
    fn drop(&mut self) {
        reset_dynamic_claude_code_overrides();
    }
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[test]
fn measured_seed_and_headers_are_coherent() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = CLAUDE_CODE_TEST_LOCK
        .lock()
        .map_err(|error| error.to_string())?;
    let _reset = ResetOverrides;
    reset_dynamic_claude_code_overrides();
    let fixture = TestRuntime::new();
    let snapshot = fixture.runtime.load()?;
    let profile = &snapshot.payload;
    assert_eq!(profile.cli_version, DEFAULT_CLAUDE_CODE_CLI_VERSION);
    assert_eq!(profile.package_version, DEFAULT_CLAUDE_CODE_PACKAGE_VERSION);
    assert_eq!(profile.runtime_version, DEFAULT_CLAUDE_CODE_RUNTIME_VERSION);
    let headers =
        ClaudeCodeSpoofer::with_session_id("contract-session").headers_for_profile(profile);
    for (name, expected) in [
        ("User-Agent", DEFAULT_CLAUDE_CODE_UA),
        ("X-App", "cli"),
        ("Anthropic-Version", DEFAULT_CLAUDE_CODE_ANTHROPIC_VERSION),
        ("Anthropic-Dangerous-Direct-Browser-Access", "true"),
        ("Anthropic-Beta", DEFAULT_CLAUDE_CODE_BETA),
        ("X-Stainless-Retry-Count", "0"),
        ("X-Stainless-Runtime", "node"),
        ("X-Stainless-Lang", "js"),
        ("X-Stainless-Timeout", "600"),
        (
            "X-Stainless-Runtime-Version",
            DEFAULT_CLAUDE_CODE_RUNTIME_VERSION,
        ),
        (
            "X-Stainless-Package-Version",
            DEFAULT_CLAUDE_CODE_PACKAGE_VERSION,
        ),
        ("X-Claude-Code-Session-Id", "contract-session"),
    ] {
        assert_eq!(header(&headers, name), Some(expected), "{name}");
    }
    assert!(header(&headers, "x-client-request-id").is_some());
    Ok(())
}

#[test]
fn manual_overrides_reach_pinned_profile_headers() -> Result<(), Box<dyn std::error::Error>> {
    let _lock = CLAUDE_CODE_TEST_LOCK
        .lock()
        .map_err(|error| error.to_string())?;
    let _reset = ResetOverrides;
    reset_dynamic_claude_code_overrides();
    set_dynamic_claude_code_version("2.2.0");
    set_dynamic_claude_code_ua("claude-cli/2.2.0 (external, custom)");
    set_dynamic_claude_code_package_version("0.130.0");
    set_dynamic_claude_code_runtime_version("v28.0.0");
    set_dynamic_claude_code_extra_header("anthropic-beta", "custom-beta");
    set_dynamic_claude_code_extra_header("X-Stainless-Os", "Linux");
    set_dynamic_claude_code_extra_header("X-Stainless-Arch", "x64");
    let headers = ClaudeCodeSpoofer::with_session_id("manual-session")
        .headers_for_profile(&ClaudeCodeProfile::default());
    for (name, expected) in [
        ("User-Agent", "claude-cli/2.2.0 (external, custom)"),
        ("Anthropic-Beta", "custom-beta"),
        ("X-Stainless-Package-Version", "0.130.0"),
        ("X-Stainless-Runtime-Version", "v28.0.0"),
        ("X-Stainless-Os", "Linux"),
        ("X-Stainless-Arch", "x64"),
        ("X-Claude-Code-Session-Id", "manual-session"),
    ] {
        assert_eq!(header(&headers, name), Some(expected), "{name}");
    }
    Ok(())
}

#[test]
fn profile_parser_rejects_invalid_versions_and_request_contracts()
-> Result<(), Box<dyn std::error::Error>> {
    let profile = serde_json::to_value(ClaudeCodeProfile::default())?;
    assert!(parse_profile(&serde_json::to_vec(&profile)?).is_ok());
    for (field, invalid) in [
        ("cli_version", json!("2.1.293\r\nInjected: true")),
        ("cli_version", json!("2.1.293.d7b")),
        ("runtime_version", json!("26.3.0")),
        ("betas", json!(["claude-code-20250219", "invalid,beta"])),
        ("request_defaults", json!({"thinking":{"type":"disabled"}})),
    ] {
        let mut candidate = profile.clone();
        candidate[field] = invalid;
        assert!(
            parse_profile(&serde_json::to_vec(&candidate)?).is_err(),
            "{field}"
        );
    }
    Ok(())
}

#[test]
fn captured_upgrade_persists_and_does_not_mutate_a_pinned_request()
-> Result<(), Box<dyn std::error::Error>> {
    let _lock = CLAUDE_CODE_TEST_LOCK
        .lock()
        .map_err(|error| error.to_string())?;
    let _reset = ResetOverrides;
    reset_dynamic_claude_code_overrides();
    let fixture = TestRuntime::new();
    let baseline = fixture.runtime.load()?;
    let adapter = ClaudeCodeAdapter::new().with_pinned_contract(std::sync::Arc::clone(&baseline));
    let upgraded = ClaudeCodeProfile {
        cli_version: "99.0.0".into(),
        package_version: "99.1.0".into(),
        ..baseline.payload.clone()
    };
    let candidate = capture_candidate(&serde_json::to_vec(&upgraded)?)?;
    let activated = fixture
        .runtime
        .commit(&candidate)?
        .ok_or("upgrade did not activate")?;
    assert_eq!(activated.payload.cli_version, "99.0.0");
    let pinned = adapter
        .pinned_contract()
        .ok_or("adapter has no pinned contract")?;
    assert_eq!(pinned.generation, baseline.generation);
    assert_eq!(pinned.payload.cli_version, DEFAULT_CLAUDE_CODE_CLI_VERSION);
    let headers = ClaudeCodeSpoofer::new().headers_for_profile(&pinned.payload);
    assert_eq!(header(&headers, "User-Agent"), Some(DEFAULT_CLAUDE_CODE_UA));

    let reopened = ClaudeContractRuntime::new(&fixture.root);
    assert_eq!(reopened.load()?.generation, activated.generation);
    let stale = capture_candidate(&serde_json::to_vec(&baseline.payload)?)?;
    assert!(matches!(
        reopened.commit(&stale),
        Err(ContractError::Downgrade { .. })
    ));
    assert_eq!(
        reopened
            .snapshot()
            .ok_or("missing active snapshot")?
            .generation,
        activated.generation
    );
    Ok(())
}
