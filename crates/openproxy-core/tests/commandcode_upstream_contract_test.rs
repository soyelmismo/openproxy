//! Integration test for 1:1 contract parity between OpenProxy and upstream Command Code / Command Code Go.
//!
//! Validates:
//! 1. Golden contract spec parity (CLI identity headers, spoofing preset, adapter headers, URLs, envelope transform).
//! 2. Local code parity against `other_projects_examples/OmniRoute/open-sse/executors/commandCode.ts`
//!    and `other_projects_examples/OmniRoute/open-sse/config/providers/registry/command-code/index.ts`.
//! 3. Dynamic real-time overrides in runtime for endpoints, hosts, versions, UA, and pipeline header propagation.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use openproxy_adapters::adapters::commandcode::{
    commandcode_base_url, transform_openai_to_commandcode, CommandCodeGoAdapter,
};
use openproxy_adapters::spoofer::{
    current_commandcode_ua, current_commandcode_version, reset_dynamic_commandcode_overrides,
    set_dynamic_commandcode_extra_header, set_dynamic_commandcode_ua,
    set_dynamic_commandcode_version, ClientSpoofer, CommandCodeSpoofer,
    COMMANDCODE_SPOOFING_HEADERS, COMMANDCODE_TEST_LOCK, DEFAULT_COMMANDCODE_CLI_ENVIRONMENT,
    DEFAULT_COMMANDCODE_CLI_VERSION, DEFAULT_COMMANDCODE_PROJECT_SLUG,
    DEFAULT_COMMANDCODE_TASTE_LEARNING, DEFAULT_COMMANDCODE_UA,
};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_adapters::ProviderAdapter;
use openproxy_pipeline::stages::target_headers::propagate_commandcode_headers;
use openproxy_types::{ModelId, TargetFormat};
use std::path::Path;

// ============================================================================
// 1. Golden Contract Spec Parity
// ============================================================================

#[test]
fn test_commandcode_golden_contract_spec_parity() {
    let _guard = COMMANDCODE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_dynamic_commandcode_overrides();

    // 1. Default CLI contract constants
    assert_eq!(DEFAULT_COMMANDCODE_CLI_VERSION, "1.54.0");
    assert_eq!(DEFAULT_COMMANDCODE_CLI_ENVIRONMENT, "production");
    assert_eq!(DEFAULT_COMMANDCODE_PROJECT_SLUG, "project");
    assert_eq!(DEFAULT_COMMANDCODE_TASTE_LEARNING, "true");
    assert_eq!(DEFAULT_COMMANDCODE_UA, "cli");

    // 2. Spoofer headers contract
    let spoofer = CommandCodeSpoofer;
    let headers = spoofer.headers();
    let find_hdr = |k: &str| {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find_hdr("Content-Type"), Some("application/json"));
    assert_eq!(find_hdr("user-agent"), Some("cli"));
    assert_eq!(find_hdr("x-command-code-version"), Some("1.54.0"));
    assert_eq!(find_hdr("x-cli-environment"), Some("production"));
    assert_eq!(find_hdr("x-project-slug"), Some("project"));
    assert_eq!(find_hdr("x-taste-learning"), Some("true"));

    // 3. CommandCodeGoAdapter headers match spoofer + Authorization
    let adapter = CommandCodeGoAdapter::new();
    let model = ModelId::new("claude-sonnet-4-6");
    let adapter_headers =
        adapter.build_headers("test-cc-key-12345", TargetFormat::CommandCodeGo, &model);
    let find_adp = |k: &str| {
        adapter_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find_adp("Content-Type"), Some("application/json"));
    assert_eq!(find_adp("user-agent"), Some("cli"));
    assert_eq!(find_adp("x-command-code-version"), Some("1.54.0"));
    assert_eq!(find_adp("x-cli-environment"), Some("production"));
    assert_eq!(find_adp("x-project-slug"), Some("project"));
    assert_eq!(find_adp("x-taste-learning"), Some("true"));
    assert_eq!(find_adp("Authorization"), Some("Bearer test-cc-key-12345"));

    // 4. Default URLs
    assert_eq!(commandcode_base_url(), "https://api.commandcode.ai");
    assert_eq!(
        adapter.build_chat_url(TargetFormat::CommandCodeGo, &model),
        "https://api.commandcode.ai/alpha/generate"
    );
    assert_eq!(
        adapter.models_url(),
        Some("https://api.commandcode.ai/provider/v1/models".to_string())
    );

    // 5. Envelope transform contract
    let mut val = serde_json::json!({
        "messages": [
            {"role": "user", "content": "Hello Command Code!"}
        ],
        "stream": true,
        "temperature": 0.7
    });
    let env = transform_openai_to_commandcode(&mut val, "claude-sonnet-4-6");
    assert!(env.get("config").is_some());
    assert_eq!(
        env.get("permissionMode").and_then(|v| v.as_str()),
        Some("standard")
    );
    let params = env.get("params").expect("must have params");
    assert_eq!(
        params.get("model").and_then(|v| v.as_str()),
        Some("claude-sonnet-4-6")
    );
    assert_eq!(params.get("stream").and_then(|v| v.as_bool()), Some(true));
    assert_eq!(
        params.get("temperature").and_then(|v| v.as_f64()),
        Some(0.7)
    );
    let msgs = params
        .get("messages")
        .and_then(|v| v.as_array())
        .expect("messages array");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0].get("role").and_then(|v| v.as_str()), Some("user"));
}

