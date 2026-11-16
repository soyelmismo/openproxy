//! Host CLI credential synchronization.
//!
//! Provides bidirectional synchronization between OpenProxy's database and local
//! CLI credentials files (Claude Code CLI, Antigravity CLI).
//!
//! Prevents `invalid_grant` / "Refresh token not found or invalid" failures caused by
//! external refresh token rotation (RTR) when running host CLIs.

use std::path::PathBuf;

use super::{
    DiscoveredAccount, scan_antigravity_cli, scan_antigravity_oauth_creds, scan_claude_code_cli,
    scan_claude_swap_backups,
};
use openproxy_types::Result;

fn match_non_empty_emails(acc_email: Option<&str>, disc_email: Option<&str>) -> Option<bool> {
    if let (Some(a), Some(d)) = (acc_email, disc_email)
        && !a.trim().is_empty()
        && !d.trim().is_empty()
    {
        return Some(a.trim().eq_ignore_ascii_case(d.trim()));
    }
    None
}

/// Matches an OpenProxy account against a discovered local CLI credential for Claude Code.
pub fn claude_code_account_matches(
    account: &crate::accounts::Account,
    discovered: &DiscoveredAccount,
) -> bool {
    if let Some(matched) =
        match_non_empty_emails(account.email.as_deref(), discovered.email.as_deref())
    {
        return matched;
    }

    if let (Some(acc_spec), Some(disc_spec)) = (
        account.oauth_provider_specific.as_deref(),
        discovered.oauth_provider_specific.as_deref(),
    ) && let (Ok(acc_val), Ok(disc_val)) = (
        serde_json::from_str::<serde_json::Value>(acc_spec),
        serde_json::from_str::<serde_json::Value>(disc_spec),
    ) {
        let acc_u = acc_val.get("account_uuid").and_then(|v| v.as_str());
        let disc_u = disc_val.get("account_uuid").and_then(|v| v.as_str());
        if let (Some(u1), Some(u2)) = (acc_u, disc_u)
            && !u1.is_empty()
            && u1 == u2
        {
            return true;
        }
    }

    account.email.is_none() && discovered.email.is_none()
}

/// Matches an OpenProxy account against a discovered local CLI credential for Antigravity.
pub fn antigravity_account_matches(
    account: &crate::accounts::Account,
    discovered: &DiscoveredAccount,
) -> bool {
    match_non_empty_emails(account.email.as_deref(), discovered.email.as_deref())
        .unwrap_or_else(|| account.email.is_none() && discovered.email.is_none())
}

fn tokens_differ(
    disc: &DiscoveredAccount,
    db_access_token: Option<&str>,
    db_refresh_token: Option<&str>,
    check_access: bool,
) -> bool {
    let rt_differs = match (&disc.refresh_token, db_refresh_token) {
        (Some(disc_rt), Some(db_rt)) => disc_rt != db_rt,
        (Some(_), None) => true,
        _ => false,
    };
    let at_differs = check_access
        && match (db_access_token, &disc.access_token) {
            (Some(db_at), disc_at) => db_at != disc_at,
            (None, _) => true,
        };
    rt_differs || at_differs
}

/// Checks whether the host CLI on disk holds updated credentials for the given account
/// that differ from OpenProxy's database.
pub fn check_local_cli_updated_tokens(
    provider_id: &str,
    account: &crate::accounts::Account,
    db_access_token: Option<&str>,
    db_refresh_token: Option<&str>,
) -> Option<DiscoveredAccount> {
    match provider_id {
        "claude-code" | "claude" => {
            if let Some(disc) = scan_claude_code_cli()
                && claude_code_account_matches(account, &disc)
                && tokens_differ(&disc, db_access_token, db_refresh_token, true)
            {
                return Some(disc);
            }
            scan_claude_swap_backups().into_iter().find(|disc| {
                claude_code_account_matches(account, disc)
                    && tokens_differ(disc, db_access_token, db_refresh_token, false)
            })
        }
        "antigravity" => {
            if let Some(disc) = scan_antigravity_cli()
                && antigravity_account_matches(account, &disc)
                && tokens_differ(&disc, db_access_token, db_refresh_token, true)
            {
                return Some(disc);
            }
            scan_antigravity_oauth_creds().filter(|disc| {
                antigravity_account_matches(account, disc)
                    && tokens_differ(disc, db_access_token, db_refresh_token, false)
            })
        }
        _ => None,
    }
}

/// Extracts `oauth_provider_specific` metadata and writes Claude Code CLI credentials
/// for `account` to disk. Shared by background OAuth refresh sync and admin CLI injection.
pub fn write_account_to_claude_code_cli(
    account: &crate::accounts::Account,
    access_token: &str,
    refresh_token: Option<&str>,
    expires_at: Option<&str>,
    email: Option<&str>,
) -> Result<PathBuf> {
    let (account_uuid, org_uuid, sub_type, rate_tier) = account
        .oauth_provider_specific
        .as_deref()
        .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
        .map_or((None, None, None, None), |val| {
            let get_str = |key: &str| val.get(key).and_then(|v| v.as_str()).map(str::to_string);
            (
                get_str("account_uuid"),
                get_str("organization_uuid"),
                get_str("subscription_type"),
                get_str("rate_limit_tier"),
            )
        });

    let inferred_sub_type = sub_type.as_deref().or_else(|| {
        account.quota_plan_name.as_deref().and_then(|p| {
            let p_low = p.to_lowercase();
            if p_low.contains("pro") {
                Some("pro")
            } else if p_low.contains("max") {
                Some("max")
            } else if p_low.contains("team") {
                Some("team")
            } else {
                None
            }
        })
    });

    super::claude_code::write_claude_code_credentials(super::claude_code::ClaudeCodeWriteOptions {
        access_token,
        refresh_token,
        expires_at,
        email: email.or(account.email.as_deref()),
        account_uuid: account_uuid.as_deref(),
        org_uuid: org_uuid.as_deref(),
        subscription_type: inferred_sub_type,
        rate_limit_tier: rate_tier.as_deref(),
    })
}

/// Propagates freshly refreshed tokens back to the host CLI on disk if the refreshed account
/// is currently active locally.
pub fn sync_to_local_cli_if_active(
    provider_id: &str,
    account: &crate::accounts::Account,
    access_token: &str,
    refresh_token: Option<&str>,
    expires_at: Option<&str>,
    email: Option<&str>,
) -> bool {
    let write_res = match provider_id {
        "claude-code" | "claude" => {
            let Some(disc) = scan_claude_code_cli() else {
                return false;
            };
            if !claude_code_account_matches(account, &disc) {
                return false;
            }
            write_account_to_claude_code_cli(account, access_token, refresh_token, expires_at, email)
        }
        "antigravity" => {
            let Some(disc) = scan_antigravity_cli() else {
                return false;
            };
            if !antigravity_account_matches(account, &disc) {
                return false;
            }
            super::antigravity::write_antigravity_credentials(
                super::antigravity::AntigravityWriteOptions {
                    access_token,
                    refresh_token,
                    expires_at,
                    email: email.or(account.email.as_deref()),
                },
            )
        }
        _ => return false,
    };

    match write_res {
        Ok(_) => {
            tracing::info!(
                account = account.id.0,
                provider = provider_id,
                "sync_to_local_cli: refreshed tokens written to local CLI credentials"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                account = account.id.0,
                provider = provider_id,
                error = %e,
                "sync_to_local_cli: failed to write refreshed credentials to disk"
            );
            false
        }
    }
}
