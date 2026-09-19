//! Integration test for 1:1 contract parity between OpenProxy and upstream Kiro / AWS CodeWhisperer.
//!
//! Validates:
//! 1. Golden contract spec parity (client registration, device auth, tokens, scopes, profile ARN, headers).
//! 2. Local code parity against `other_projects_examples/OmniRoute/open-sse/executors/kiro.ts`.
//! 3. Dynamic real-time overrides in runtime for endpoints, hosts, and spoofer headers.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use openproxy_adapters::adapters::kiro_ai::kiro_runtime_url;
use openproxy_adapters::spoofer::{
    ClientSpoofer, KIRO_SPOOFING_HEADERS, KIRO_TEST_LOCK, KiroSpoofer, current_kiro_ua,
    reset_dynamic_kiro_overrides, set_dynamic_kiro_extra_header, set_dynamic_kiro_ua,
};
use openproxy_adapters::{KiroAdapter, ProviderAdapter, load_upstream_source};
use openproxy_core::oauth::kiro::{
    DEFAULT_REGION, DEVICE_AUTH_URL, REGISTER_URL, SCOPES, TOKEN_URL, kiro_codewhisperer_host,
    kiro_device_auth_url, kiro_oidc_base_url, kiro_register_url, kiro_social_token_url,
    kiro_token_url,
};
use openproxy_pipeline::stages::target_headers::propagate_kiro_headers;
use openproxy_types::{ModelId, TargetFormat};

// ============================================================================
// 1. Golden Contract Spec Parity
// ============================================================================

