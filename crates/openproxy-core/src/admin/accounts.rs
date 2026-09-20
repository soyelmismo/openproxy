//! Account and quota administration service layer.

use crate::accounts;
use crate::error::{CoreError, Result};
use crate::ids::{AccountId, ProviderId};
use crate::quota::AccountQuota;
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::secrets::MasterKey;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Inputs for [`create_account`]. The plaintext `api_key` is encrypted via
/// `master_key` before insertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAccountInput {
    pub provider_id: String,
    /// API key for api_key accounts. `None` for OAuth accounts.
    /// Accepts both `api_key` and `secret` as JSON field names for
    /// compatibility with the web UI (which sends `secret`).
    #[serde(alias = "secret")]
    pub api_key: Option<String>,
    pub label: Option<String>,
    pub priority: Option<i32>,
    pub extra_config_json: Option<String>,
}

/// Insert a new account. The plaintext `api_key` is encrypted with
/// `master_key` and only the resulting BLOB is stored.
///
/// `priority` defaults to `100` when not provided, matching the
/// "lower = higher priority" convention documented in
/// [`crate::accounts`].
pub fn create_account(
    conn: &Connection,
    master_key: &MasterKey,
    input: CreateAccountInput,
) -> Result<AccountId> {
    let provider = ProviderId::new(input.provider_id);
    if let Some(key) = input
        .api_key
        .as_deref()
        .map(str::trim)
        .filter(|k| !k.is_empty())
    {
        let existing_keys = accounts::list_api_keys_for_provider(conn, &provider, master_key)?;
        if existing_keys.contains(key) {
            return Err(CoreError::Validation(
                "an account with this API key already exists for this provider".into(),
            ));
        }
    }
    let priority = input.priority.unwrap_or(100);
    let effective_label = match input.label.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => Some(s.to_string()),
        _ => input
            .api_key
            .as_deref()
            .filter(|k| !k.trim().is_empty())
            .map(openproxy_db::accounts::summarize_api_key),
    };
    accounts::create(
        conn,
        &provider,
        input.api_key.as_deref().map(str::trim),
        master_key,
        effective_label.as_deref(),
        priority,
        input.extra_config_json.as_deref(),
    )
}

/// Single item for bulk account creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkCreateAccountItem {
    #[serde(alias = "secret")]
    pub api_key: String,
    pub label: Option<String>,
    pub priority: Option<i32>,
    pub extra_config_json: Option<String>,
}

/// Inputs for [`bulk_create_accounts`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkCreateAccountsInput {
    pub provider_id: String,
    pub items: Vec<BulkCreateAccountItem>,
}

/// Response returned by bulk account creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkCreateAccountsResponse {
    pub created: usize,
    pub ids: Vec<AccountId>,
}

/// Insert multiple accounts in batch.
///
/// Deduplication (3 phases):
/// 1. Queries and decrypts all existing API keys for `provider_id` in the database.
/// 2. Tracks seen keys in-memory across the incoming batch.
/// 3. Silently discards duplicate keys (already in DB or duplicated within the batch),
///    making bulk creation idempotent and duplicate-free.
pub fn bulk_create_accounts(
    conn: &Connection,
    master_key: &MasterKey,
    input: BulkCreateAccountsInput,
) -> Result<Vec<AccountId>> {
    let provider = ProviderId::new(input.provider_id);
    let mut existing_keys = accounts::list_api_keys_for_provider(conn, &provider, master_key)?;
    let mut ids = Vec::with_capacity(input.items.len());

    for item in input.items {
        let trimmed = item.api_key.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Deduplication: skip if already in DB for this provider or previously seen in this batch
        if !existing_keys.insert(trimmed.to_string()) {
            continue;
        }

        let priority = item.priority.unwrap_or(100);
        let effective_label = match item.label.as_deref().map(str::trim) {
            Some(s) if !s.is_empty() => Some(s.to_string()),
            _ => Some(openproxy_db::accounts::summarize_api_key(trimmed)),
        };
        let id = accounts::create(
            conn,
            &provider,
            Some(trimmed),
            master_key,
            effective_label.as_deref(),
            priority,
            item.extra_config_json.as_deref(),
        )?;
        ids.push(id);
    }
    Ok(ids)
}

/// List accounts, optionally filtered by provider.
/// The `master_key` is required to decrypt `oauth_provider_specific`.
pub fn list_accounts(
    conn: &Connection,
    provider: Option<&ProviderId>,
    master_key: &MasterKey,
) -> Result<Vec<accounts::Account>> {
    accounts::list(conn, provider, master_key)
}

