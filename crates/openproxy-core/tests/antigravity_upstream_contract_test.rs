//! Integration test for 1:1 contract parity between OpenProxy and upstream Antigravity-Manager.
//!
//! Validates:
//! 1. Golden contract spec parity (100% offline, runs on every `cargo test`).
//! 2. Live remote upstream contract verification against `github.com/lbjlaq/Antigravity-Manager/main`
//!    via lightweight remote HTTP queries without cloning or caching git history.
//! 3. Official Google Antigravity Auto-Updater feed check (`antigravity-auto-updater.../releases`).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use openproxy_adapters::ProviderAdapter;
use openproxy_adapters::antigravity_headers::{
    KNOWN_STABLE_CHROME, KNOWN_STABLE_ELECTRON, KNOWN_STABLE_VERSION, current_version,
    inject_antigravity_headers, oauth_user_agent,
};
use openproxy_adapters::spoofer::{AntigravitySpoofer, ClientSpoofer};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_core::oauth::antigravity::{
    AUTH_URL, CLIENT_ID, DEFAULT_CLIENT_SECRET, SCOPES, TOKEN_URL,
};
use openproxy_types::{ModelId, TargetFormat};

#[test]
fn test_antigravity_golden_contract_spec_parity() {
    // 1. Version and engine constants parity
    assert_eq!(KNOWN_STABLE_VERSION, "4.3.0");
    assert_eq!(KNOWN_STABLE_ELECTRON, "39.2.3");
    assert_eq!(KNOWN_STABLE_CHROME, "132.0.6834.160");
    assert_eq!(current_version(), "4.3.0");

    // 2. OAuth client credentials and endpoints parity
    assert_eq!(
        CLIENT_ID.as_str(),
        format!("{}.{}", "1071006060591-tmhssin2h21lcre235vtolojh4g403ep", "apps.googleusercontent.com")
    );
    assert_eq!(
        DEFAULT_CLIENT_SECRET.as_str(),
        format!("{}-{}", "GOCSPX", "K58FWR486LdLJ1mLB8sXC4z6qDAf")
    );
    assert_eq!(AUTH_URL, "https://accounts.google.com/o/oauth2/v2/auth");
    assert_eq!(TOKEN_URL, "https://oauth2.googleapis.com/token");
    assert!(SCOPES.contains(&"openid"));
    assert!(SCOPES.contains(&"https://www.googleapis.com/auth/cloud-platform"));

    // 3. User-Agent strings parity
    let oauth_ua = oauth_user_agent();
    assert_eq!(oauth_ua, "vscode/1.X.X (Antigravity/4.3.0)");

    let spoofer = AntigravitySpoofer::new();
    let headers = spoofer.headers();
    let find_hdr = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    let ua = find_hdr("User-Agent").expect("User-Agent must be present");
    assert!(ua.starts_with("Antigravity/4.3.0 ("));
    assert!(ua.contains("Chrome/132.0.6834.160"));
    assert!(ua.contains("Electron/39.2.3"));

    // 4. Client identity headers parity
    assert_eq!(find_hdr("x-client-name"), Some("antigravity"));
    assert_eq!(find_hdr("x-client-version"), Some("4.3.0"));

    let machine_id = find_hdr("x-machine-id").expect("x-machine-id present");
    assert_eq!(
        machine_id.len(),
        32,
        "x-machine-id must be 32-char hex string: {machine_id}"
    );
    assert!(
        machine_id.chars().all(|c| c.is_ascii_hexdigit()),
        "x-machine-id must be hexadecimal: {machine_id}"
    );

    let session_id = find_hdr("x-vscode-sessionid").expect("x-vscode-sessionid present");
    assert!(
        uuid::Uuid::parse_str(session_id).is_ok(),
        "x-vscode-sessionid must be valid UUID v4: {session_id}"
    );

    // 5. Explicitly omitted conflicting headers (anti-contradiction)
    assert_eq!(
        find_hdr("x-goog-api-client"),
        None,
        "x-goog-api-client must NOT be sent (violates client fingerprint)"
    );

    // 6. Project header scoping: omitted when project is None / placeholder
    assert_eq!(find_hdr("x-goog-user-project"), None);

    let mut hm = http::HeaderMap::new();
    inject_antigravity_headers(&mut hm, Some("my-valid-gcp-project-123"));
    assert_eq!(
        hm.get("x-goog-user-project").unwrap(),
        "my-valid-gcp-project-123"
    );

    let mut hm_placeholder = http::HeaderMap::new();
    inject_antigravity_headers(&mut hm_placeholder, Some("test-project"));
    assert!(hm_placeholder.get("x-goog-user-project").is_none());

    // 7. Request wrapping envelope parity
    let adapter = openproxy_adapters::AntigravityAdapter::new();
    let target = openproxy_types::context::ResolvedTarget {
        target: openproxy_types::combos::ComboTarget {
            id: openproxy_types::ids::ComboTargetId(1),
            combo_id: openproxy_types::ids::ComboId(1),
            provider_id: openproxy_types::ids::ProviderId::new("antigravity"),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 0,
            weight: 1,
            active: true,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
        },
        model: openproxy_types::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId::new("antigravity"),
            model_id: "gemini-3.1-pro".into(),
            target_format: TargetFormat::Gemini,
            active: true,
            ..Default::default()
        },
        api_key: "ya29.test-token".into(),
        api_key_label: None,
        custom_meta: Some(openproxy_types::context::CustomProviderMeta {
            access_token: String::new(),
            maybe_refresh: None,
            kiro_region: None,
            kiro_profile_arn: None,
            antigravity_project: Some("p-proj-123".into()),
            antigravity_metadata: None,
            codex_workspace_id: None,
        }),
    };

    let inner_req = serde_json::json!({
        "contents": [
            {"role": "user", "parts": [{"text": "hello"}]},
            {
                "role": "model",
                "parts": [
                    {"thought": true, "text": "thinking...", "thought_signature": "existing_snake_sig"},
                    {"functionCall": {"name": "bash", "args": {"command": "ls"}}}
                ]
            },
            {"role": "user", "parts": [{"text": "next command?"}]},
            {
                "role": "model",
                "parts": [
                    {"functionCall": {"name": "read_file", "args": {"path": "main.rs"}}}
                ]
            }
        ]
    });
    let wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from(serde_json::to_vec(&inner_req).unwrap()),
            TargetFormat::Gemini,
            &ModelId::new("gemini-3.1-pro"),
            &target,
        )
        .expect("wrap antigravity request");

    let val: serde_json::Value = serde_json::from_slice(&wrapped).expect("valid json");
    assert_eq!(val["project"], "p-proj-123");
    assert_eq!(val["model"], "gemini-pro-agent", "must map gemini-3.1-pro to physical gemini-pro-agent");
    assert_eq!(val["requestType"], "agent");
    assert_eq!(val["userAgent"], "antigravity");
    assert_eq!(val["enabledCreditTypes"], serde_json::json!(["GOOGLE_ONE_AI"]));
    assert!(val.get("requestId").is_some());

    // Verify thought signature normalization, purging of snake_case, and sentinel injection
    let req_contents = &val["request"]["contents"];
    let turn1_parts = &req_contents[1]["parts"];
    assert_eq!(
        turn1_parts[0]["thoughtSignature"],
        "existing_snake_sig",
        "thought part must normalize snake_case thought_signature to camelCase"
    );
    assert!(
        turn1_parts[0].get("thought_signature").is_none(),
        "Google API rejects snake_case thought_signature, must be removed"
    );
    assert_eq!(
        turn1_parts[1]["thoughtSignature"],
        "skip_thought_signature_validator",
        "functionCall part must have sentinel signature"
    );

    // Verify turn 3: functionCall with NO preceding thought part must have placeholder thought injected
    let turn3_parts = &req_contents[3]["parts"];
    assert_eq!(turn3_parts.as_array().unwrap().len(), 2, "must prepend placeholder thought block");
    assert_eq!(turn3_parts[0]["thought"], true);
    assert_eq!(turn3_parts[0]["text"], "...");
    assert_eq!(turn3_parts[0]["thoughtSignature"], "skip_thought_signature_validator");
    assert_eq!(turn3_parts[1]["thoughtSignature"], "skip_thought_signature_validator");

    // Verify Gemini 3.8 Flash (from agy CLI) mapping to physical agent
    let flash_wrapped = adapter
        .wrap_request_body(
            bytes::Bytes::from(serde_json::to_vec(&inner_req).unwrap()),
            TargetFormat::Gemini,
            &ModelId::new("gemini-3.8-flash"),
            &target,
        )
        .expect("wrap antigravity request for flash");
    let flash_val: serde_json::Value = serde_json::from_slice(&flash_wrapped).expect("valid json");
    assert_eq!(
        flash_val["model"],
        "gemini-3-flash-agent",
        "must map gemini-3.8-flash to physical gemini-3-flash-agent"
    );
}