#[test]
fn test_kiro_golden_contract_spec_parity() {
    let _guard = KIRO_TEST_LOCK.lock().unwrap();
    reset_dynamic_kiro_overrides();

    // 1. Default OIDC endpoints & region
    assert_eq!(DEFAULT_REGION, "us-east-1");
    assert_eq!(
        REGISTER_URL,
        "https://oidc.us-east-1.amazonaws.com/client/register"
    );
    assert_eq!(
        DEVICE_AUTH_URL,
        "https://oidc.us-east-1.amazonaws.com/device_authorization"
    );
    assert_eq!(TOKEN_URL, "https://oidc.us-east-1.amazonaws.com/token");

    // 2. Canonical CodeWhisperer OAuth scopes
    assert!(SCOPES.contains(&"codewhisperer:completions"));
    assert!(SCOPES.contains(&"codewhisperer:analysis"));
    assert!(SCOPES.contains(&"codewhisperer:conversations"));

    // 3. Spoofer headers contract
    let spoofer = KiroSpoofer;
    let headers = spoofer.headers();
    let find_hdr = |k: &str| {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find_hdr("Content-Type"), Some("application/json"));
    assert_eq!(
        find_hdr("x-amz-user-agent"),
        Some("aws-sdk-js/3.0.0 kiro/0.1")
    );
    assert_eq!(find_hdr("Amz-Sdk-Request"), Some("attempt=1; max=3"));
    assert_eq!(find_hdr("x-amzn-bedrock-cache-control"), Some("enable"));
    assert_eq!(
        find_hdr("anthropic-beta"),
        Some("prompt-caching-2024-07-31")
    );

    let inv_id = find_hdr("Amz-Sdk-Invocation-Id").expect("must have Amz-Sdk-Invocation-Id");
    assert!(
        uuid::Uuid::parse_str(inv_id).is_ok(),
        "Invocation ID must be a valid UUID v4"
    );

    // 4. KiroAdapter headers match spoofer
    let adapter = KiroAdapter::new();
    let model = ModelId::new("claude-3-7-sonnet");
    let adapter_headers = adapter.build_headers("test-token-xyz", TargetFormat::Openai, &model);
    let find_adp = |k: &str| {
        adapter_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(
        find_adp("x-amz-user-agent"),
        Some("aws-sdk-js/3.0.0 kiro/0.1")
    );
    assert_eq!(find_adp("Amz-Sdk-Request"), Some("attempt=1; max=3"));
    assert_eq!(find_adp("Authorization"), Some("Bearer test-token-xyz"));

    // 5. Runtime URL host resolution
    assert_eq!(
        kiro_runtime_url("us-east-1"),
        "https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse"
    );
    assert_eq!(
        kiro_runtime_url("eu-central-1"),
        "https://q.eu-central-1.amazonaws.com/generateAssistantResponse"
    );
}

// ============================================================================
// 2. Upstream Repository Code Drift Parity (Remote HTTP + Local Fallback)
// ============================================================================

#[tokio::test]
async fn test_kiro_remote_or_local_upstream_code_parity() {
    let raw_url =
        "https://raw.githubusercontent.com/diegosouzapw/OmniRoute/main/open-sse/executors/kiro.ts";
    let local_path = "../../other_projects_examples/OmniRoute/open-sse/executors/kiro.ts";

    let Some(executor_src) = load_upstream_source(raw_url, local_path).await else {
        eprintln!(
            "[KiroCodeTest] Skipping upstream kiro.ts check: unable to fetch from remote URL {raw_url} or local path {local_path}"
        );
        return;
    };

    // 1. Verify upstream headers parity
    assert!(
        executor_src.contains("Amz-Sdk-Request"),
        "Upstream kiro.ts must define Amz-Sdk-Request header"
    );
    assert!(
        executor_src.contains("attempt=1; max=3"),
        "Upstream kiro.ts must specify 'attempt=1; max=3'"
    );
    assert!(
        executor_src.contains("Amz-Sdk-Invocation-Id"),
        "Upstream kiro.ts must generate Amz-Sdk-Invocation-Id"
    );
    assert!(
        executor_src.contains("x-amzn-bedrock-cache-control"),
        "Upstream kiro.ts must define x-amzn-bedrock-cache-control"
    );
    assert!(
        executor_src.contains("anthropic-beta"),
        "Upstream kiro.ts must define anthropic-beta"
    );
    assert!(
        executor_src.contains("prompt-caching-2024-07-31"),
        "Upstream kiro.ts prompt caching beta flag must match"
    );

    // 2. Verify all spoofed headers exist in our KiroSpoofer definition
    for &(hdr, _) in KIRO_SPOOFING_HEADERS {
        if hdr != "Content-Type" && hdr != "x-amz-user-agent" {
            assert!(
                executor_src.contains(hdr),
                "Upstream kiro.ts missing header '{hdr}' present in KIRO_SPOOFING_HEADERS"
            );
        }
    }
}

// ============================================================================
// 3. Dynamic Real-time Overrides & Pipeline Propagation
// ============================================================================

#[test]
fn test_kiro_dynamic_overrides_and_pipeline_propagation() {
    let _guard = KIRO_TEST_LOCK.lock().unwrap();
    reset_dynamic_kiro_overrides();

    // 1. In-memory spoofer dynamic overrides
    assert_eq!(current_kiro_ua(), "aws-sdk-js/3.0.0 kiro/0.1");

    set_dynamic_kiro_ua("aws-sdk-js/3.20.0 kiro/0.2.0");
    set_dynamic_kiro_extra_header("x-amzn-codewhisperer-optout", "true");

    let headers = KiroSpoofer.headers();
    let find = |k: &str| {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(
        find("x-amz-user-agent"),
        Some("aws-sdk-js/3.20.0 kiro/0.2.0")
    );
    assert_eq!(find("x-amzn-codewhisperer-optout"), Some("true"));

    reset_dynamic_kiro_overrides();
    assert_eq!(current_kiro_ua(), "aws-sdk-js/3.0.0 kiro/0.1");

    // 2. Dynamic endpoint and URL resolvers via environment variables
    // SAFETY: isolated test holding KIRO_TEST_LOCK
    unsafe {
        std::env::set_var("OPENPROXY_KIRO_OIDC_BASE_URL", "https://mock-oidc.internal");
        std::env::set_var("OPENPROXY_KIRO_HOST", "https://mock-host.internal");
        std::env::set_var(
            "OPENPROXY_KIRO_RUNTIME_URL",
            "https://mock-runtime.internal/gen",
        );
        std::env::set_var(
            "OPENPROXY_KIRO_SOCIAL_TOKEN_URL",
            "https://mock-auth.internal/refresh",
        );
    }

    assert_eq!(kiro_oidc_base_url(None), "https://mock-oidc.internal");
    assert_eq!(
        kiro_register_url(None),
        "https://mock-oidc.internal/client/register"
    );
    assert_eq!(
        kiro_device_auth_url(None),
        "https://mock-oidc.internal/device_authorization"
    );
    assert_eq!(kiro_token_url(None), "https://mock-oidc.internal/token");
    assert_eq!(
        kiro_social_token_url(),
        "https://mock-auth.internal/refresh"
    );
    assert_eq!(
        kiro_codewhisperer_host("us-east-1"),
        "https://mock-host.internal"
    );
    assert_eq!(
        kiro_runtime_url("us-east-1"),
        "https://mock-runtime.internal/gen"
    );

    unsafe {
        std::env::remove_var("OPENPROXY_KIRO_OIDC_BASE_URL");
        std::env::remove_var("OPENPROXY_KIRO_HOST");
        std::env::remove_var("OPENPROXY_KIRO_RUNTIME_URL");
        std::env::remove_var("OPENPROXY_KIRO_SOCIAL_TOKEN_URL");
    }

    assert_eq!(
        kiro_register_url(None),
        "https://oidc.us-east-1.amazonaws.com/client/register"
    );
    assert_eq!(
        kiro_runtime_url("us-east-1"),
        "https://codewhisperer.us-east-1.amazonaws.com/generateAssistantResponse"
    );

    // 3. Pipeline header propagation
    let mut pipe_headers = vec![
        ("Content-Type".into(), "application/json".into()),
        (
            "x-amz-user-agent".into(),
            "aws-sdk-js/3.0.0 kiro/0.1".into(),
        ),
        ("Authorization".into(), "Bearer mock-tok".into()),
    ];
    let mut req_headers = std::collections::BTreeMap::new();
    req_headers.insert("tokentype".into(), "API_KEY".into());
    req_headers.insert("x-kiro-profile".into(), "dev-profile".into());
    req_headers.insert("x-conversation-id".into(), "session-kiro-456".into());
    req_headers.insert("anthropic-beta".into(), "custom-beta".into());
    req_headers.insert("x-amzn-bedrock-cache-control".into(), "disable".into());

    propagate_kiro_headers(&mut pipe_headers, &req_headers);

    let find_pipe = |k: &str| {
        pipe_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find_pipe("tokentype"), Some("API_KEY"));
    assert_eq!(find_pipe("x-kiro-profile"), Some("dev-profile"));
    assert_eq!(find_pipe("x-conversation-id"), Some("session-kiro-456"));
    assert_eq!(find_pipe("anthropic-beta"), Some("custom-beta"));
    assert_eq!(find_pipe("x-amzn-bedrock-cache-control"), Some("disable"));
    assert_eq!(find_pipe("Authorization"), Some("Bearer mock-tok"));
}
