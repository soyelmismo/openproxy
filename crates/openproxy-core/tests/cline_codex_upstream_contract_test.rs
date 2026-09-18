//! Integration test for 1:1 contract parity between OpenProxy and upstream Cline / Codex / Kilocode.
//!
//! Validates:
//! 1. Golden contract spec parity against local repository source code (`other_projects_examples/cline`).
//! 2. Live remote upstream contract verification against `github.com/cline/cline/main` and
//!    `github.com/Kilo-Org/kilocode/main` via lightweight remote HTTP queries without cloning.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use openproxy_adapters::spoofer::{
    CLINE_SPOOFING_HEADERS, CLINE_TEST_LOCK, CODEX_TEST_LOCK, ClientSpoofer, ClineSpoofer,
    CodexSpoofer, KILOCODE_TEST_LOCK, KilocodeSpoofer, current_cline_ua, current_cline_version,
    current_codex_ua, current_codex_version, current_kilocode_ua, current_kilocode_version,
    reset_dynamic_cline_overrides, reset_dynamic_codex_overrides, reset_dynamic_kilocode_overrides,
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
// 1. Upstream Repository Code Drift Parity (Remote HTTP + Local Fallback)
// ============================================================================

async fn load_upstream_source(http_url: &str, local_rel_path: &str) -> Option<String> {
    let client = UpstreamClient::new();
    let cancel = CancellationToken::new();
    let req = UpstreamRequest::get(http_url);
    if let Ok(resp) = client.call(req, TimeoutProfile::OAuth, cancel).await
        && resp.status.is_success()
        && let Ok(body) = resp.collect().await
    {
        return Some(String::from_utf8_lossy(&body).into_owned());
    }

    let base_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".into());
    let path = Path::new(&base_dir).join(local_rel_path);
    if path.exists() {
        return std::fs::read_to_string(&path).ok();
    }

    None
}

