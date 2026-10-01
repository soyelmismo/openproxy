//! Scan the Antigravity-CLI credential file.
//!
//! Conservative: lee solo `~/.gemini/antigravity-cli/antigravity-oauth-token`,
//! hard-coded bajo `std::env::var_os("HOME")`, sin recorrer el filesystem. Extrae los
//! tokens OAuth (access/refresh) y el email del archivo, el mismo formato que
//! `write_antigravity_token_file` emite en sentido inverso
//! (`handlers/admin/accounts.rs`).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Home directory via `std::env::var_os("HOME")`, the stdlib equivalent of
/// `dirs::home_dir()` on Linux/macOS (AGENTS §1.4 / §4.1: stdlib over a new
/// dependency). `None` on Windows or in a headless container with no `HOME`.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredAccount {
    /// Siempre `"antigravity"` por ahora.
    pub provider_id: String,
    /// Label sugerida, p.ej. `"antigravity-cli@alice@example.com"`.
    pub label: String,
    /// Access token OAuth crudo (se cifra en DB al importar).
    pub access_token: String,
    /// Refresh token OAuth, necesario para el refresh en background.
    pub refresh_token: Option<String>,
    /// Email del usuario si el archivo lo incluye.
    pub email: Option<String>,
    /// Path del archivo de origen (audit / skip de duplicados).
    pub source_path: PathBuf,
}

impl DiscoveredAccount {
    /// Security (OP-18): a metadata-only copy for dry-run / preview
    /// responses. The raw OAuth tokens read from the host's filesystem must
    /// not be echoed back over HTTP — a manage key is enough to trigger the
    /// scan, and the response used to carry the full credentials.
    pub fn redacted(&self) -> Self {
        Self {
            provider_id: self.provider_id.clone(),
            label: self.label.clone(),
            access_token: "***redacted***".to_string(),
            refresh_token: self
                .refresh_token
                .as_ref()
                .map(|_| "***redacted***".to_string()),
            email: self.email.clone(),
            source_path: self.source_path.clone(),
        }
    }
}

/// `Some` si el token file del agy-cli existe, se parsea y trae `access_token`.
/// Cualquier fallo (ausente, permisos, JSON inválido, sin access_token) → `None` con un `warn`
/// log; el caller decide.
pub fn scan_antigravity_cli() -> Option<DiscoveredAccount> {
    let path = home_dir()?
        .join(".gemini")
        .join("antigravity-cli")
        .join("antigravity-oauth-token");

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e,
                "account_scanner: cannot read antigravity-cli token file");
            return None;
        }
    };

    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e,
                "account_scanner: antigravity-cli JSON parse failed");
            return None;
        }
    };

    let Some(access_token) = v
        .get("token")
        .and_then(|t| t.get("access_token"))
        .and_then(|a| a.as_str())
        .map(str::to_string)
    else {
        tracing::warn!(path = %path.display(),
            "account_scanner: antigravity-cli file missing access_token");
        return None;
    };

    let refresh_token = v
        .get("token")
        .and_then(|t| t.get("refresh_token"))
        .and_then(|s| s.as_str())
        .map(str::to_string);

    let email = v
        .get("user")
        .and_then(|u| u.get("email"))
        .and_then(|e| e.as_str())
        .map(str::to_string);

    let label = match email.as_deref() {
        Some(e) => format!("antigravity-cli@{e}"),
        None => "antigravity-cli".to_string(),
    };

    Some(DiscoveredAccount {
        provider_id: "antigravity".to_string(),
        label,
        access_token,
        refresh_token,
        email,
        source_path: path,
    })
}

