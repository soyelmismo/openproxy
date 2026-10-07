//! Claude Code CLI and Claude-Swap backup credential scanner and manager.

use std::io::Write;
use std::path::PathBuf;

use base64::Engine;
use openproxy_types::error::CoreError;

use super::{DiscoveredAccount, home_dir};

/// Returns the configuration directory for Claude Code.
/// Respects `CLAUDE_CONFIG_DIR` if set, otherwise defaults to `~/.claude`.
pub fn claude_config_dir() -> Option<PathBuf> {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|h| h.join(".claude")))
}

/// Discovers credentials from the active Claude Code CLI installation
/// (`~/.claude/.credentials.json` and `~/.claude.json`).
pub fn scan_claude_code_cli() -> Option<DiscoveredAccount> {
    let config_dir = claude_config_dir()?;
    let creds_path = config_dir.join(".credentials.json");

    let bytes = match std::fs::read(&creds_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %creds_path.display(), error = %e,
                "account_scanner: cannot read claude credentials file");
            return None;
        }
    };

    let v: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(path = %creds_path.display(), error = %e,
                "account_scanner: claude credentials JSON parse failed");
            return None;
        }
    };

    let oauth_obj = v.get("claudeAiOauth").unwrap_or(&v);

    let Some(access_token) = oauth_obj
        .get("accessToken")
        .or_else(|| oauth_obj.get("access_token"))
        .and_then(|a| a.as_str())
        .map(str::to_string)
    else {
        tracing::warn!(path = %creds_path.display(),
            "account_scanner: claude credentials file missing accessToken");
        return None;
    };

    let refresh_token = oauth_obj
        .get("refreshToken")
        .or_else(|| oauth_obj.get("refresh_token"))
        .and_then(|s| s.as_str())
        .map(str::to_string);

    // Read identity / email and UUIDs from ~/.claude.json (or config_dir/.config.json)
    let mut email = None;
    let mut account_uuid = None;
    let mut org_uuid = None;
    let global_config = home_dir().map(|h| h.join(".claude.json"));
    let local_config = config_dir.join(".config.json");

    for candidate in [global_config, Some(local_config)].into_iter().flatten() {
        if let Ok(cfg_bytes) = std::fs::read(&candidate)
            && let Ok(cfg_v) = serde_json::from_slice::<serde_json::Value>(&cfg_bytes)
        {
            if let Some(acc) = cfg_v.get("oauthAccount").and_then(|v| v.as_object()) {
                if email.is_none()
                    && let Some(em) = acc
                        .get("emailAddress")
                        .and_then(|s| s.as_str())
                        .filter(|s| !s.trim().is_empty())
                {
                    email = Some(em.to_string());
                }
                if account_uuid.is_none()
                    && let Some(u) = acc
                        .get("accountUuid")
                        .or_else(|| acc.get("account_uuid"))
                        .and_then(|s| s.as_str())
                {
                    account_uuid = Some(u.to_string());
                }
                if org_uuid.is_none()
                    && let Some(o) = acc
                        .get("organizationUuid")
                        .or_else(|| acc.get("organization_uuid"))
                        .and_then(|s| s.as_str())
                {
                    org_uuid = Some(o.to_string());
                }
            }
            if email.is_some() && account_uuid.is_some() {
                break;
            }
        }
    }

    if email.is_none()
        && let Some(claims) = crate::oauth::decode_jwt_payload(&access_token)
    {
        email = claims
            .get("email")
            .and_then(|e| e.as_str())
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string);
    }

    let label = match email.as_deref() {
        Some(e) => format!("claude-code@{e}"),
        None => "claude-code".to_string(),
    };

    let oauth_provider_specific = if account_uuid.is_some() || org_uuid.is_some() {
        Some(
            serde_json::json!({
                "account_uuid": account_uuid,
                "organization_uuid": org_uuid,
            })
            .to_string(),
        )
    } else {
        None
    };

    Some(DiscoveredAccount {
        provider_id: "claude-code".to_string(),
        label,
        access_token,
        refresh_token,
        email,
        source_path: creds_path,
        oauth_provider_specific,
    })
}

