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

    let expires_at = v
        .get("token")
        .and_then(|t| t.get("expiry"))
        .and_then(|e| e.as_str())
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
        expires_at,
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

    let expires_at = v
        .get("expiry_date")
        .and_then(|e| e.as_i64())
        .and_then(|ms| {
            chrono::DateTime::from_timestamp_millis(ms)
                .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        });

    Some(DiscoveredAccount {
        provider_id: "antigravity".to_string(),
        label,
        access_token,
        refresh_token,
        email,
        source_path: path,
        oauth_provider_specific: None,
        expires_at,
    })
}

#[derive(Debug, Clone, Default)]
pub struct AntigravityWriteOptions<'a> {
    pub access_token: &'a str,
    pub refresh_token: Option<&'a str>,
    pub expires_at: Option<&'a str>,
    pub email: Option<&'a str>,
}

pub fn write_antigravity_credentials(
    opts: AntigravityWriteOptions<'_>,
) -> Result<std::path::PathBuf, crate::error::CoreError> {
    use std::io::Write;

    let home = home_dir().ok_or_else(|| {
        crate::error::CoreError::Validation("Could not determine home directory".into())
    })?;
    let gemini_dir = home.join(".gemini");
    let cli_dir = gemini_dir.join("antigravity-cli");

    std::fs::create_dir_all(&cli_dir).map_err(|e| {
        crate::error::CoreError::Validation(format!(
            "Failed to create ~/.gemini/antigravity-cli: {e}"
        ))
    })?;

    let token_file = cli_dir.join("antigravity-oauth-token");
    let payload = serde_json::json!({
        "token": {
            "access_token": opts.access_token,
            "token_type": "Bearer",
            "refresh_token": opts.refresh_token.unwrap_or_default(),
            "expiry": opts.expires_at.unwrap_or_default(),
        },
        "auth_method": "consumer"
    });
    let payload_str = serde_json::to_string(&payload).map_err(|e| {
        crate::error::CoreError::Validation(format!("Failed to serialize payload: {e}"))
    })?;

    let mut open_options = std::fs::OpenOptions::new();
    open_options.write(true).create(true).truncate(true);

    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_options.mode(0o600);
    }

    let mut file = open_options.open(&token_file).map_err(|e| {
        crate::error::CoreError::Validation(format!(
            "Failed to open {}: {}",
            token_file.display(),
            e
        ))
    })?;

    file.write_all(payload_str.as_bytes()).map_err(|e| {
        crate::error::CoreError::Validation(format!(
            "Failed to write to {}: {}",
            token_file.display(),
            e
        ))
    })?;

    // Gemini CLI también lee ~/.gemini/oauth_creds.json (SSH y contenedores).
    let creds_file = gemini_dir.join("oauth_creds.json");
    let expiry_ms = opts
        .expires_at
        .and_then(|exp| chrono::DateTime::parse_from_rfc3339(exp).ok())
        .map_or_else(
            || (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp_millis(),
            |dt| dt.timestamp_millis(),
        );
    let creds_payload = serde_json::json!({
        "access_token": opts.access_token,
        "refresh_token": opts.refresh_token.unwrap_or_default(),
        "token_type": "Bearer",
        "expiry_date": expiry_ms,
        "scope": "https://www.googleapis.com/auth/userinfo.email openid https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.profile"
    });
    if let Ok(json_str) = serde_json::to_string_pretty(&creds_payload) {
        let mut opts_creds = std::fs::OpenOptions::new();
        opts_creds.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts_creds.mode(0o600);
        }
        if let Ok(mut f) = opts_creds.open(&creds_file) {
            let _ = f.write_all(json_str.as_bytes());
        }
    }

    if let Some(em) = opts.email.filter(|s| !s.trim().is_empty()) {
        let accounts_file = gemini_dir.join("google_accounts.json");
        let accounts_payload = serde_json::json!({
            "active": em,
            "old": []
        });
        if let Ok(json_str) = serde_json::to_string_pretty(&accounts_payload) {
            let mut opts_acc = std::fs::OpenOptions::new();
            opts_acc.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts_acc.mode(0o600);
            }
            if let Ok(mut f) = opts_acc.open(&accounts_file) {
                let _ = f.write_all(json_str.as_bytes());
            }
        }
    }

    Ok(token_file)
}
