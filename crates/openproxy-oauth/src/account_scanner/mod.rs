//! External CLI credential scanner & manager.
//!
//! Discovers OAuth credentials from host tools (Antigravity CLI, Claude Code CLI,
//! Claude-Swap backups) and synchronizes credentials back to the local environment.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub(crate) mod antigravity;
pub(crate) mod claude_code;

pub(crate) mod local_sync;

#[cfg(test)]
mod tests;

pub use antigravity::{
    AntigravityWriteOptions, scan_antigravity_cli, scan_antigravity_oauth_creds,
    write_antigravity_credentials,
};
pub use claude_code::{
    ClaudeCodeWriteOptions, claude_config_dir, scan_claude_code_cli, scan_claude_swap_backups,
    write_claude_code_credentials,
};
pub use local_sync::{
    antigravity_account_matches, check_local_cli_updated_tokens, claude_code_account_matches,
    sync_to_local_cli_if_active,
};

/// Home directory via `std::env::var_os("HOME")`, fallback to `USERPROFILE`.
pub(crate) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredAccount {
    /// Provider ID: `"antigravity"`, `"claude-code"`, etc.
    pub provider_id: String,
    /// Suggested label, e.g. `"antigravity-cli@alice@example.com"` or `"claude-code@user@example.com"`.
    pub label: String,
    /// Raw OAuth access token (encrypted in DB upon import).
    pub access_token: String,
    /// OAuth refresh token, needed for background refresh.
    pub refresh_token: Option<String>,
    /// User email if available.
    pub email: Option<String>,
    /// Source file path (for audit / duplicate skipping).
    pub source_path: PathBuf,
    /// Provider-specific metadata (e.g. account_uuid, organization_uuid).
    pub oauth_provider_specific: Option<String>,
    /// Token expiry timestamp in RFC 3339 format if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

impl DiscoveredAccount {
    /// Security (OP-18): a metadata-only copy for dry-run / preview responses.
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
            oauth_provider_specific: self.oauth_provider_specific.clone(),
            expires_at: self.expires_at.clone(),
        }
    }
}

/// Discovers accounts from local CLIs (Antigravity CLI, Antigravity OAuth creds,
/// Claude Code CLI, Claude-Swap backups).
pub fn scan_external_accounts() -> Vec<DiscoveredAccount> {
    let mut accounts = Vec::new();

    // Antigravity CLI
    if let Some(cli_acc) = scan_antigravity_cli() {
        accounts.push(cli_acc);
    }

    // Antigravity OAuth creds
    if let Some(creds_acc) = scan_antigravity_oauth_creds() {
        let duplicate = accounts.iter().any(|a| {
            a.provider_id == creds_acc.provider_id
                && ((a.email.is_some() && a.email == creds_acc.email)
                    || a.access_token == creds_acc.access_token
                    || (a.refresh_token.is_some() && a.refresh_token == creds_acc.refresh_token))
        });
        if !duplicate {
            accounts.push(creds_acc);
        }
    }

    // Claude Code CLI
    if let Some(claude_acc) = scan_claude_code_cli() {
        let duplicate = accounts.iter().any(|a| {
            a.provider_id == claude_acc.provider_id
                && ((a.email.is_some() && a.email == claude_acc.email)
                    || a.access_token == claude_acc.access_token
                    || (a.refresh_token.is_some() && a.refresh_token == claude_acc.refresh_token))
        });
        if !duplicate {
            accounts.push(claude_acc);
        }
    }

    // Claude-Swap backups
    for cswap_acc in scan_claude_swap_backups() {
        let duplicate = accounts.iter().any(|a| {
            a.provider_id == cswap_acc.provider_id
                && ((a.email.is_some() && a.email == cswap_acc.email)
                    || a.access_token == cswap_acc.access_token
                    || (a.refresh_token.is_some() && a.refresh_token == cswap_acc.refresh_token))
        });
        if !duplicate {
            accounts.push(cswap_acc);
        }
    }

    accounts
}