/// Discovers accounts stored in Claude-Swap (cswap) backup directories:
/// - `~/.claude-swap-backup/`
/// - `~/.local/share/claude-swap/`
/// - `~/.config/claude-swap/backups/`
pub fn scan_claude_swap_backups() -> Vec<DiscoveredAccount> {
    let mut discovered = Vec::new();
    let Some(home) = home_dir() else {
        return discovered;
    };

    let candidate_dirs = [
        home.join(".claude-swap-backup"),
        home.join(".local").join("share").join("claude-swap"),
        home.join(".config").join("claude-swap").join("backups"),
    ];

    for dir in &candidate_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let Ok(raw_bytes) = std::fs::read(&path) else {
                continue;
            };

            // Attempt to parse directly as JSON or decode from base64
            let json_val = if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw_bytes) {
                Some(v)
            } else if let Ok(decoded) = base64::engine::general_purpose::STANDARD.decode(&raw_bytes)
                && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&decoded)
            {
                Some(v)
            } else if let Ok(trimmed) = std::str::from_utf8(&raw_bytes)
                && let Ok(decoded) =
                    base64::engine::general_purpose::STANDARD.decode(trimmed.trim())
                && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&decoded)
            {
                Some(v)
            } else {
                None
            };

            let Some(val) = json_val else {
                continue;
            };

            let oauth_obj = val.get("claudeAiOauth").unwrap_or(&val);
            let Some(access_token) = oauth_obj
                .get("accessToken")
                .or_else(|| oauth_obj.get("access_token"))
                .and_then(|s| s.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
            else {
                continue;
            };

            let refresh_token = oauth_obj
                .get("refreshToken")
                .or_else(|| oauth_obj.get("refresh_token"))
                .and_then(|s| s.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string);

            let email = val
                .get("oauthAccount")
                .and_then(|a| a.get("emailAddress"))
                .or_else(|| val.get("emailAddress"))
                .or_else(|| val.get("email"))
                .and_then(|s| s.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(str::to_string)
                .or_else(|| {
                    crate::oauth::decode_jwt_payload(&access_token).and_then(|claims| {
                        claims
                            .get("email")
                            .and_then(|e| e.as_str())
                            .map(str::to_string)
                    })
                });

            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("backup");

            let label = match email.as_deref() {
                Some(e) => format!("claude-code@{e}"),
                None => format!("claude-swap@{stem}"),
            };

            let account_uuid = val
                .get("oauthAccount")
                .and_then(|a| a.get("accountUuid").or_else(|| a.get("account_uuid")))
                .or_else(|| val.get("accountUuid"))
                .and_then(|s| s.as_str())
                .map(str::to_string);

            let org_uuid = val
                .get("oauthAccount")
                .and_then(|a| {
                    a.get("organizationUuid")
                        .or_else(|| a.get("organization_uuid"))
                })
                .or_else(|| val.get("organizationUuid"))
                .and_then(|s| s.as_str())
                .map(str::to_string);

            let oauth_provider_specific = if account_uuid.is_some() || org_uuid.is_some() {
                Some(
                    serde_json::json!({
                        "account_uuid": account_uuid,
                        "organization_uuid": org_uuid,
                    })
                    .to_string(),
                )
            } else {
                None
            };

            discovered.push(DiscoveredAccount {
                provider_id: "claude-code".to_string(),
                label,
                access_token,
                refresh_token,
                email,
                source_path: path,
                oauth_provider_specific,
            });
        }
    }

    discovered
}

#[derive(Debug, Clone, Default)]
pub struct ClaudeCodeWriteOptions<'a> {
    pub access_token: &'a str,
    pub refresh_token: Option<&'a str>,
    pub expires_at: Option<&'a str>,
    pub email: Option<&'a str>,
    pub account_uuid: Option<&'a str>,
    pub org_uuid: Option<&'a str>,
    pub subscription_type: Option<&'a str>,
    pub rate_limit_tier: Option<&'a str>,
}

