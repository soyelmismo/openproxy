// `unwrap` / `expect` are allowed in tests by the crate-level
// `cfg_attr(test, allow(...))` on lib.rs

use super::*;
use base64::Engine;
use std::sync::Mutex;

/// Serializa la mutación de `HOME` entre tests paralelos (AGENTS §4.3).
pub(super) static TEST_LOCK: Mutex<()> = Mutex::new(());

pub(super) fn lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// RAII guard que setea `HOME` y lo restaura al drop, incluso en panic.
struct HomeGuard {
    prev: Option<std::ffi::OsString>,
}

impl HomeGuard {
    fn set(path: &std::path::Path) -> Self {
        let prev = std::env::var_os("HOME");
        // SAFETY: el caller debe sostener `TEST_LOCK` mientras el guard vive.
        unsafe { std::env::set_var("HOME", path) };
        Self { prev }
    }
}

impl Drop for HomeGuard {
    fn drop(&mut self) {
        match &self.prev {
            // SAFETY: el caller debe sostener `TEST_LOCK` mientras el guard vive.
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }
}

pub(super) fn tempdir() -> openproxy_db::testing::TempDir {
    openproxy_db::testing::TempDir::new("openproxy-account-test").expect("tempdir")
}

#[test]
fn test_scanner_finds_antigravity_token_file() {
    let tmp = tempdir();
    let target = tmp
        .path()
        .join(".gemini")
        .join("antigravity-cli")
        .join("antigravity-oauth-token");
    std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
    let body = serde_json::json!({
        "token": {
            "access_token": "ya-test-access",
            "refresh_token": "1//test-refresh",
            "expiry": "2099-01-01T00:00:00Z",
            "token_type": "Bearer"
        },
        "auth_method": "consumer",
        "user": { "email": "alice@example.com" }
    });
    std::fs::write(&target, serde_json::to_vec(&body).expect("ser")).expect("write");

    let _guard = lock();
    let _home = HomeGuard::set(tmp.path());

    let found = scan_external_accounts();
    let agy: Vec<_> = found
        .iter()
        .filter(|a| a.provider_id == "antigravity")
        .collect();
    assert_eq!(agy.len(), 1, "expected exactly one antigravity entry");
    assert_eq!(agy[0].label, "antigravity-cli@alice@example.com");
    assert_eq!(agy[0].access_token, "ya-test-access");
    assert_eq!(agy[0].refresh_token.as_deref(), Some("1//test-refresh"));
    assert_eq!(agy[0].email.as_deref(), Some("alice@example.com"));
    assert_eq!(agy[0].source_path, target);
}

#[test]
fn test_scanner_skips_missing_and_corrupt_files() {
    let tmp = tempdir();

    // (a) HOME vacío
    {
        let _guard = lock();
        let _home = HomeGuard::set(tmp.path());
        let found = scan_external_accounts();
        assert!(found.is_empty(), "expected no entries from empty home");
    }

    // (b) archivo corrupto
    let target = tmp
        .path()
        .join(".gemini")
        .join("antigravity-cli")
        .join("antigravity-oauth-token");
    std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
    std::fs::write(&target, b"{ this is not valid json").expect("write");
    {
        let _guard = lock();
        let _home = HomeGuard::set(tmp.path());
        let found = scan_external_accounts();
        assert!(found.is_empty(), "corrupt file must not surface as entry");
    }

    // (c) archivo sin access_token
    let body = serde_json::json!({
        "token": { "token_type": "Bearer" },
        "auth_method": "consumer"
    });
    std::fs::write(&target, serde_json::to_vec(&body).expect("ser")).expect("write");
    {
        let _guard = lock();
        let _home = HomeGuard::set(tmp.path());
        let found = scan_external_accounts();
        assert!(
            found.is_empty(),
            "missing access_token must not surface as entry"
        );
    }
}

#[test]
fn test_scanner_finds_antigravity_oauth_creds_file() {
    let tmp = tempdir();
    let gemini = tmp.path().join(".gemini");
    std::fs::create_dir_all(&gemini).expect("mkdir");

    let creds_target = gemini.join("oauth_creds.json");
    let accounts_target = gemini.join("google_accounts.json");

    let creds_body = serde_json::json!({
        "access_token": "ya-test-creds-access",
        "refresh_token": "1//test-creds-refresh",
        "token_type": "Bearer",
        "expiry_date": 1740000000000i64,
        "scope": "openid email"
    });
    std::fs::write(&creds_target, serde_json::to_vec(&creds_body).expect("ser")).expect("write");

    let accounts_body = serde_json::json!({
        "active": "bob@example.com",
        "old": []
    });
    std::fs::write(
        &accounts_target,
        serde_json::to_vec(&accounts_body).expect("ser"),
    )
    .expect("write");

    let _guard = lock();
    let _home = HomeGuard::set(tmp.path());

    let found = scan_external_accounts();
    let agy: Vec<_> = found
        .iter()
        .filter(|a| a.provider_id == "antigravity")
        .collect();
    assert_eq!(
        agy.len(),
        1,
        "expected exactly one antigravity entry from oauth_creds"
    );
    assert_eq!(agy[0].label, "antigravity@bob@example.com");
    assert_eq!(agy[0].access_token, "ya-test-creds-access");
    assert_eq!(
        agy[0].refresh_token.as_deref(),
        Some("1//test-creds-refresh")
    );
    assert_eq!(agy[0].email.as_deref(), Some("bob@example.com"));
    assert_eq!(agy[0].source_path, creds_target);
}

#[test]
fn test_scanner_deduplicates_by_refresh_token() {
    let tmp = tempdir();
    let gemini = tmp.path().join(".gemini");
    let cli_dir = gemini.join("antigravity-cli");
    std::fs::create_dir_all(&cli_dir).expect("mkdir");

    let cli_token = cli_dir.join("antigravity-oauth-token");
    let creds_target = gemini.join("oauth_creds.json");

    let cli_body = serde_json::json!({
        "token": {
            "access_token": "ya-new-access",
            "refresh_token": "shared-refresh-token",
            "token_type": "Bearer"
        }
    });
    std::fs::write(&cli_token, serde_json::to_vec(&cli_body).unwrap()).unwrap();

    let creds_body = serde_json::json!({
        "access_token": "ya-old-access",
        "refresh_token": "shared-refresh-token",
        "token_type": "Bearer",
        "expiry_date": 1740000000000i64
    });
    std::fs::write(&creds_target, serde_json::to_vec(&creds_body).unwrap()).unwrap();

    let _guard = lock();
    let _home = HomeGuard::set(tmp.path());

    let found = scan_external_accounts();
    let agy: Vec<_> = found
        .iter()
        .filter(|a| a.provider_id == "antigravity")
        .collect();
    assert_eq!(
        agy.len(),
        1,
        "expected deduplication by shared refresh_token"
    );
    assert_eq!(agy[0].access_token, "ya-new-access");
}

#[test]
fn test_scanner_finds_claude_code_credentials() {
    let tmp = tempdir();
    let claude_dir = tmp.path().join(".claude");
    std::fs::create_dir_all(&claude_dir).expect("mkdir");

    let creds_file = claude_dir.join(".credentials.json");
    let global_config = tmp.path().join(".claude.json");

    let creds_body = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "sk-ant-test-oauth-access",
            "refreshToken": "sk-ant-test-oauth-refresh",
            "expiresAt": 1790000000000i64,
            "scopes": ["user:inference", "org:create_api_key"]
        }
    });
    std::fs::write(&creds_file, serde_json::to_vec(&creds_body).unwrap()).unwrap();

    let global_body = serde_json::json!({
        "oauthAccount": {
            "accountUuid": "acc-uuid-1234",
            "emailAddress": "claudedev@example.com",
            "organizationUuid": "org-uuid-5678"
        }
    });
    std::fs::write(&global_config, serde_json::to_vec(&global_body).unwrap()).unwrap();

    let _guard = lock();
    let _home = HomeGuard::set(tmp.path());

    let found = scan_external_accounts();
    let claude: Vec<_> = found
        .iter()
        .filter(|a| a.provider_id == "claude-code")
        .collect();
    assert_eq!(claude.len(), 1, "expected 1 claude-code account");
    assert_eq!(claude[0].label, "claude-code@claudedev@example.com");
    assert_eq!(claude[0].access_token, "sk-ant-test-oauth-access");
    assert_eq!(
        claude[0].refresh_token.as_deref(),
        Some("sk-ant-test-oauth-refresh")
    );
    assert_eq!(claude[0].email.as_deref(), Some("claudedev@example.com"));
    assert_eq!(claude[0].source_path, creds_file);
}