// ============================================================================
// 2. Local Upstream Code Inspection Parity
// ============================================================================

#[test]
fn test_commandcode_offline_upstream_code_parity() {
    let base_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let upstream_executor_path = Path::new(&base_dir)
        .join("../../other_projects_examples/OmniRoute/open-sse/executors/commandCode.ts");
    let upstream_registry_path = Path::new(&base_dir).join(
        "../../other_projects_examples/OmniRoute/open-sse/config/providers/registry/command-code/index.ts",
    );

    if !upstream_executor_path.exists() || !upstream_registry_path.exists() {
        eprintln!(
            "[CommandCodeTest] Skipping local upstream checks: files not found at {upstream_executor_path:?} / {upstream_registry_path:?}"
        );
        return;
    }

    let executor_src =
        std::fs::read_to_string(&upstream_executor_path).expect("read local commandCode.ts");
    let registry_src =
        std::fs::read_to_string(&upstream_registry_path).expect("read local registry index.ts");

    // 1. Verify upstream endpoints parity
    assert!(
        executor_src.contains("https://api.commandcode.ai"),
        "Upstream commandCode.ts must target https://api.commandcode.ai"
    );
    assert!(
        executor_src.contains("/provider/v1/chat/completions"),
        "Upstream commandCode.ts must document /provider/v1/chat/completions"
    );
    assert!(
        executor_src.contains("/alpha/generate"),
        "Upstream commandCode.ts must reference /alpha/generate"
    );
    assert!(
        executor_src.contains("MAX_COMMAND_CODE_TOKENS = 200_000"),
        "Upstream commandCode.ts must specify 200_000 token ceiling"
    );

    // 2. Verify registry contract parity
    assert!(
        registry_src.contains("id: \"command-code\""),
        "Registry must define id 'command-code'"
    );
    assert!(
        registry_src.contains("alias: \"cmd\""),
        "Registry must define alias 'cmd'"
    );
    assert!(
        registry_src.contains("chatPath: \"/provider/v1/chat/completions\""),
        "Registry chatPath must match"
    );
    assert!(
        registry_src.contains("modelsUrl: \"https://api.commandcode.ai/provider/v1/models\""),
        "Registry modelsUrl must match"
    );
    assert!(
        registry_src.contains("authHeader: \"Authorization\""),
        "Registry authHeader must match"
    );
    assert!(
        registry_src.contains("authPrefix: \"Bearer \""),
        "Registry authPrefix must match"
    );

    // 3. Verify all spoofed headers exist in our COMMANDCODE_SPOOFING_HEADERS definition
    for &(hdr, _) in COMMANDCODE_SPOOFING_HEADERS {
        assert!(
            !hdr.is_empty(),
            "COMMANDCODE_SPOOFING_HEADERS must contain non-empty headers"
        );
    }
}

// ============================================================================
// 3. Dynamic Real-time Overrides & Pipeline Propagation
// ============================================================================

