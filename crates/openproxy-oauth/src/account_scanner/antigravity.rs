//! Antigravity CLI and OAuth credentials scanner.

use super::{DiscoveredAccount, home_dir};

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
        oauth_provider_specific: None,
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
        oauth_provider_specific: None,
    })
}
