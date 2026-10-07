//! Host CLI credential synchronization.
//!
//! Provides bidirectional synchronization between OpenProxy's database and local
//! CLI credentials files (Claude Code CLI, Antigravity CLI).
//!
//! Prevents `invalid_grant` / "Refresh token not found or invalid" failures caused by
//! external refresh token rotation (RTR) when running host CLIs.

use super::{
    DiscoveredAccount, scan_antigravity_cli, scan_antigravity_oauth_creds, scan_claude_code_cli,
    scan_claude_swap_backups,
};

/// Matches an OpenProxy account against a discovered local CLI credential for Claude Code.
pub fn claude_code_account_matches(
    account: &crate::accounts::Account,
    discovered: &DiscoveredAccount,
) -> bool {
    // 1. By email
    if let (Some(acc_email), Some(disc_email)) =
        (account.email.as_deref(), discovered.email.as_deref())
        && !acc_email.trim().is_empty()
        && !disc_email.trim().is_empty()
    {
        return acc_email.trim().eq_ignore_ascii_case(disc_email.trim());
    }

    // 2. By account_uuid in oauth_provider_specific
    if let (Some(acc_spec), Some(disc_spec)) = (
        account.oauth_provider_specific.as_deref(),
        discovered.oauth_provider_specific.as_deref(),
    )
        && let (Ok(acc_val), Ok(disc_val)) = (
            serde_json::from_str::<serde_json::Value>(acc_spec),
            serde_json::from_str::<serde_json::Value>(disc_spec),
        )
    {
        let acc_u = acc_val.get("account_uuid").and_then(|v| v.as_str());
        let disc_u = disc_val.get("account_uuid").and_then(|v| v.as_str());
        if let (Some(u1), Some(u2)) = (acc_u, disc_u)
            && !u1.is_empty()
            && u1 == u2
        {
            return true;
        }
    }

    // 3. Fallback: single local account without email
    if account.email.is_none() && discovered.email.is_none() {
        return true;
    }

    false
}

/// Matches an OpenProxy account against a discovered local CLI credential for Antigravity.
pub fn antigravity_account_matches(
    account: &crate::accounts::Account,
    discovered: &DiscoveredAccount,
) -> bool {
    if let (Some(acc_email), Some(disc_email)) =
        (account.email.as_deref(), discovered.email.as_deref())
        && !acc_email.trim().is_empty()
        && !disc_email.trim().is_empty()
    {
        return acc_email.trim().eq_ignore_ascii_case(disc_email.trim());
    }
    if account.email.is_none() && discovered.email.is_none() {
        return true;
    }
    false
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
            // Check active CLI credentials (~/.claude/.credentials.json)
            if let Some(disc) = scan_claude_code_cli()
                && claude_code_account_matches(account, &disc)
            {
                let rt_differs = match (&disc.refresh_token, db_refresh_token) {
                    (Some(disc_rt), Some(db_rt)) => disc_rt != db_rt,
                    (Some(_), None) => true,
                    _ => false,
                };
                let at_differs = match (db_access_token, &disc.access_token) {
                    (Some(db_at), disc_at) => db_at != disc_at,
                    (None, _) => true,
                };
                if rt_differs || at_differs {
                    return Some(disc);
                }
            }
            // Fallback: check backups in ~/.claude-swap-backup
            for disc in scan_claude_swap_backups() {
                if claude_code_account_matches(account, &disc) {
                    let rt_differs = match (&disc.refresh_token, db_refresh_token) {
                        (Some(disc_rt), Some(db_rt)) => disc_rt != db_rt,
                        (Some(_), None) => true,
                        _ => false,
                    };
                    if rt_differs {
                        return Some(disc);
                    }
                }
            }
            None
        }
        "antigravity" => {
            if let Some(disc) = scan_antigravity_cli()
                && antigravity_account_matches(account, &disc)
            {
                let rt_differs = match (&disc.refresh_token, db_refresh_token) {
                    (Some(disc_rt), Some(db_rt)) => disc_rt != db_rt,
                    (Some(_), None) => true,
                    _ => false,
                };
                let at_differs = match (db_access_token, &disc.access_token) {
                    (Some(db_at), disc_at) => db_at != disc_at,
                    (None, _) => true,
                };
                if rt_differs || at_differs {
                    return Some(disc);
                }
            }
            if let Some(disc) = scan_antigravity_oauth_creds()
                && antigravity_account_matches(account, &disc)
            {
                let rt_differs = match (&disc.refresh_token, db_refresh_token) {
                    (Some(disc_rt), Some(db_rt)) => disc_rt != db_rt,
                    (Some(_), None) => true,
                    _ => false,
                };
                if rt_differs {
                    return Some(disc);
                }
            }
            None
        }
        _ => None,
    }
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
    match provider_id {
        "claude-code" | "claude" => {
            if let Some(disc) = scan_claude_code_cli()
                && claude_code_account_matches(account, &disc)
            {
                let (account_uuid, org_uuid, sub_type, rate_tier) = account
                    .oauth_provider_specific
                    .as_deref()
                    .and_then(|json| serde_json::from_str::<serde_json::Value>(json).ok())
                    .map_or((None, None, None, None), |val| {
                        let acc_uuid = val
                            .get("account_uuid")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let org_uuid = val
                            .get("organization_uuid")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let sub = val
                            .get("subscription_type")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        let tier = val
                            .get("rate_limit_tier")
                            .and_then(|v| v.as_str())
                            .map(str::to_string);
                        (acc_uuid, org_uuid, sub, tier)
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

                let write_res = super::claude_code::write_claude_code_credentials(
                    super::claude_code::ClaudeCodeWriteOptions {
                        access_token,
                        refresh_token,
                        expires_at,
                        email: email.or(account.email.as_deref()),
                        account_uuid: account_uuid.as_deref(),
                        org_uuid: org_uuid.as_deref(),
                        subscription_type: inferred_sub_type,
                        rate_limit_tier: rate_tier.as_deref(),
                    },
                );
                if let Err(e) = write_res {
                    tracing::warn!(
                        account = account.id.0,
                        error = %e,
                        "sync_to_local_cli: failed to write refreshed claude-code credentials to disk"
                    );
                    return false;
                }
                tracing::info!(
                    account = account.id.0,
                    "sync_to_local_cli: refreshed claude-code tokens written to local CLI credentials"
                );
                return true;
            }
            false
        }
        "antigravity" => {
            if let Some(disc) = scan_antigravity_cli()
                && antigravity_account_matches(account, &disc)
            {
                let write_res = super::antigravity::write_antigravity_credentials(
                    super::antigravity::AntigravityWriteOptions {
                        access_token,
                        refresh_token,
                        expires_at,
                        email: email.or(account.email.as_deref()),
                    },
                );
                if let Err(e) = write_res {
                    tracing::warn!(
                        account = account.id.0,
                        error = %e,
                        "sync_to_local_cli: failed to write refreshed antigravity credentials to disk"
                    );
                    return false;
                }
                tracing::info!(
                    account = account.id.0,
                    "sync_to_local_cli: refreshed antigravity tokens written to local CLI credentials"
                );
                return true;
            }
            false
        }
        _ => false,
    }
}
