//! Integration test for 1:1 contract parity between OpenProxy and upstream Cline / Codex / Kilocode.
//!
//! Validates:
//! 1. Golden contract spec parity against local repository source code (`other_projects_examples/cline`).
//! 2. Live remote upstream contract verification against `github.com/cline/cline/main` and
//!    `github.com/Kilo-Org/kilocode/main` via lightweight remote HTTP queries without cloning.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use openproxy_adapters::spoofer::{
    CLINE_SPOOFING_HEADERS, ClientSpoofer, ClineSpoofer, CodexSpoofer, KilocodeSpoofer,
    current_cline_ua, current_cline_version, current_codex_ua, current_codex_version,
    current_kilocode_ua, current_kilocode_version,
};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_core::oauth::cline::{
    CLINE_AUTH_AUTHORIZE_PATH, CLINE_AUTH_REFRESH_PATH, CLINE_AUTH_TOKEN_PATH,
    CLINE_DEFAULT_BASE_URL,
};
use openproxy_core::oauth::codex::{
    CLIENT_ID as CODEX_CLIENT_ID, SCOPES as CODEX_SCOPES, TOKEN_URL as CODEX_TOKEN_URL,
    VERIFICATION_URI as CODEX_VERIFICATION_URI,
};
use openproxy_core::oauth::refresh::refresh_lead_seconds;
use std::path::Path;

// ============================================================================
// 1. Offline Local Upstream Code Inspection (100% offline, zero network)
// ============================================================================

#[test]
fn test_cline_offline_upstream_code_parity() {
    let base_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let cline_auth_path = Path::new(&base_dir).join("../../other_projects_examples/cline/sdk/packages/core/src/auth/cline.ts");
    let cline_env_path = Path::new(&base_dir).join("../../other_projects_examples/cline/apps/vscode/src/services/EnvUtils.ts");

    if !cline_auth_path.exists() {
        eprintln!("[LocalCodeTest] Skipping local cline auth check: file not found at {cline_auth_path:?}");
        return;
    }

    let auth_src = std::fs::read_to_string(&cline_auth_path).expect("read local cline.ts");

    // 1. Verify endpoint constants match OpenProxy's Cline constants
    assert_eq!(CLINE_DEFAULT_BASE_URL, "https://api.cline.bot");
    assert!(
        auth_src.contains(CLINE_AUTH_AUTHORIZE_PATH),
        "Upstream cline.ts must define authorize endpoint '{CLINE_AUTH_AUTHORIZE_PATH}'"
    );
    assert!(
        auth_src.contains(CLINE_AUTH_TOKEN_PATH),
        "Upstream cline.ts must define token endpoint '{CLINE_AUTH_TOKEN_PATH}'"
    );
    assert!(
        auth_src.contains(CLINE_AUTH_REFRESH_PATH),
        "Upstream cline.ts must define refresh endpoint '{CLINE_AUTH_REFRESH_PATH}'"
    );

    // 2. Verify refresh lead time: DEFAULT_REFRESH_BUFFER_MS = 5 * 60 * 1000 (300s)
    assert!(
        auth_src.contains("DEFAULT_REFRESH_BUFFER_MS = 5 * 60 * 1000"),
        "Upstream cline.ts buffer must be 5 min (300s)"
    );
    assert_eq!(
        refresh_lead_seconds("cline"),
        300,
        "OpenProxy refresh lead time for cline must be 300 seconds"
    );

    // 3. Verify WorkOS device auth endpoints match
    assert!(auth_src.contains("/user_management/authorize/device"));
    assert!(auth_src.contains("/user_management/authenticate"));

    // 4. Verify client headers from EnvUtils.ts
    if cline_env_path.exists() {
        let env_src = std::fs::read_to_string(&cline_env_path).expect("read local EnvUtils.ts");
        let expected_headers = [
            "X-PLATFORM",
            "X-PLATFORM-VERSION",
            "X-CLIENT-VERSION",
            "X-CLIENT-TYPE",
            "X-CORE-VERSION",
            "X-IS-MULTIROOT",
        ];

        for hdr in expected_headers {
            assert!(
                env_src.contains(hdr),
                "Upstream EnvUtils.ts must define header '{hdr}'"
            );
            assert!(
                CLINE_SPOOFING_HEADERS.iter().any(|(k, _)| k.eq_ignore_ascii_case(hdr)),
                "OpenProxy CLINE_SPOOFING_HEADERS missing upstream header '{hdr}'"
            );
        }
    }

    // 5. Verify ClineSpoofer header generation
    let headers = ClineSpoofer.headers();
    let find_hdr = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find_hdr("http-referer"), Some("https://cline.bot"));
    assert_eq!(find_hdr("x-title"), Some("Cline"));
    assert_eq!(find_hdr("user-agent"), Some(current_cline_ua().as_str()));
    assert_eq!(find_hdr("x-client-version"), Some(current_cline_version().as_str()));
    assert_eq!(find_hdr("x-client-type"), Some("VSCode Extension"));
    assert_eq!(find_hdr("x-is-multiroot"), Some("false"));
}

