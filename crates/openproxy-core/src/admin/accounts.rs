//! Account and quota administration service layer.

use crate::accounts;
use crate::error::{CoreError, Result};
use crate::ids::{AccountId, ProviderId};
use crate::quota::AccountQuota;
use openproxy_db::secrets::MasterKey;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Inputs for [`create_account`]. The plaintext `api_key` is encrypted via
/// `master_key` before insertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAccountInput {
    pub provider_id: String,
    /// API key for api_key accounts, `None` for OAuth accounts. Accepts `api_key`
    /// or `secret`, since the web UI sends the latter.
    #[serde(alias = "secret")]
    pub api_key: Option<String>,
    pub label: Option<String>,
    pub priority: Option<i32>,
    pub extra_config_json: Option<String>,
}

/// Insert a new account, storing only the encrypted `api_key` BLOB.
///
/// `priority` defaults to `100`, matching the "lower is higher priority"
/// convention in [`crate::accounts`].
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
/// Dedup runs in three phases: decrypt every existing key for `provider_id`,
/// track keys seen in-memory across the batch, then drop duplicates (already in
/// the DB or repeated in the batch), which makes the call idempotent.
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

        // skip keys already in the DB for this provider or seen in this batch
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

/// Accounts, optionally filtered by provider. `master_key` decrypts
/// `oauth_provider_specific`.
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

/// Encrypt and store (or clear) an existing account's API key.
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

/// Update an existing account's label.
pub fn update_account_label(
    conn: &Connection,
    id: AccountId,
    input: &UpdateAccountLabelInput,
) -> Result<()> {
    accounts::update_label(conn, id, input.label.as_deref())
}

/// Decrypted API key for an account.
pub fn get_account_api_key(
    conn: &Connection,
    master_key: &MasterKey,
    id: AccountId,
) -> Result<String> {
    accounts::decrypt_api_key(conn, id, master_key)
}

/// Decrypt an account's API key. The caller drops the connection before any async
/// work. This exists so the quota-refresh path need not repeat the boilerplate.
pub fn decrypt_api_key_for_account(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
) -> Result<String> {
    accounts::decrypt_api_key(conn, id, master_key)
}

/// Stamp a quota snapshot onto an account row. See [`accounts::set_quota`] for the
/// column-level semantics.
pub fn persist_account_quota(conn: &Connection, id: AccountId, q: &AccountQuota) -> Result<()> {
    accounts::set_quota(conn, id, q)
}

/// Account row needed to route a quota refresh. The caller still holds the writer
/// guard on return: call this, drop the guard, then fire the upstream HTTP call.
/// `master_key` decrypts `oauth_provider_specific`.
pub fn account_for_quota_refresh(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
) -> Result<accounts::Account> {
    accounts::get(conn, id, master_key)?.ok_or(CoreError::AccountNotFound(id.0))
}

/// Fetch quota for one account through the provider-specific fetcher, optionally
/// routing auxiliary upstream calls through `proxy_url`.
///
/// MiniMax (and its CN sibling), OpenRouter and Antigravity have fetchers. Any
/// other provider id returns an `AccountQuota` with NULL numeric fields and a
/// `fetch_error` saying the provider is unsupported.
pub use openproxy_oauth::{fetch_account_quota, fetch_account_quota_with_proxy};