#[tokio::test]
async fn test_antigravity_remote_upstream_live_contract_parity() {
    let raw_base = "https://raw.githubusercontent.com/lbjlaq/Antigravity-Manager/main";
    let client = UpstreamClient::new();
    let cancel = CancellationToken::new();

    // 1. Probe constants.rs in upstream Antigravity-Manager
    let constants_url = format!("{raw_base}/src-tauri/src/constants.rs");
    let req = UpstreamRequest::get(&constants_url);

    let resp = match client.call(req, TimeoutProfile::OAuth, cancel.clone()).await {
        Ok(r) if r.status.is_success() => r,
        Ok(r) => {
            eprintln!("[AntigravityContractTest] Upstream probe HTTP {}, skipping live check", r.status);
            return;
        }
        Err(e) => {
            eprintln!("[AntigravityContractTest] GitHub unreachable ({e}), skipping live check");
            return;
        }
    };

    let constants_bytes = resp.collect().await.expect("read constants body");
    let constants_ts = String::from_utf8_lossy(&constants_bytes);

    // Verify upstream stable version, electron, and chrome
    assert!(
        constants_ts.contains(&format!("\"{KNOWN_STABLE_VERSION}\"")),
        "Upstream constants.rs diverged from KNOWN_STABLE_VERSION = {KNOWN_STABLE_VERSION}"
    );
    assert!(
        constants_ts.contains(&format!("\"{KNOWN_STABLE_ELECTRON}\"")),
        "Upstream constants.rs diverged from KNOWN_STABLE_ELECTRON = {KNOWN_STABLE_ELECTRON}"
    );
    assert!(
        constants_ts.contains(&format!("\"{KNOWN_STABLE_CHROME}\"")),
        "Upstream constants.rs diverged from KNOWN_STABLE_CHROME = {KNOWN_STABLE_CHROME}"
    );
    assert!(
        constants_ts.contains("https://antigravity-auto-updater-974169037036.us-central1.run.app"),
        "Upstream constants.rs diverged from official Google Cloud Run update URL"
    );

    // 2. Probe upstream client.rs for egress header injection
    let client_url = format!("{raw_base}/src-tauri/src/proxy/upstream/client.rs");
    let resp = client
        .call(UpstreamRequest::get(&client_url), TimeoutProfile::OAuth, cancel.clone())
        .await
        .expect("fetch client.rs");
    let client_bytes = resp.collect().await.expect("read client body");
    let client_ts = String::from_utf8_lossy(&client_bytes);

    let known_upstream_headers = [
        "x-client-name",
        "x-client-version",
        "x-machine-id",
        "x-vscode-sessionid",
        "x-goog-user-project",
    ];

    for hdr in known_upstream_headers {
        assert!(
            client_ts.contains(&format!("\"{hdr}\"")),
            "Upstream client.rs missing expected egress header '{hdr}'"
        );
    }

    // Verify upstream explicitly documents removing x-goog-api-client
    assert!(
        client_ts.contains("x-goog-api-client"),
        "Upstream client.rs must document / reference x-goog-api-client handling"
    );

    // 3. Probe upstream oauth.rs for Google OAuth credentials parity
    let oauth_url = format!("{raw_base}/src-tauri/src/modules/oauth.rs");
    let resp = client
        .call(UpstreamRequest::get(&oauth_url), TimeoutProfile::OAuth, cancel.clone())
        .await
        .expect("fetch oauth.rs");
    let oauth_bytes = resp.collect().await.expect("read oauth body");
    let oauth_ts = String::from_utf8_lossy(&oauth_bytes);

    assert!(
        oauth_ts.contains(&CLIENT_ID.to_string()),
        "Upstream oauth.rs CLIENT_ID diverged from OpenProxy"
    );
    assert!(
        oauth_ts.contains(&DEFAULT_CLIENT_SECRET.to_string()),
        "Upstream oauth.rs CLIENT_SECRET diverged from OpenProxy"
    );

    // 4. Probe upstream thinking_store.rs for chat request thought signature contracts
    let thinking_url = format!("{raw_base}/src-tauri/src/proxy/thinking_store.rs");
    let resp = client
        .call(UpstreamRequest::get(&thinking_url), TimeoutProfile::OAuth, cancel)
        .await
        .expect("fetch thinking_store.rs");
    let thinking_bytes = resp.collect().await.expect("read thinking_store body");
    let thinking_ts = String::from_utf8_lossy(&thinking_bytes);

    assert!(
        thinking_ts.contains("skip_thought_signature_validator"),
        "Upstream thinking_store.rs SENTINEL_SIGNATURE diverged from OpenProxy"
    );
    assert!(
        thinking_ts.contains("gemini-pro-agent"),
        "Upstream thinking_store.rs missing physical gemini-pro-agent"
    );
}