#[test]
fn test_codex_offline_upstream_code_parity() {
    let base_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let codex_auth_path = Path::new(&base_dir).join("../../other_projects_examples/cline/sdk/packages/core/src/auth/codex.ts");

    if !codex_auth_path.exists() {
        eprintln!("[LocalCodeTest] Skipping local codex auth check: file not found at {codex_auth_path:?}");
        return;
    }

    let auth_src = std::fs::read_to_string(&codex_auth_path).expect("read local codex.ts");

    // 1. Verify client ID
    assert!(
        auth_src.contains(&format!("clientId: \"{CODEX_CLIENT_ID}\"")),
        "Upstream codex.ts client_id diverged from CODEX_CLIENT_ID = {CODEX_CLIENT_ID}"
    );

    // 2. Verify token and authorization endpoints
    assert_eq!(CODEX_VERIFICATION_URI, "https://auth.openai.com/codex/device");
    assert!(
        auth_src.contains(CODEX_TOKEN_URL),
        "Upstream codex.ts tokenEndpoint diverged from {CODEX_TOKEN_URL}"
    );
    assert!(
        auth_src.contains("https://auth.openai.com/oauth/authorize"),
        "Upstream codex.ts authorizationEndpoint diverged"
    );

    // 3. Verify scopes
    for &scope in CODEX_SCOPES {
        assert!(
            auth_src.contains(scope),
            "Upstream codex.ts missing scope '{scope}'"
        );
    }

    // 4. Verify refresh buffer: 5 * 60 * 1000 ms (300s)
    assert!(
        auth_src.contains("refreshBufferMs: 5 * 60 * 1000"),
        "Upstream codex.ts refresh buffer must be 5 minutes"
    );
    assert_eq!(
        refresh_lead_seconds("codex"),
        300,
        "OpenProxy refresh lead time for codex must be 300 seconds"
    );

    // 5. Verify CodexSpoofer output
    let headers = CodexSpoofer.headers();
    let find_hdr = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find_hdr("origin"), Some("https://chatgpt.com"));
    assert_eq!(find_hdr("originator"), Some("codex_cli_rs"));
    assert_eq!(find_hdr("version"), Some(current_codex_version().as_str()));
    assert_eq!(find_hdr("user-agent"), Some(current_codex_ua().as_str()));
}

// ============================================================================
// 2. Remote Live Upstream Code Drift Detection (via GitHub raw without cloning)
// ============================================================================