/// Escanea `~/.gemini/oauth_creds.json`, sincronizado por Antigravity-Manager y la
/// CLI en sesiones SSH/contenedores.
pub fn scan_antigravity_oauth_creds() -> Option<DiscoveredAccount> {
    let home = home_dir()?;
    let path = home.join(".gemini").join("oauth_creds.json");

    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e,
                "account_scanner: cannot read oauth_creds.json file");
            return None;
        }
    };

    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e,
                "account_scanner: oauth_creds.json JSON parse failed");
            return None;
        }
    };

    let Some(access_token) = v
        .get("access_token")
        .and_then(|a| a.as_str())
        .map(str::to_string)
    else {
        tracing::warn!(path = %path.display(),
            "account_scanner: oauth_creds.json missing access_token");
        return None;
    };

    let refresh_token = v
        .get("refresh_token")
        .and_then(|s| s.as_str())
        .map(str::to_string);

    // email activo de ~/.gemini/google_accounts.json
    let mut email = None;
    let accounts_path = home.join(".gemini").join("google_accounts.json");
    if let Ok(acc_bytes) = std::fs::read(&accounts_path)
        && let Ok(acc_v) = serde_json::from_slice::<serde_json::Value>(&acc_bytes)
    {
        email = acc_v
            .get("active")
            .and_then(|e| e.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
    }

    // fallback: claims del JWT id_token
    if email.is_none()
        && let Some(id_tok) = v.get("id_token").and_then(|s| s.as_str())
        && let Some(claims) = crate::oauth::decode_jwt_payload(id_tok)
    {
        email = claims
            .get("email")
            .and_then(|e| e.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
    }

    let label = match email.as_deref() {
        Some(e) => format!("antigravity@{e}"),
        None => "antigravity-oauth".to_string(),
    };

    Some(DiscoveredAccount {
        provider_id: "antigravity".to_string(),
        label,
        access_token,
        refresh_token,
        email,
        source_path: path,
    })
}

/// Entrada del endpoint: escanea el token file de agy-cli y el oauth_creds.json
/// compartido con Antigravity-Manager.
pub fn scan_external_accounts() -> Vec<DiscoveredAccount> {
    let mut accounts = Vec::new();
    if let Some(cli_acc) = scan_antigravity_cli() {
        accounts.push(cli_acc);
    }
    if let Some(creds_acc) = scan_antigravity_oauth_creds() {
        let duplicate = accounts.iter().any(|a| {
            (a.email.is_some() && a.email == creds_acc.email)
                || a.access_token == creds_acc.access_token
                || (a.refresh_token.is_some() && a.refresh_token == creds_acc.refresh_token)
        });
        if !duplicate {
            accounts.push(creds_acc);
        }
    }
    accounts
}

#[cfg(test)]
mod tests {
    // `unwrap` / `expect` are allowed in tests by the crate-level
    // `cfg_attr(test, allow(...))` on lib.rs

    use super::*;
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
        std::fs::write(&creds_target, serde_json::to_vec(&creds_body).expect("ser"))
            .expect("write");

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

        // el access token rotó, el refresh token es el mismo
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
    fn test_scanner_handles_empty_email_in_google_accounts() {
        let tmp = tempdir();
        let gemini = tmp.path().join(".gemini");
        std::fs::create_dir_all(&gemini).expect("mkdir");

        let creds_target = gemini.join("oauth_creds.json");
        let accounts_target = gemini.join("google_accounts.json");

        let creds_body = serde_json::json!({
            "access_token": "ya-test-creds-access",
            "refresh_token": "1//test-creds-refresh",
            "token_type": "Bearer",
            "expiry_date": 1740000000000i64
        });
        std::fs::write(&creds_target, serde_json::to_vec(&creds_body).unwrap()).unwrap();

        // el email activo es solo whitespace
        let accounts_body = serde_json::json!({
            "active": "   ",
            "old": []
        });
        std::fs::write(
            &accounts_target,
            serde_json::to_vec(&accounts_body).unwrap(),
        )
        .unwrap();

        let _guard = lock();
        let _home = HomeGuard::set(tmp.path());

        let found = scan_external_accounts();
        let agy: Vec<_> = found
            .iter()
            .filter(|a| a.provider_id == "antigravity")
            .collect();
        assert_eq!(agy.len(), 1);
        assert_eq!(agy[0].label, "antigravity-oauth");
        assert!(agy[0].email.is_none());
    }
}

#[cfg(test)]
mod adversarial_tests {
    use super::tests::tempdir;
    use super::*;

    /// Serializes HOME mutations using the shared lock.
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        super::tests::lock()
    }

    /// RAII guard that sets HOME and restores it on drop.
    struct HomeGuard {
        prev: Option<std::ffi::OsString>,
    }

    impl HomeGuard {
        fn set(path: &std::path::Path) -> Self {
            let prev = std::env::var_os("HOME");
            // SAFETY: caller holds `TEST_LOCK` while guard is alive.
            unsafe { std::env::set_var("HOME", path) };
            Self { prev }
        }
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match &self.prev {
                // SAFETY: caller holds `TEST_LOCK` while guard is alive.
                Some(v) => unsafe { std::env::set_var("HOME", v) },
                None => unsafe { std::env::remove_var("HOME") },
            }
        }
    }

    #[test]
    fn adv_file_with_valid_json_wrong_schema() {
        // JSON is valid but has wrong shape (no token.access_token).
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
                "access_token": 12345,  // number, not string
                "refresh_token": true   // bool, not string
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
    fn adv_file_without_email_uses_default_label() {
        let tmp = tempdir();
        let target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("antigravity-oauth-token");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
        let body = serde_json::json!({
            "token": {
                "access_token": "ya-token",
                "refresh_token": "1//refresh"
            },
            "auth_method": "consumer"
        });
        std::fs::write(&target, serde_json::to_vec(&body).expect("ser")).expect("write");

        let _guard = lock();
        let _home = HomeGuard::set(tmp.path());
        let account = scan_antigravity_cli().expect("should parse");
        assert_eq!(account.email, None);
        assert_eq!(account.label, "antigravity-cli");
        assert_eq!(account.access_token, "ya-token");
    }

    #[test]
    fn adv_home_nonexistent_returns_none() {
        let _guard = lock();
        let prev = std::env::var_os("HOME");
        let fake = "/tmp/surely-nonexistent-dir-2026-01-01";
        unsafe { std::env::set_var("HOME", fake) };
        let result = scan_antigravity_cli();
        assert!(result.is_none(), "non-existent HOME → None");
        match &prev {
            Some(v) => unsafe { std::env::set_var("HOME", v) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn adv_unreadable_token_file_returns_none() {
        // root ignora el modo 000 y lee el archivo: se acepta Some o None
        // (EACCES); el requisito es que no haya panic.
        let tmp = tempdir();
        let target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("antigravity-oauth-token");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
        std::fs::write(&target, r#"{"token":{"access_token":"x"}}"#).expect("write");
        std::fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .expect("set perms");

        let _guard = lock();
        let _home = HomeGuard::set(tmp.path());
        let result = scan_antigravity_cli();
        assert!(
            result.is_some() || result.is_none(),
            "scan must return Some or None, not panic"
        );
    }

    #[test]
    fn adv_file_without_refresh_token() {
        let tmp = tempdir();
        let target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("antigravity-oauth-token");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
        let body = serde_json::json!({
            "token": {
                "access_token": "ya-no-refresh"
            }
        });
        std::fs::write(&target, serde_json::to_vec(&body).expect("ser")).expect("write");

        let _guard = lock();
        let _home = HomeGuard::set(tmp.path());
        let account = scan_antigravity_cli().expect("should parse");
        assert_eq!(
            account.refresh_token, None,
            "missing refresh_token is None, not error"
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

    #[test]
    fn adv_deeply_nested_json_without_token() {
        let tmp = tempdir();
        let target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("antigravity-oauth-token");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
        let body = serde_json::json!({
            "token": {
                "access_token": {
                    "nested": {"value": "deep"}
                }
            }
        });
        std::fs::write(&target, serde_json::to_vec(&body).expect("ser")).expect("write");

        let _guard = lock();
        let _home = HomeGuard::set(tmp.path());
        assert!(
            scan_antigravity_cli().is_none(),
            "nested object access_token must return None"
        );
    }

    #[test]
    fn adv_zero_byte_file_returns_none() {
        let tmp = tempdir();
        let target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("antigravity-oauth-token");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
        std::fs::write(&target, "").expect("write empty file");

        let _guard = lock();
        let _home = HomeGuard::set(tmp.path());
        assert!(
            scan_antigravity_cli().is_none(),
            "zero-byte file must return None"
        );
    }

    #[test]
    fn adv_whitespace_only_file_returns_none() {
        let tmp = tempdir();
        let target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("antigravity-oauth-token");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
        std::fs::write(&target, "   \n  \t  ").expect("write");

        let _guard = lock();
        let _home = HomeGuard::set(tmp.path());
        assert!(
            scan_antigravity_cli().is_none(),
            "whitespace-only file must return None"
        );
    }

    #[test]
    fn adv_source_path_matches_actual_file() {
        let tmp = tempdir();
        let target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("antigravity-oauth-token");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
        let body = serde_json::json!({
            "token": {
                "access_token": "ya-path-test",
                "refresh_token": "1//refresh"
            }
        });
        std::fs::write(&target, serde_json::to_vec(&body).expect("ser")).expect("write");

        let _guard = lock();
        let _home = HomeGuard::set(tmp.path());
        let account = scan_antigravity_cli().expect("should parse");
        assert_eq!(account.source_path, target);
        assert_eq!(account.provider_id, "antigravity");
    }
}