/// Delete an account by id. Idempotent.
pub fn delete_account(conn: &Connection, id: AccountId) -> Result<()> {
    accounts::delete(conn, id)
}

/// Input for [`update_account_api_key`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAccountApiKeyInput {
    /// New API key. `null` clears the key (OAuth-only account).
    pub api_key: Option<String>,
}

/// Encrypt and store (or clear) the API key for an existing account.
/// Returns [`CoreError::AccountNotFound`] when `id` is missing.
pub fn update_account_api_key(
    conn: &Connection,
    master_key: &MasterKey,
    id: AccountId,
    input: &UpdateAccountApiKeyInput,
) -> Result<()> {
    accounts::update_api_key(conn, id, input.api_key.as_deref(), master_key)
}

/// Input for [`update_account_label`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAccountLabelInput {
    pub label: Option<String>,
}

/// Update the label for an existing account.
/// Returns [`CoreError::AccountNotFound`] when `id` is missing.
pub fn update_account_label(
    conn: &Connection,
    id: AccountId,
    input: &UpdateAccountLabelInput,
) -> Result<()> {
    accounts::update_label(conn, id, input.label.as_deref())
}

/// Get the decrypted API key for an account.
/// Returns [`CoreError::AccountNotFound`] when `id` is missing.
pub fn get_account_api_key(
    conn: &Connection,
    master_key: &MasterKey,
    id: AccountId,
) -> Result<String> {
    accounts::decrypt_api_key(conn, id, master_key)
}

/// Decrypt an account's API key. The connection must be dropped by the
/// caller before any async work (e.g. the upstream HTTP call); this
/// helper exists so the quota-refresh path doesn't have to repeat the
/// `decrypt_api_key` boilerplate.
pub fn decrypt_api_key_for_account(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
) -> Result<String> {
    accounts::decrypt_api_key(conn, id, master_key)
}

/// Stamp a quota snapshot onto an account row. See
/// [`accounts::set_quota`] for the column-level semantics.
pub fn persist_account_quota(conn: &Connection, id: AccountId, q: &AccountQuota) -> Result<()> {
    accounts::set_quota(conn, id, q)
}

/// Look up the account row needed to route a quota refresh. Returns
/// the account on success, or [`CoreError::AccountNotFound`] when the
/// id is missing. The caller still holds the writer guard when this
/// returns; the typical pattern is to call this, drop the guard, then
/// fire the upstream HTTP call.
/// The `master_key` is required to decrypt `oauth_provider_specific`.
pub fn account_for_quota_refresh(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
) -> Result<accounts::Account> {
    accounts::get(conn, id, master_key)?.ok_or(CoreError::AccountNotFound(id.0))
}

/// Fetch quota for a single account using the right provider-specific
/// fetcher. Today MiniMax (and its CN sibling), OpenRouter, and
/// Antigravity have fetchers; any other provider id returns an
/// `AccountQuota` with all-NULL numeric fields and a `fetch_error`
/// string saying the provider is unsupported.
///
/// Fetch quota for a single account using the right provider-specific fetcher,
/// optionally passing a proxy URL for upstream auxiliary routing.
pub async fn fetch_account_quota_with_proxy(
    provider_id: &str,
    upstream: &Arc<UpstreamClient>,
    api_key: &str,
    access_token: Option<&str>,
    provider_specific: Option<&str>,
    proxy_url: Option<&str>,
) -> AccountQuota {
    let mut result_quota = None;

    let mapped_id = match provider_id {
        "minimax-cn" => "minimax",
        "agy" => "antigravity",
        "zcode" | "z.ai" => "zai",
        other => other,
    };

    let adapters = openproxy_adapters::adapters::builtin_adapters();
    if let Some(adapter) = adapters.iter().find(|a| a.id().as_str() == mapped_id)
        && let Some(res) = adapter
            .fetch_quota_with_proxy(
                upstream,
                api_key,
                access_token,
                provider_specific,
                proxy_url,
            )
            .await
    {
        result_quota = Some(res.unwrap_or_else(|e| AccountQuota::with_error(e.to_string())));
    }

    result_quota.unwrap_or_else(|| {
        AccountQuota::with_error(format!(
            "quota fetching not implemented for provider '{provider_id}'"
        ))
    })
}

/// Fetch quota for a single account using the right provider-specific
/// fetcher without an explicit proxy.
pub async fn fetch_account_quota(
    provider_id: &str,
    upstream: &Arc<UpstreamClient>,
    api_key: &str,
    access_token: Option<&str>,
    provider_specific: Option<&str>,
) -> AccountQuota {
    fetch_account_quota_with_proxy(
        provider_id,
        upstream,
        api_key,
        access_token,
        provider_specific,
        None,
    )
    .await
}