#[tokio::test]
async fn test_cline_remote_upstream_repo_code_drift_detection() {
    let client = UpstreamClient::new();
    let cancel = CancellationToken::new();

    // 1. Probe upstream cline.ts on GitHub
    let auth_url = "https://raw.githubusercontent.com/cline/cline/main/sdk/packages/core/src/auth/cline.ts";
    let req = UpstreamRequest::get(auth_url);
    let resp = match client.call(req, TimeoutProfile::OAuth, cancel.clone()).await {
        Ok(r) if r.status.is_success() => r,
        Ok(r) => {
            eprintln!("[ClineCodeDrift] Upstream GitHub probe HTTP {}, skipping live check", r.status);
            return;
        }
        Err(e) => {
            eprintln!("[ClineCodeDrift] Offline or GitHub unreachable ({e}), skipping live check");
            return;
        }
    };

    let body_bytes = resp.collect().await.expect("read cline.ts body");
    let cline_ts = String::from_utf8_lossy(&body_bytes);

    // Assert endpoints match canonical OpenProxy constants
    assert!(
        cline_ts.contains(&format!("authorize: \"{CLINE_AUTH_AUTHORIZE_PATH}\"")),
        "Upstream Cline diverged in authorize endpoint"
    );
    assert!(
        cline_ts.contains(&format!("token: \"{CLINE_AUTH_TOKEN_PATH}\"")),
        "Upstream Cline diverged in token endpoint"
    );
    assert!(
        cline_ts.contains(&format!("refresh: \"{CLINE_AUTH_REFRESH_PATH}\"")),
        "Upstream Cline diverged in refresh endpoint"
    );
    assert!(
        cline_ts.contains("DEFAULT_REFRESH_BUFFER_MS = 5 * 60 * 1000"),
        "Upstream Cline diverged in refresh lead time"
    );

    // 2. Probe upstream EnvUtils.ts on GitHub for new X- headers
    let env_url = "https://raw.githubusercontent.com/cline/cline/main/apps/vscode/src/services/EnvUtils.ts";
    let env_req = UpstreamRequest::get(env_url);
    if let Ok(env_resp) = client.call(env_req, TimeoutProfile::OAuth, cancel).await
        && env_resp.status.is_success()
        && let Ok(env_bytes) = env_resp.collect().await
    {
        let env_ts = String::from_utf8_lossy(&env_bytes);
        let re = regex::Regex::new(r#""(X-[A-Z-]+)""#).expect("regex");
        for cap in re.captures_iter(&env_ts) {
            let header = &cap[1];
            assert!(
                CLINE_SPOOFING_HEADERS.iter().any(|(k, _)| k.eq_ignore_ascii_case(header)),
                "Upstream Cline added new client header '{header}'! Update OpenProxy to support it."
            );
        }
    }
}

#[tokio::test]
async fn test_codex_remote_upstream_repo_code_drift_detection() {
    let client = UpstreamClient::new();
    let cancel = CancellationToken::new();

    // Probe upstream codex.ts on GitHub
    let codex_url = "https://raw.githubusercontent.com/cline/cline/main/sdk/packages/core/src/auth/codex.ts";
    let req = UpstreamRequest::get(codex_url);
    let resp = match client.call(req, TimeoutProfile::OAuth, cancel).await {
        Ok(r) if r.status.is_success() => r,
        Ok(r) => {
            eprintln!("[CodexCodeDrift] Upstream GitHub probe HTTP {}, skipping live check", r.status);
            return;
        }
        Err(e) => {
            eprintln!("[CodexCodeDrift] Offline or GitHub unreachable ({e}), skipping live check");
            return;
        }
    };

    let body_bytes = resp.collect().await.expect("read codex.ts body");
    let codex_ts = String::from_utf8_lossy(&body_bytes);

    // Assert client ID and token endpoint match OpenProxy constants
    assert!(
        codex_ts.contains(&format!("clientId: \"{CODEX_CLIENT_ID}\"")),
        "Upstream codex.ts clientId diverged from CODEX_CLIENT_ID = {CODEX_CLIENT_ID}"
    );
    assert!(
        codex_ts.contains(&format!("tokenEndpoint: \"{CODEX_TOKEN_URL}\"")),
        "Upstream codex.ts tokenEndpoint diverged from {CODEX_TOKEN_URL}"
    );
    assert!(
        codex_ts.contains("refreshBufferMs: 5 * 60 * 1000"),
        "Upstream codex.ts refresh lead time diverged from 300s"
    );
}

#[tokio::test]
async fn test_kilocode_remote_upstream_repo_code_drift_detection() {
    let client = UpstreamClient::new();
    let cancel = CancellationToken::new();

    // Probe upstream Kilocode VSCode extension package.json on GitHub
    let pkg_url = "https://raw.githubusercontent.com/Kilo-Org/kilocode/main/packages/kilo-vscode/package.json";
    let req = UpstreamRequest::get(pkg_url);
    let resp = match client.call(req, TimeoutProfile::OAuth, cancel).await {
        Ok(r) if r.status.is_success() => r,
        Ok(r) => {
            eprintln!("[KilocodeCodeDrift] Upstream GitHub probe HTTP {}, skipping live check", r.status);
            return;
        }
        Err(e) => {
            eprintln!("[KilocodeCodeDrift] Offline or GitHub unreachable ({e}), skipping live check");
            return;
        }
    };

    let body_bytes = resp.collect().await.expect("read kilo package.json body");
    let pkg_json: serde_json::Value = serde_json::from_slice(&body_bytes).expect("parse json");

    assert_eq!(pkg_json["name"], "kilo-code");
    assert!(
        pkg_json["displayName"]
            .as_str()
            .unwrap_or_default()
            .contains("Kilo Code"),
        "Upstream displayName must contain 'Kilo Code'"
    );

    // Verify KilocodeSpoofer outputs expected headers matching upstream
    let headers = KilocodeSpoofer.headers();
    let find_hdr = |name: &str| {
        headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    };

    assert_eq!(find_hdr("http-referer"), Some("https://kilocode.ai"));
    assert_eq!(find_hdr("x-title"), Some("Kilo Code"));
    assert_eq!(find_hdr("user-agent"), Some(current_kilocode_ua().as_str()));
    assert_eq!(find_hdr("x-kilocode-version"), Some(current_kilocode_version().as_str()));
    assert_eq!(find_hdr("x-client-version"), Some(current_kilocode_version().as_str()));
    assert_eq!(find_hdr("x-client-type"), Some("VSCode Extension"));
}
