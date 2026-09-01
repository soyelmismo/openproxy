//! Scan the Antigravity-CLI credential file.
//!
//! Conservative: solo lee `~/.gemini/antigravity-cli/antigravity-oauth-token`
//! (path hard-coded bajo `std::env::var_os("HOME")`). No camina el filesystem
//! recursivamente. El parser extrae los tokens OAuth (access/refresh) y el
//! email del archivo — el mismo formato que `write_antigravity_token_file`
//! emite en sentido inverso (`handlers/admin/accounts.rs`).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Resolve the user's home directory without dragging in a new dep.
///
/// `std::env::var_os("HOME")` is the stdlib equivalent of `dirs::home_dir()`
/// on Linux/macOS (AGENTS §1.4 / §4.1: stdlib > new dependency). Returns
/// `None` on Windows / headless containers with no `HOME` set.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredAccount {
    /// Siempre `"antigravity"` en esta iteración.
    pub provider_id: String,
    /// Label sugerida, p.ej. `"antigravity-cli@alice@example.com"`.
    pub label: String,
    /// Access token OAuth crudo (se cifra en DB al importar).
    pub access_token: String,
    /// Refresh token OAuth (necesario para el refresh en background).
    pub refresh_token: Option<String>,
    /// Email del usuario si el archivo lo incluye.
    pub email: Option<String>,
    /// Path del archivo que produjo esta entry (audit / skip duplicados).
    pub source_path: PathBuf,
}

/// Scan el token file del agy-cli. Devuelve `Some` si el archivo existe,
/// es parseable y tiene un `access_token`. Cualquier fallo (missing,
/// permisos, JSON inválido, sin access_token) → `None` con un `warn`
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

/// Punto de entrada del endpoint. Hoy solo escanea el agy-cli; el follow-up
/// multi-provider iterará sobre un registry (ver docs/specs/antigravity-gaps-p3.md
/// §Out of scope).
pub fn scan_external_accounts() -> Vec<DiscoveredAccount> {
    scan_antigravity_cli().into_iter().collect()
}

#[cfg(test)]
mod tests {
    // `unwrap()` / `expect()` are allowed in tests by the crate-level
    // `#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]`
    // on `lib.rs` — no per-module override needed.

    use super::*;
    use std::sync::Mutex;

    /// Serializa la mutación de `HOME` entre tests paralelos
    /// (AGENTS §4.3 + P3-4 de la spec).
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// RAII guard que setea `HOME` y lo restaura al drop (incluso en panic).
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

    #[test]
    fn test_scanner_finds_antigravity_token_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
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

        let _guard = TEST_LOCK.lock().expect("test lock poisoned");
        let _home = HomeGuard::set(tmp.path());

        let found = scan_external_accounts();
        let agy: Vec<_> = found.iter().filter(|a| a.provider_id == "antigravity").collect();
        assert_eq!(agy.len(), 1, "expected exactly one antigravity entry");
        assert_eq!(agy[0].label, "antigravity-cli@alice@example.com");
        assert_eq!(agy[0].access_token, "ya-test-access");
        assert_eq!(agy[0].refresh_token.as_deref(), Some("1//test-refresh"));
        assert_eq!(agy[0].email.as_deref(), Some("alice@example.com"));
        assert_eq!(agy[0].source_path, target);
    }

    #[test]
    fn test_scanner_skips_missing_and_corrupt_files() {
        let tmp = tempfile::tempdir().expect("tempdir");

        // (a) HOME vacío → ningún archivo.
        {
            let _guard = TEST_LOCK.lock().expect("test lock poisoned");
            let _home = HomeGuard::set(tmp.path());
            let found = scan_external_accounts();
            assert!(found.is_empty(), "expected no entries from empty home");
        }

        // (b) Archivo corrupto → no debe surfear como entry.
        let target = tmp
            .path()
            .join(".gemini")
            .join("antigravity-cli")
            .join("antigravity-oauth-token");
        std::fs::create_dir_all(target.parent().expect("parent")).expect("mkdir");
        std::fs::write(&target, b"{ this is not valid json").expect("write");
        {
            let _guard = TEST_LOCK.lock().expect("test lock poisoned");
            let _home = HomeGuard::set(tmp.path());
            let found = scan_external_accounts();
            assert!(found.is_empty(), "corrupt file must not surface as entry");
        }

        // (c) Archivo sin access_token → no debe surfear como entry.
        let body = serde_json::json!({
            "token": { "token_type": "Bearer" },
            "auth_method": "consumer"
        });
        std::fs::write(&target, serde_json::to_vec(&body).expect("ser")).expect("write");
        {
            let _guard = TEST_LOCK.lock().expect("test lock poisoned");
            let _home = HomeGuard::set(tmp.path());
            let found = scan_external_accounts();
            assert!(
                found.is_empty(),
                "missing access_token must not surface as entry"
            );
        }
    }
}