#[tokio::test]
async fn test_cline_upstream_code_parity() {
    let auth_url =
        "https://raw.githubusercontent.com/cline/cline/main/sdk/packages/core/src/auth/cline.ts";
    let local_auth_path = "../../other_projects_examples/cline/sdk/packages/core/src/auth/cline.ts";
    let env_url =
        "https://raw.githubusercontent.com/cline/cline/main/apps/vscode/src/services/EnvUtils.ts";
    let local_env_path = "../../other_projects_examples/cline/apps/vscode/src/services/EnvUtils.ts";

    let Some(auth_src) = load_upstream_source(auth_url, local_auth_path).await else {
        eprintln!(
            "[LocalCodeTest] Skipping cline auth check: unable to fetch from remote URL {auth_url} or local path {local_auth_path}"
        );
        return;
    };
    let env_src_opt = load_upstream_source(env_url, local_env_path).await;

    let _guard = CLINE_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_dynamic_cline_overrides();

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
    if let Some(env_src) = env_src_opt {
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

#[tokio::test]
async fn test_codex_upstream_code_parity() {
    let codex_url =
        "https://raw.githubusercontent.com/cline/cline/main/sdk/packages/core/src/auth/codex.ts";
    let local_codex_path = "../../other_projects_examples/cline/sdk/packages/core/src/auth/codex.ts";

    let Some(auth_src) = load_upstream_source(codex_url, local_codex_path).await else {
        eprintln!(
            "[LocalCodeTest] Skipping codex auth check: unable to fetch from remote URL {codex_url} or local path {local_codex_path}"
        );
        return;
    };

    let _guard = CODEX_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    reset_dynamic_codex_overrides();

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
    let _guard_k = KILOCODE_TEST_LOCK.lock().unwrap();
    reset_dynamic_kilocode_overrides();
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
    drop(_guard_k);
}

#[test]
fn test_cline_codex_kilocode_dynamic_spoofer_overrides() {
    use openproxy_adapters::spoofer::{
        reset_dynamic_cline_overrides, reset_dynamic_codex_overrides,
        reset_dynamic_kilocode_overrides, set_dynamic_cline_extra_header,
        set_dynamic_cline_version, set_dynamic_codex_extra_header,
        set_dynamic_codex_version, set_dynamic_kilocode_extra_header,
        set_dynamic_kilocode_version,
    };

    let _guard_cline = CLINE_TEST_LOCK.lock().unwrap();
    let _guard_codex = CODEX_TEST_LOCK.lock().unwrap();
    let _guard_kilo = KILOCODE_TEST_LOCK.lock().unwrap();

    // 1. Dynamic Cline spoofer
    reset_dynamic_cline_overrides();
    set_dynamic_cline_version("3.8.0");
    set_dynamic_cline_extra_header("user-agent", "Cline-Bot-Runner/3.8.0");
    set_dynamic_cline_extra_header("x-platform", "darwin");
    set_dynamic_cline_extra_header("x-custom-engine", "fast");

    let cline_headers = ClineSpoofer.headers();
    let find_cline = |k: &str| {
        cline_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(find_cline("user-agent"), Some("Cline-Bot-Runner/3.8.0"));
    assert_eq!(find_cline("x-client-version"), Some("3.8.0"));
    assert_eq!(find_cline("x-platform"), Some("darwin"));
    assert_eq!(find_cline("x-custom-engine"), Some("fast"));
    reset_dynamic_cline_overrides();

    // 2. Dynamic Codex spoofer
    reset_dynamic_codex_overrides();
    set_dynamic_codex_version("0.160.0");
    set_dynamic_codex_extra_header("user-agent", "Codex-CLI-Mock/0.160.0");
    set_dynamic_codex_extra_header("chatgpt-account-id", "acc-enterprise-99");

    let codex_headers = CodexSpoofer.headers();
    let find_codex = |k: &str| {
        codex_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(find_codex("user-agent"), Some("Codex-CLI-Mock/0.160.0"));
    assert_eq!(find_codex("version"), Some("0.160.0"));
    assert_eq!(find_codex("chatgpt-account-id"), Some("acc-enterprise-99"));
    reset_dynamic_codex_overrides();

    // 3. Dynamic Kilocode spoofer
    reset_dynamic_kilocode_overrides();
    set_dynamic_kilocode_version("0.18.0");
    set_dynamic_kilocode_extra_header("user-agent", "KiloCode-Editor/0.18.0");
    set_dynamic_kilocode_extra_header("x-kilocode-session", "sess-kilo-123");

    let kilo_headers = KilocodeSpoofer.headers();
    let find_kilo = |k: &str| {
        kilo_headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(k))
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(find_kilo("user-agent"), Some("KiloCode-Editor/0.18.0"));
    assert_eq!(find_kilo("x-kilocode-version"), Some("0.18.0"));
    assert_eq!(find_kilo("x-client-version"), Some("0.18.0"));
    assert_eq!(find_kilo("x-kilocode-session"), Some("sess-kilo-123"));
    reset_dynamic_kilocode_overrides();
}

#[test]
fn test_oauth_dynamic_endpoint_resolution_generic_and_kiro() {
    use openproxy_core::oauth::generic::{GenericOAuthProvider, OAuthRequestEncoding, OAuthSpec};
    use openproxy_core::oauth::kiro::{
        kiro_device_auth_url, kiro_register_url, kiro_social_token_url, kiro_token_url,
    };
    use openproxy_core::oauth::OAuthFlow;

    // 1. GenericOAuthProvider Antigravity resolution
    let spec = OAuthSpec {
        id: "antigravity",
        flow: OAuthFlow::AuthorizationCode,
        authorize_url: Some("https://accounts.google.com/o/oauth2/v2/auth"),
        token_url: "https://oauth2.googleapis.com/token",
        device_authorization_url: None,
        client_id_env: None,
        client_id_default: "test",
        client_secret_env: None,
        client_secret_default: None,
        scopes: &["openid"],
        auth_extra_params: &[],
        request_encoding: OAuthRequestEncoding::FormUrlEncoded,
        user_agent: None,
    };
    let ag = GenericOAuthProvider::new(spec);
    assert_eq!(ag.spec().token_url, "https://oauth2.googleapis.com/token");
    assert_eq!(ag.resolved_token_url(), "https://oauth2.googleapis.com/token");

    // SAFETY: isolated test verification
    unsafe {
        std::env::set_var("OPENPROXY_ANTIGRAVITY_TOKEN_URL", "https://mock-google.local/token");
    }
    assert_eq!(ag.resolved_token_url(), "https://mock-google.local/token");
    unsafe {
        std::env::remove_var("OPENPROXY_ANTIGRAVITY_TOKEN_URL");
    }
    assert_eq!(ag.resolved_token_url(), "https://oauth2.googleapis.com/token");

    // 2. Kiro dynamic URL resolution
    assert_eq!(
        kiro_register_url(None),
        "https://oidc.us-east-1.amazonaws.com/client/register"
    );
    assert_eq!(
        kiro_device_auth_url(None),
        "https://oidc.us-east-1.amazonaws.com/device_authorization"
    );
    assert_eq!(
        kiro_token_url(None),
        "https://oidc.us-east-1.amazonaws.com/token"
    );
    assert_eq!(
        kiro_social_token_url(),
        "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken"
    );

    // Dynamic OIDC base URL and social token URL overrides
    unsafe {
        std::env::set_var("OPENPROXY_KIRO_OIDC_BASE_URL", "https://mock-oidc.local");
        std::env::set_var(
            "OPENPROXY_KIRO_SOCIAL_TOKEN_URL",
            "https://mock-social.local/refresh",
        );
    }

    assert_eq!(
        kiro_register_url(None),
        "https://mock-oidc.local/client/register"
    );
    assert_eq!(
        kiro_device_auth_url(None),
        "https://mock-oidc.local/device_authorization"
    );
    assert_eq!(kiro_token_url(None), "https://mock-oidc.local/token");
    assert_eq!(kiro_social_token_url(), "https://mock-social.local/refresh");

    unsafe {
        std::env::remove_var("OPENPROXY_KIRO_OIDC_BASE_URL");
        std::env::remove_var("OPENPROXY_KIRO_SOCIAL_TOKEN_URL");
    }

    assert_eq!(
        kiro_register_url(None),
        "https://oidc.us-east-1.amazonaws.com/client/register"
    );
    assert_eq!(
        kiro_device_auth_url(None),
        "https://oidc.us-east-1.amazonaws.com/device_authorization"
    );
    assert_eq!(
        kiro_token_url(None),
        "https://oidc.us-east-1.amazonaws.com/token"
    );
}