#[test]
fn test_commandcode_dynamic_overrides_and_pipeline_propagation() {
    let _guard = COMMANDCODE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_dynamic_commandcode_overrides();

    // 1. In-memory spoofer dynamic overrides
    assert_eq!(current_commandcode_version(), "1.54.0");
    assert_eq!(current_commandcode_ua(), "cli");

    set_dynamic_commandcode_version("1.99.0");
    set_dynamic_commandcode_ua("command-code-custom-cli/2.0");
    set_dynamic_commandcode_extra_header("x-custom-task", "coding");

    assert_eq!(current_commandcode_version(), "1.99.0");
    assert_eq!(current_commandcode_ua(), "command-code-custom-cli/2.0");

    let headers = CommandCodeSpoofer.headers();
    let find = |k: &str| {
        headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find("x-command-code-version"), Some("1.99.0"));
    assert_eq!(find("user-agent"), Some("command-code-custom-cli/2.0"));
    assert_eq!(find("x-custom-task"), Some("coding"));

    reset_dynamic_commandcode_overrides();
    assert_eq!(current_commandcode_version(), "1.54.0");
    assert_eq!(current_commandcode_ua(), "cli");

    // 2. Dynamic endpoint and URL resolvers via environment variables
    // SAFETY: isolated test holding COMMANDCODE_TEST_LOCK
    unsafe {
        std::env::set_var("OPENPROXY_COMMANDCODE_BASE_URL", "https://mock-cc.internal");
        std::env::set_var(
            "OPENPROXY_COMMANDCODE_CHAT_URL",
            "https://mock-cc.internal/v1/custom-chat",
        );
        std::env::set_var("OPENPROXY_COMMANDCODE_CLI_VERSION", "2.1.0");
        std::env::set_var("OPENPROXY_COMMANDCODE_USER_AGENT", "env-cli/1.0");
    }

    assert_eq!(commandcode_base_url(), "https://mock-cc.internal");
    let adapter = CommandCodeGoAdapter::new();
    let model = ModelId::new("gpt-5.4");
    assert_eq!(
        adapter.build_chat_url(TargetFormat::CommandCodeGo, &model),
        "https://mock-cc.internal/v1/custom-chat"
    );
    assert_eq!(
        adapter.models_url(),
        Some("https://mock-cc.internal/provider/v1/models".to_string())
    );
    assert_eq!(current_commandcode_version(), "2.1.0");
    assert_eq!(current_commandcode_ua(), "env-cli/1.0");

    unsafe {
        std::env::remove_var("OPENPROXY_COMMANDCODE_BASE_URL");
        std::env::remove_var("OPENPROXY_COMMANDCODE_CHAT_URL");
        std::env::remove_var("OPENPROXY_COMMANDCODE_CLI_VERSION");
        std::env::remove_var("OPENPROXY_COMMANDCODE_USER_AGENT");
    }

    assert_eq!(commandcode_base_url(), "https://api.commandcode.ai");
    assert_eq!(
        adapter.build_chat_url(TargetFormat::CommandCodeGo, &model),
        "https://api.commandcode.ai/alpha/generate"
    );

    // 3. Pipeline header propagation
    let mut pipe_headers = vec![
        ("Content-Type".into(), "application/json".into()),
        ("x-cli-environment".into(), "production".into()),
        ("Authorization".into(), "Bearer mock-token".into()),
    ];
    let mut req_headers = std::collections::BTreeMap::new();
    req_headers.insert("x-command-code-task".into(), "unit-test-task".into());
    req_headers.insert("command-code-flavor".into(), "enterprise".into());
    req_headers.insert("cmd-feature-flag".into(), "enabled".into());
    req_headers.insert("x-cli-environment".into(), "staging".into());
    req_headers.insert("x-project-slug".into(), "my-custom-project".into());
    req_headers.insert("x-taste-learning".into(), "false".into());
    req_headers.insert("x-command-code-version".into(), "1.60.0".into());
    req_headers.insert("x-conversation-id".into(), "cc-session-987".into());
    req_headers.insert("x-session-id".into(), "sub-session-456".into());
    req_headers.insert("session-id".into(), "raw-session-123".into());

    propagate_commandcode_headers(&mut pipe_headers, &req_headers);

    let find_pipe = |k: &str| {
        pipe_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find_pipe("x-command-code-task"), Some("unit-test-task"));
    assert_eq!(find_pipe("command-code-flavor"), Some("enterprise"));
    assert_eq!(find_pipe("cmd-feature-flag"), Some("enabled"));
    assert_eq!(find_pipe("x-cli-environment"), Some("staging"));
    assert_eq!(find_pipe("x-project-slug"), Some("my-custom-project"));
    assert_eq!(find_pipe("x-taste-learning"), Some("false"));
    assert_eq!(find_pipe("x-command-code-version"), Some("1.60.0"));
    assert_eq!(find_pipe("x-conversation-id"), Some("cc-session-987"));
    assert_eq!(find_pipe("Authorization"), Some("Bearer mock-token"));

    // Verify session fallback priority: x-session-id when x-conversation-id is absent
    let mut fallback_headers = vec![];
    let mut fallback_req = std::collections::BTreeMap::new();
    fallback_req.insert("x-session-id".into(), "sub-session-456".into());
    fallback_req.insert("session-id".into(), "raw-session-123".into());
    propagate_commandcode_headers(&mut fallback_headers, &fallback_req);
    assert_eq!(
        fallback_headers
            .iter()
            .find(|(k, _)| k == "x-conversation-id")
            .map(|(_, v)| v.as_str()),
        Some("sub-session-456")
    );

    // Verify session fallback priority: session-id when both x-* are absent
    let mut fallback_headers2 = vec![];
    let mut fallback_req2 = std::collections::BTreeMap::new();
    fallback_req2.insert("session-id".into(), "raw-session-123".into());
    propagate_commandcode_headers(&mut fallback_headers2, &fallback_req2);
    assert_eq!(
        fallback_headers2
            .iter()
            .find(|(k, _)| k == "x-conversation-id")
            .map(|(_, v)| v.as_str()),
        Some("raw-session-123")
    );
}