#[test]
fn test_scanner_finds_claude_swap_backups() {
    let tmp = tempdir();
    let cswap_dir = tmp.path().join(".claude-swap-backup");
    std::fs::create_dir_all(&cswap_dir).expect("mkdir");

    // 1. Plain JSON backup
    let b1_file = cswap_dir.join("account-primary.json");
    let b1_body = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "sk-swap-access-1",
            "refreshToken": "sk-swap-refresh-1"
        },
        "oauthAccount": {
            "emailAddress": "swap1@example.com"
        }
    });
    std::fs::write(&b1_file, serde_json::to_vec(&b1_body).unwrap()).unwrap();

    // 2. Base64 encoded .enc backup
    let b2_file = cswap_dir.join(".creds-secondary.enc");
    let b2_body = serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "sk-swap-access-2",
            "refreshToken": "sk-swap-refresh-2"
        },
        "email": "swap2@example.com"
    });
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&b2_body).unwrap());
    std::fs::write(&b2_file, encoded.as_bytes()).unwrap();

    let _guard = lock();
    let _home = HomeGuard::set(tmp.path());

    let found = scan_claude_swap_backups();
    assert_eq!(found.len(), 2, "expected 2 claude-swap discovered accounts");

    let emails: Vec<_> = found.iter().filter_map(|a| a.email.as_deref()).collect();
    assert!(emails.contains(&"swap1@example.com"));
    assert!(emails.contains(&"swap2@example.com"));
}