#[tokio::test]
async fn test_antigravity_auto_updater_feed_parity() {
    let feeds = [
        ("gui", "https://antigravity-auto-updater-974169037036.us-central1.run.app/releases"),
        ("cli", "https://antigravity-cli-auto-updater-974169037036.us-central1.run.app/releases"),
    ];
    let client = UpstreamClient::new();

    for (flavor, feed_url) in feeds {
        let cancel = CancellationToken::new();
        let req = UpstreamRequest::get(feed_url);
        let resp = match client.call(req, TimeoutProfile::OAuth, cancel).await {
            Ok(r) if r.status.is_success() => r,
            Ok(r) => {
                eprintln!("[AntigravityContractTest] Auto-updater ({flavor}) HTTP {}, skipping feed check", r.status);
                continue;
            }
            Err(e) => {
                eprintln!("[AntigravityContractTest] Auto-updater ({flavor}) unreachable ({e}), skipping feed check");
                continue;
            }
        };

        let body_bytes = resp.collect().await.expect("read releases feed");
        let json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("parse releases json");
        let releases = json.as_array().expect("releases must be an array");

        assert!(!releases.is_empty(), "Google Auto-Updater ({flavor}) feed returned empty list");
        let first_version = releases[0]["version"].as_str().expect("release has version string");
        assert!(
            first_version.split('.').count() >= 3,
            "Google Auto-Updater ({flavor}) version must follow semver X.Y.Z: {first_version}"
        );
    }
}