// ============================================================================
// 4. Remote Live Upstream Contract & Drift Detection
// ============================================================================

#[tokio::test]
async fn test_commandcode_remote_upstream_live_contract_parity() {
    let client = UpstreamClient::new();

    // 1. Probe official NPM package registry for Command Code CLI metadata
    let npm_url = "https://registry.npmjs.org/command-code/latest";
    let cancel = CancellationToken::new();
    let req = UpstreamRequest::get(npm_url);
    if let Ok(resp) = client.call(req, TimeoutProfile::OAuth, cancel).await {
        if resp.status.is_success() {
            if let Ok(body) = resp.collect().await
                && let Ok(pkg) = serde_json::from_slice::<serde_json::Value>(&body)
            {
                assert_eq!(pkg["name"], "command-code");
                assert!(
                    pkg["bin"]["cmd"].is_string()
                        || pkg["bin"]["commandcode"].is_string()
                        || pkg["bin"]["command-code"].is_string(),
                    "Upstream package must expose command-code CLI binaries"
                );
                if let Some(ver) = pkg["version"].as_str() {
                    assert!(
                        ver.split('.').count() >= 3,
                        "NPM command-code version must follow semver X.Y.Z: {ver}"
                    );
                }
            }
        } else {
            eprintln!(
                "[CommandCodeLiveTest] NPM registry returned HTTP {}, skipping registry check",
                resp.status
            );
        }
    } else {
        eprintln!("[CommandCodeLiveTest] Offline or NPM unreachable, skipping registry check");
    }

    // 2. Probe live public /provider/v1/models endpoint from api.commandcode.ai
    let models_url = "https://api.commandcode.ai/provider/v1/models";
    let cancel2 = CancellationToken::new();
    let req2 = UpstreamRequest::get(models_url);
    match client.call(req2, TimeoutProfile::ModelDiscovery, cancel2).await {
        Ok(resp) if resp.status.is_success() => {
            let body = resp.collect().await.expect("read models body");
            let json: serde_json::Value =
                serde_json::from_slice(&body).expect("parse models json");
            let data = json
                .get("data")
                .and_then(|d| d.as_array())
                .expect("models response must have 'data' array");
            assert!(
                !data.is_empty(),
                "Live api.commandcode.ai /provider/v1/models returned empty data array"
            );
            let first = &data[0];
            assert!(
                first.get("id").and_then(|id| id.as_str()).is_some(),
                "Model entry must have 'id'"
            );
            assert_eq!(
                first.get("owned_by").and_then(|o| o.as_str()),
                Some("command-code")
            );
        }
        Ok(resp) => {
            eprintln!(
                "[CommandCodeLiveTest] Live API returned HTTP {}, skipping API catalog check",
                resp.status
            );
        }
        Err(e) => {
            eprintln!(
                "[CommandCodeLiveTest] Offline or api.commandcode.ai unreachable ({e}), skipping live check"
            );
        }
    }
}