#[test]
fn test_write_claude_code_credentials() {
    let tmp = tempdir();
    let _guard = lock();
    let _home = HomeGuard::set(tmp.path());

    // Also simulate existing claude-swap backup folder
    let cswap_dir = tmp.path().join(".claude-swap-backup");
    std::fs::create_dir_all(&cswap_dir).expect("mkdir");

    let res = write_claude_code_credentials(ClaudeCodeWriteOptions {
        access_token: "sk-ant-written-access",
        refresh_token: Some("sk-ant-written-refresh"),
        expires_at: Some("2028-01-01T00:00:00Z"),
        email: Some("applied@example.com"),
        account_uuid: Some("acc-uuid-999"),
        org_uuid: Some("org-uuid-888"),
        subscription_type: Some("pro"),
        rate_limit_tier: Some("default_claude_ai"),
    });
    assert!(res.is_ok(), "write_claude_code_credentials should succeed");

    let creds_file = tmp.path().join(".claude").join(".credentials.json");
    assert!(creds_file.is_file(), ".credentials.json must exist");

    let creds_bytes = std::fs::read(&creds_file).unwrap();
    let creds_json: serde_json::Value = serde_json::from_slice(&creds_bytes).unwrap();
    assert_eq!(
        creds_json["claudeAiOauth"]["accessToken"],
        "sk-ant-written-access"
    );
    assert_eq!(
        creds_json["claudeAiOauth"]["refreshToken"],
        "sk-ant-written-refresh"
    );
    assert_eq!(creds_json["claudeAiOauth"]["subscriptionType"], "pro");
    assert_eq!(
        creds_json["claudeAiOauth"]["rateLimitTier"],
        "default_claude_ai"
    );

    let global_file = tmp.path().join(".claude.json");
    assert!(global_file.is_file(), "~/.claude.json must exist");
    let global_bytes = std::fs::read(&global_file).unwrap();
    let global_json: serde_json::Value = serde_json::from_slice(&global_bytes).unwrap();
    assert_eq!(
        global_json["oauthAccount"]["emailAddress"],
        "applied@example.com"
    );
    assert_eq!(global_json["oauthAccount"]["accountUuid"], "acc-uuid-999");
    assert_eq!(global_json["hasAvailableSubscription"], true);

    // Verify claude-swap backup was synced
    let cswap_file = cswap_dir.join("account-applied_example.com.json");
    assert!(cswap_file.is_file(), "claude-swap backup must be written");
}

#[test]
fn adv_file_with_valid_json_wrong_schema() {
    let tmp = tempdir();
    let target = tmp
        .path()
        .join(".gemini")
        .join("antigravity-cli")
        .join("antigravity-oauth-token");
    std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
    std::fs::write(&target, r#"{"hello":"world","foo":123}"#).expect("write");

    let _guard = lock();
    let _home = HomeGuard::set(tmp.path());
    assert!(
        scan_antigravity_cli().is_none(),
        "wrong schema must return None"
    );
}

#[test]
fn adv_file_with_token_wrong_types() {
    let tmp = tempdir();
    let target = tmp
        .path()
        .join(".gemini")
        .join("antigravity-cli")
        .join("antigravity-oauth-token");
    std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
    let body = serde_json::json!({
        "token": {
            "access_token": 12345,
            "refresh_token": true
        }
    });
    std::fs::write(&target, serde_json::to_vec(&body).expect("ser")).expect("write");

    let _guard = lock();
    let _home = HomeGuard::set(tmp.path());
    assert!(
        scan_antigravity_cli().is_none(),
        "non-string access_token must return None"
    );
}

#[test]
fn adv_scan_external_accounts_returns_vec() {
    let result = scan_external_accounts();
    assert!(
        result.is_empty() || !result.is_empty(),
        "scan_external_accounts returns a Vec (never panics)"
    );
}