/// Applies Claude Code credentials to local CLI files:
/// 1. Writes `~/.claude/.credentials.json` (mode 0600 on unix) with `claudeAiOauth`.
/// 2. Updates `~/.claude.json` with `oauthAccount` (email, accountUuid, organizationUuid).
/// 3. Synchronizes to `~/.claude-swap-backup` if the directory exists.
pub fn write_claude_code_credentials(
    opts: ClaudeCodeWriteOptions<'_>,
) -> Result<PathBuf, CoreError> {
    let config_dir = claude_config_dir().ok_or_else(|| {
        CoreError::Validation("Could not determine Claude configuration directory".into())
    })?;

    std::fs::create_dir_all(&config_dir).map_err(|e| {
        CoreError::Validation(format!(
            "Failed to create Claude config directory {}: {e}",
            config_dir.display()
        ))
    })?;

    let creds_file = config_dir.join(".credentials.json");

    // Read existing credentials JSON to preserve other fields
    let mut creds_root = match std::fs::read(&creds_file) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };

    if !creds_root.is_object() {
        creds_root = serde_json::json!({});
    }

    let existing_oauth = creds_root.get("claudeAiOauth");
    let existing_sub = existing_oauth
        .and_then(|v| v.get("subscriptionType"))
        .and_then(|s| s.as_str())
        .map(str::to_string);
    let existing_tier = existing_oauth
        .and_then(|v| v.get("rateLimitTier"))
        .and_then(|s| s.as_str())
        .map(str::to_string);

    let effective_sub = opts
        .subscription_type
        .map(str::to_string)
        .or(existing_sub)
        .unwrap_or_else(|| "pro".to_string());
    let effective_tier = opts
        .rate_limit_tier
        .map(str::to_string)
        .or(existing_tier)
        .unwrap_or_else(|| "default_claude_ai".to_string());

    let expiry_ms = opts
        .expires_at
        .and_then(|exp| chrono::DateTime::parse_from_rfc3339(exp).ok())
        .map_or_else(
            || (chrono::Utc::now() + chrono::Duration::hours(1)).timestamp_millis(),
            |dt| dt.timestamp_millis(),
        );

    let claude_ai_oauth = serde_json::json!({
        "accessToken": opts.access_token,
        "refreshToken": opts.refresh_token.unwrap_or_default(),
        "expiresAt": expiry_ms,
        "scopes": ["org:create_api_key", "user:profile", "user:inference"],
        "subscriptionType": effective_sub.as_str(),
        "rateLimitTier": effective_tier.as_str(),
    });

    if let Some(obj) = creds_root.as_object_mut() {
        obj.insert("claudeAiOauth".to_string(), claude_ai_oauth.clone());
    }

    let creds_str = serde_json::to_string_pretty(&creds_root)
        .map_err(|e| CoreError::Validation(format!("Failed to serialize credentials: {e}")))?;

    let mut fs_opts = std::fs::OpenOptions::new();
    fs_opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs_opts.mode(0o600);
    }

    let mut f = fs_opts.open(&creds_file).map_err(|e| {
        CoreError::Validation(format!(
            "Failed to open {} for writing: {e}",
            creds_file.display()
        ))
    })?;

    f.write_all(creds_str.as_bytes()).map_err(|e| {
        CoreError::Validation(format!("Failed to write to {}: {e}", creds_file.display()))
    })?;

    // Update global ~/.claude.json oauthAccount and hasAvailableSubscription
    if let Some(home) = home_dir() {
        let global_config = home.join(".claude.json");
        let mut global_root = match std::fs::read(&global_config) {
            Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
                .unwrap_or_else(|_| serde_json::json!({})),
            Err(_) => serde_json::json!({}),
        };

        if !global_root.is_object() {
            global_root = serde_json::json!({});
        }

        let mut oauth_account = match global_root.get("oauthAccount").and_then(|v| v.as_object()) {
            Some(obj) => serde_json::Value::Object(obj.clone()),
            None => serde_json::json!({}),
        };

        if let Some(u) = opts.account_uuid {
            oauth_account["accountUuid"] = serde_json::Value::String(u.to_string());
        }
        if let Some(e) = opts.email {
            oauth_account["emailAddress"] = serde_json::Value::String(e.to_string());
        }
        if let Some(o) = opts.org_uuid {
            oauth_account["organizationUuid"] = serde_json::Value::String(o.to_string());
        }
        if oauth_account.get("billingType").is_none() {
            oauth_account["billingType"] = serde_json::json!("stripe_subscription");
        }
        let org_type = match effective_sub.as_str() {
            "max" => "claude_max",
            "team" => "claude_team",
            "enterprise" => "claude_enterprise",
            _ => "claude_pro",
        };
        oauth_account["organizationType"] = serde_json::json!(org_type);
        oauth_account["organizationRateLimitTier"] = serde_json::json!(effective_tier);

        if let Some(obj) = global_root.as_object_mut() {
            obj.insert("oauthAccount".to_string(), oauth_account.clone());
            obj.insert(
                "hasAvailableSubscription".to_string(),
                serde_json::Value::Bool(true),
            );
        }

        if let Ok(global_str) = serde_json::to_string_pretty(&global_root) {
            let mut g_opts = std::fs::OpenOptions::new();
            g_opts.write(true).create(true).truncate(true);
            if let Ok(mut gf) = g_opts.open(&global_config) {
                let _ = gf.write_all(global_str.as_bytes());
            }
        }

        // Synchronize with Claude-Swap backup folder if present
        let cswap_backup_dir = home.join(".claude-swap-backup");
        if cswap_backup_dir.is_dir() {
            let backup_file_name = if let Some(em) = opts.email.filter(|s| !s.trim().is_empty()) {
                let safe_em: String = em
                    .chars()
                    .map(|c| {
                        if c.is_alphanumeric() || c == '.' || c == '-' {
                            c
                        } else {
                            '_'
                        }
                    })
                    .collect();
                format!("account-{safe_em}.json")
            } else {
                "account-current.json".to_string()
            };
            let backup_file = cswap_backup_dir.join(backup_file_name);
            let backup_payload = serde_json::json!({
                "claudeAiOauth": claude_ai_oauth,
                "oauthAccount": oauth_account,
            });
            if let Ok(b_str) = serde_json::to_string_pretty(&backup_payload) {
                let mut b_opts = std::fs::OpenOptions::new();
                b_opts.write(true).create(true).truncate(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    b_opts.mode(0o600);
                }
                if let Ok(mut bf) = b_opts.open(&backup_file) {
                    let _ = bf.write_all(b_str.as_bytes());
                }
            }
        }
    }

    Ok(creds_file)
}
