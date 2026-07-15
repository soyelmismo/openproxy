//! Account CRUD. API keys are stored encrypted (BLOB) using a MasterKey.
//!
//! See docs/mvp-spec.md §3 (Provider configuration in the DB) and §8 (schema).
//! The `api_key_encrypted` column is a BLOB produced by [`crate::secrets::MasterKey::encrypt`];
//! plaintext keys never touch the database.

use crate::error::{CoreError, Result};
use crate::ids::{AccountId, ProviderId};
use crate::secrets::MasterKey;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// Health flag tracked per account. `Healthy` is the default; `Degraded` and
/// `Unhealthy` are sticky signals set by the runtime when an account repeatedly
/// fails or returns 429s, and used by combo routing to skip bad accounts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthStatus {
    Healthy,
    Degraded,
    Unhealthy,
}

impl HealthStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "healthy" => Ok(Self::Healthy),
            "degraded" => Ok(Self::Degraded),
            "unhealthy" => Ok(Self::Unhealthy),
            other => Err(CoreError::Validation(format!("invalid health: {}", other))),
        }
    }
}

/// Row view of the `accounts` table. `api_key_encrypted` is intentionally not
/// exposed here — use [`decrypt_api_key`] to obtain the plaintext.
///
/// `quota_*` fields are populated by [`crate::quota::fetch_minimax_quota`]
/// (or similar per-provider fetchers) and stamped onto the row by
/// [`set_quota`]. They are NULL for providers that do not expose a quota
/// endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub provider_id: ProviderId,
    pub label: Option<String>,
    pub priority: i32,
    pub extra_config_json: Option<String>,
    pub health_status: HealthStatus,
    pub rate_limited_until: Option<String>,
    pub quota_session_used: Option<i64>,
    pub quota_session_limit: Option<i64>,
    pub quota_session_reset_at: Option<String>,
    pub quota_weekly_used: Option<i64>,
    pub quota_weekly_limit: Option<i64>,
    pub quota_weekly_reset_at: Option<String>,
    pub quota_plan_name: Option<String>,
    pub quota_last_fetched_at: Option<String>,
    pub quota_fetch_error: Option<String>,
    /// Per-model quota details. Populated by providers that expose
    /// per-model quota (Antigravity family). NULL for providers that
    /// only expose aggregate quota. Stored in the DB as a JSON TEXT
    /// column; deserialized into a `serde_json::Value` (Array) so the
    /// API response sends a proper JSON array, not a stringified one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_model_details: Option<serde_json::Value>,
    /// Account auth method: `api_key` (static key) or `oauth` (OAuth tokens).
    pub auth_type: String,
    /// Email associated with the OAuth account.
    pub email: Option<String>,
    /// OAuth scope string.
    pub oauth_scope: Option<String>,
    /// Provider-specific OAuth metadata (JSON).
    #[serde(skip_serializing)]
    pub oauth_provider_specific: Option<String>,
    /// Token expiry timestamp (ISO-8601). NULL for non-OAuth accounts.
    pub expires_at: Option<String>,
    pub created_at: String,
}

/// Insert a new account. The API key is encrypted with `master_key` before it
/// reaches the database; only the resulting BLOB is stored. Returns the new
/// row's `AccountId`.
///
/// `priority` follows the spec convention: lower number = higher priority
/// (consumed by the priority-strategy router).
///
/// `api_key` may be `None` for OAuth accounts (auth_type = "oauth").
pub fn create(
    conn: &Connection,
    provider_id: &ProviderId,
    api_key: Option<&str>,
    master_key: &MasterKey,
    label: Option<&str>,
    priority: i32,
    extra_config_json: Option<&str>,
) -> Result<AccountId> {
    let blob = if let Some(key) = api_key {
        Some(master_key.encrypt(key)?)
    } else {
        None
    };

    conn.execute(
        "INSERT INTO accounts(provider_id, api_key_encrypted, label, priority, extra_config_json) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            provider_id.as_str(),
            blob,
            label,
            priority,
            extra_config_json,
        ],
    )
    .map_err(|e| {
        // FK violation → unknown provider.
        let msg = e.to_string();
        if msg.contains("FOREIGN KEY") {
            CoreError::Validation(format!("provider_id does not exist: {}", provider_id))
        } else {
            CoreError::Database {
                message: format!("insert account for {}: {}", provider_id, e),
                source: Some(Box::new(e)),
            }
        }
    })?;

    let id: i64 = conn
        .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
        .map_err(crate::error::map_db_error)?;
    Ok(AccountId(id))
}

/// Look up a single account by id. Returns `Ok(None)` when absent.
/// The `master_key` is required to decrypt `oauth_provider_specific`.
pub fn get(conn: &Connection, id: AccountId, master_key: &MasterKey) -> Result<Option<Account>> {
    let row = conn
        .query_row(
            "SELECT id, provider_id, label, priority, extra_config_json, \
                    health_status, rate_limited_until, \
                    quota_session_used, quota_session_limit, quota_session_reset_at, \
                    quota_weekly_used, quota_weekly_limit, quota_weekly_reset_at, \
                    quota_plan_name, quota_last_fetched_at, quota_fetch_error, \
                    quota_model_details, \
                    auth_type, email, oauth_scope, oauth_provider_specific, expires_at, \
                    created_at \
             FROM accounts WHERE id = ?1",
            params![id.0],
            |row| row_to_account(row, master_key),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx(format!(
            "get account {}",
            id.0
        )))?;
    Ok(row)
}

/// List accounts, optionally filtered by provider. Ordered by
/// `(priority ASC, id ASC)` so the highest-priority account comes first; this
/// is the canonical order consumed by the routing layer.
/// The `master_key` is required to decrypt `oauth_provider_specific`.
pub fn list(
    conn: &Connection,
    provider: Option<&ProviderId>,
    master_key: &MasterKey,
) -> Result<Vec<Account>> {
    let sql = match provider {
        Some(_) => {
            "SELECT id, provider_id, label, priority, extra_config_json, \
                    health_status, rate_limited_until, \
                    quota_session_used, quota_session_limit, quota_session_reset_at, \
                    quota_weekly_used, quota_weekly_limit, quota_weekly_reset_at, \
                    quota_plan_name, quota_last_fetched_at, quota_fetch_error, \
                    quota_model_details, \
                    auth_type, email, oauth_scope, oauth_provider_specific, expires_at, \
                    created_at \
             FROM accounts WHERE provider_id = ?1 \
             ORDER BY priority ASC, id ASC"
        }
        None => {
            "SELECT id, provider_id, label, priority, extra_config_json, \
                    health_status, rate_limited_until, \
                    quota_session_used, quota_session_limit, quota_session_reset_at, \
                    quota_weekly_used, quota_weekly_limit, quota_weekly_reset_at, \
                    quota_plan_name, quota_last_fetched_at, quota_fetch_error, \
                    quota_model_details, \
                    auth_type, email, oauth_scope, oauth_provider_specific, expires_at, \
                    created_at \
             FROM accounts \
             ORDER BY priority ASC, id ASC"
        }
    };

    let mut stmt = conn
        .prepare(sql)
        .map_err(crate::error::map_db_error_ctx("list accounts prepare"))?;

    let accounts: Vec<Account> = match provider {
        Some(p) => stmt
            .query_map(params![p.as_str()], |row| row_to_account(row, master_key))
            .map_err(crate::error::map_db_error)?
            .map(|r| r.map_err(crate::error::map_db_error_ctx("list accounts row")))
            .collect::<Result<Vec<Account>>>()?,
        None => stmt
            .query_map(params![], |row| row_to_account(row, master_key))
            .map_err(crate::error::map_db_error)?
            .map(|r| r.map_err(crate::error::map_db_error_ctx("list accounts row")))
            .collect::<Result<Vec<Account>>>()?,
    };
    Ok(accounts)
}

/// Decrypt the stored API key for `id`. Returns [`CoreError::AccountNotFound`]
/// if the account is missing, and a decrypt error (via `MasterKey`) if the
/// stored blob is corrupt or the key has changed.
/// Returns [`CoreError::Validation`] if the account uses OAuth auth (no API key).
pub fn decrypt_api_key(conn: &Connection, id: AccountId, master_key: &MasterKey) -> Result<String> {
    let blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT api_key_encrypted FROM accounts WHERE id = ?1",
            params![id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx(format!(
            "select api_key_encrypted for account {}",
            id.0
        )))?
        .ok_or(CoreError::AccountNotFound(id.0))?;

    let blob = blob
        .ok_or_else(|| CoreError::Validation("account has no API key (OAuth account?)".into()))?;
    master_key.decrypt(&blob)
}

/// Decrypt the API key for `id` AND fetch the account's `label` in
/// a single DB round-trip. The label is needed by URL builders for
/// providers like `cloudflare-workers-ai` that interpolate the
/// account label into the endpoint URL
/// (`/client/v4/accounts/{label}/ai/v1/chat/completions`).
///
/// Returns `(api_key, label)` where `label` is `None` when the
/// account has no label set or when `account_id` is `None`
/// (anonymous / `auth_type = None` providers).
///
/// This was added to fix the bug where Cloudflare chat requests
/// fell through the label-less `build_chat_url` path and hit
/// `__missing_account_label__` in the URL, producing upstream 404s.
/// See the companion change in `Pipeline::execute_single` that
/// calls `build_chat_url_for_account` with the label returned here.
pub fn decrypt_api_key_and_label(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
) -> Result<(String, Option<String>)> {
    let row: Option<(Option<Vec<u8>>, Option<String>)> = conn
        .query_row(
            "SELECT api_key_encrypted, label FROM accounts WHERE id = ?1",
            params![id.0],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx(format!(
            "select api_key+label for account {}",
            id.0
        )))?;
    let (blob, label) = match row {
        Some(r) => r,
        None => return Err(CoreError::AccountNotFound(id.0)),
    };
    let blob = blob
        .ok_or_else(|| CoreError::Validation("account has no API key (OAuth account?)".into()))?;
    let key = master_key.decrypt(&blob)?;
    Ok((key, label))
}

/// Update the `health_status` column. Returns `AccountNotFound` if no row was
/// affected.
pub fn set_health(conn: &Connection, id: AccountId, health: HealthStatus) -> Result<()> {
    let affected = conn
        .execute(
            "UPDATE accounts SET health_status = ?1 WHERE id = ?2",
            params![health.as_str(), id.0],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update health for account {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::AccountNotFound(id.0));
    }
    Ok(())
}

/// Set or clear the `rate_limited_until` timestamp (ISO-8601 string).
/// Passing `None` clears the column. Returns `AccountNotFound` if no row was
/// affected.
pub fn set_rate_limited_until(
    conn: &Connection,
    id: AccountId,
    iso_ts: Option<&str>,
) -> Result<()> {
    let affected = conn
        .execute(
            "UPDATE accounts SET rate_limited_until = ?1 WHERE id = ?2",
            params![iso_ts, id.0],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update rate_limited_until for account {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::AccountNotFound(id.0));
    }
    Ok(())
}

/// Stamp a fresh quota snapshot onto the account row. The `AccountQuota`
/// struct is the one defined in [`crate::quota`]; the fields map 1:1 onto
/// the `quota_*` columns added by migration 000012.
///
/// Every field is written in a single UPDATE so the row stays
/// consistent: a half-written quota snapshot (e.g. session filled but
/// weekly missing) is never observable.
///
/// A failure to find the row surfaces as [`CoreError::AccountNotFound`].
pub fn set_quota(conn: &Connection, id: AccountId, q: &crate::quota::AccountQuota) -> Result<()> {
    // Serialize model_details (per-model quota breakdown) as JSON for
    // storage. NULL when the provider doesn't expose per-model quota.
    let model_details_json: Option<String> = q
        .model_details
        .as_ref()
        .and_then(|d| serde_json::to_string(d).ok())
        .filter(|s| s != "null" && s != "[]");

    let affected = conn
        .execute(
            "UPDATE accounts SET \
                quota_session_used       = ?1, \
                quota_session_limit      = ?2, \
                quota_session_reset_at   = ?3, \
                quota_weekly_used        = ?4, \
                quota_weekly_limit       = ?5, \
                quota_weekly_reset_at    = ?6, \
                quota_plan_name          = ?7, \
                quota_last_fetched_at    = ?8, \
                quota_fetch_error        = ?9, \
                quota_model_details      = ?10 \
             WHERE id = ?11",
            params![
                q.session_used,
                q.session_limit,
                q.session_reset_at,
                q.weekly_used,
                q.weekly_limit,
                q.weekly_reset_at,
                q.plan_name,
                q.last_fetched_at,
                q.fetch_error,
                model_details_json,
                id.0,
            ],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update quota for account {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::AccountNotFound(id.0));
    }
    Ok(())
}

/// Encrypt and store (or clear) the API key for an existing account.
///
/// When `api_key` is `Some`, the plaintext is encrypted with `master_key`
/// and the resulting BLOB is written to the row. When `None`, the column
/// is set to NULL (useful when converting an account to OAuth auth).
///
/// Returns [`CoreError::AccountNotFound`] if no row matches `id`.
pub fn update_api_key(
    conn: &Connection,
    id: AccountId,
    api_key: Option<&str>,
    master_key: &MasterKey,
) -> Result<()> {
    let blob = if let Some(key) = api_key {
        Some(master_key.encrypt(key)?)
    } else {
        None
    };
    let affected = conn
        .execute(
            "UPDATE accounts SET api_key_encrypted = ?1 WHERE id = ?2",
            params![blob, id.0],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "update api_key for account {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::AccountNotFound(id.0));
    }
    Ok(())
}

/// Delete an account by id. Idempotent: a missing id is a no-op (0 rows
/// affected) and not an error, matching the providers module's delete policy.
///
/// FK cleanup: before deleting the account row, we NULL out any
/// `combo_targets.account_id` references. The `combo_targets` table's
/// FK on `account_id` does NOT have `ON DELETE SET NULL` (historical
/// migrations 1/16/25/26/30 all recreate the table without the
/// clause), so a raw `DELETE FROM accounts` fails with
/// `FOREIGN KEY constraint failed` when any combo target still
/// references the account. Setting the FK to NULL makes the combo
/// target fall back to automatic account selection (the default
/// behavior when `account_id` is NULL), which is the correct
/// semantic: deleting an account should not force the operator to
/// re-create or edit every combo that happened to pin it.
pub fn delete(conn: &Connection, id: AccountId) -> Result<()> {
    // Bug fix: NULL out combo_targets.account_id before deleting the
    // account, otherwise the FK constraint blocks the delete.
    conn.execute(
        "UPDATE combo_targets SET account_id = NULL WHERE account_id = ?1",
        params![id.0],
    )
    .map_err(crate::error::map_db_error_ctx(format!(
        "null combo_targets.account_id for account {}",
        id.0
    )))?;
    conn.execute("DELETE FROM accounts WHERE id = ?1", params![id.0])
        .map_err(crate::error::map_db_error_ctx(format!(
            "delete account {}",
            id.0
        )))?;
    Ok(())
}

// =====================================================================
// OAuth token storage / retrieval
// =====================================================================

/// Store encrypted OAuth tokens and metadata on an account row.
///
/// All token fields and `oauth_provider_specific` are encrypted with
/// `master_key` before being written. The `email` and `oauth_scope`
/// columns are stored as plaintext (they are not secrets).
/// Default token lifetime when the upstream omits `expires_in` (1 hour).
const DEFAULT_EXPIRES_IN_SECS: i64 = 3600;

/// Encrypt `oauth_provider_specific` JSON with `master_key` and return
/// base64-encoded ciphertext suitable for storage in a TEXT column.
fn encrypt_oauth_provider_specific(value: &str, master_key: &MasterKey) -> Result<String> {
    let blob = master_key.encrypt(value)?;
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &blob,
    ))
}

/// Decrypt an `oauth_provider_specific` value that was encrypted with
/// [`encrypt_oauth_provider_specific`]. Returns `None` when the input
/// is `None`, or if decryption fails (legacy plaintext data).
pub fn decrypt_oauth_provider_specific(
    encrypted_b64: Option<&str>,
    master_key: &MasterKey,
) -> Option<String> {
    let b64 = encrypted_b64?;
    let blob = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).ok()?;
    master_key.decrypt(&blob).ok()
}

// ponytail: [Demasiados argumentos] -> [Refactorizar a struct en el futuro]
pub fn store_oauth_tokens(
    conn: &Connection,
    id: AccountId,
    access_token: &str,
    refresh_token: Option<&str>,
    master_key: &MasterKey,
    token_type: &str,
    expires_at: Option<&str>,
    scope: Option<&str>,
    provider_specific: Option<&str>,
    email: Option<&str>,
) -> Result<()> {
    let access_blob = master_key.encrypt(access_token)?;
    let refresh_blob = match refresh_token {
        Some(rt) => Some(master_key.encrypt(rt)?),
        None => None,
    };

    // Encrypt oauth_provider_specific with master_key (base64 for TEXT column).
    let provider_specific_encrypted = match provider_specific {
        Some(ps) => Some(encrypt_oauth_provider_specific(ps, master_key)?),
        None => None,
    };

    // Default to 1 hour from now when the upstream omits `expires_in`.
    // Without this the account would never be auto-refreshed because
    // `list_expiring_oauth_accounts` filters on `expires_at IS NOT NULL`.
    let expires_at_owned;
    let expires_at_resolved = match expires_at {
        Some(ts) => Some(ts),
        None => {
            expires_at_owned = (chrono::Utc::now()
                + chrono::Duration::seconds(DEFAULT_EXPIRES_IN_SECS))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
            Some(expires_at_owned.as_str())
        }
    };

    let affected = conn
        .execute(
            "UPDATE accounts SET \
                auth_type = 'oauth', \
                access_token_encrypted = ?1, \
                refresh_token_encrypted = COALESCE(?2, refresh_token_encrypted), \
                token_type = ?3, \
                expires_at = ?4, \
                oauth_scope = COALESCE(?5, oauth_scope), \
                oauth_provider_specific = COALESCE(?6, oauth_provider_specific), \
                email = COALESCE(?7, email) \
             WHERE id = ?8",
            params![
                access_blob,
                refresh_blob,
                token_type,
                expires_at_resolved,
                scope,
                provider_specific_encrypted,
                email,
                id.0,
            ],
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "store_oauth_tokens for account {}",
            id.0
        )))?;
    if affected == 0 {
        return Err(CoreError::AccountNotFound(id.0));
    }
    Ok(())
}

/// Decrypt and return the access token for an account.
pub fn decrypt_access_token(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
) -> Result<String> {
    let blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT access_token_encrypted FROM accounts WHERE id = ?1",
            params![id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx(format!(
            "select access_token for account {}",
            id.0
        )))?
        .ok_or(CoreError::AccountNotFound(id.0))?;

    let blob = blob.ok_or_else(|| {
        CoreError::Validation("account has no access token (not an OAuth account?)".into())
    })?;
    master_key.decrypt(&blob)
}

/// Decrypt and return the refresh token for an account. Returns `None`
/// if the account has no refresh token stored.
pub fn decrypt_refresh_token(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
) -> Result<Option<String>> {
    let blob: Option<Vec<u8>> = conn
        .query_row(
            "SELECT refresh_token_encrypted FROM accounts WHERE id = ?1",
            params![id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx(format!(
            "select refresh_token for account {}",
            id.0
        )))?
        .ok_or(CoreError::AccountNotFound(id.0))?;

    match blob {
        Some(b) => Ok(Some(master_key.decrypt(&b)?)),
        None => Ok(None),
    }
}

/// Return all OAuth accounts whose tokens expire within `within_seconds`
/// from now. Used by the refresh scheduler to proactively refresh tokens.
pub fn list_expiring_oauth_accounts(
    conn: &Connection,
    within_seconds: i64,
    master_key: &MasterKey,
) -> Result<Vec<Account>> {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let threshold = (chrono::Utc::now() + chrono::Duration::seconds(within_seconds))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    let mut stmt = conn
        .prepare(
            "SELECT id, provider_id, label, priority, extra_config_json, \
                    health_status, rate_limited_until, \
                    quota_session_used, quota_session_limit, quota_session_reset_at, \
                    quota_weekly_used, quota_weekly_limit, quota_weekly_reset_at, \
                    quota_plan_name, quota_last_fetched_at, quota_fetch_error, \
                    quota_model_details, \
                    auth_type, email, oauth_scope, oauth_provider_specific, expires_at, \
                    created_at \
             FROM accounts \
             WHERE auth_type = 'oauth' \
               AND expires_at IS NOT NULL \
               AND expires_at <= ?1 \
             ORDER BY priority ASC, id ASC",
        )
        .map_err(crate::error::map_db_error)?;

    let rows = stmt
        .query_map(params![threshold], |row| row_to_account(row, master_key))
        .map_err(crate::error::map_db_error)?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(crate::error::map_db_error)?);
    }

    // Silence the unused-variable lint for the now variable.
    let _ = now;
    Ok(out)
}

/// List the DB row ids of all OAuth accounts (regardless of expiry).
/// Used by the OAuth refresh scheduler's leak-fix prune pass to
/// detect accounts that have been deleted from the DB but still have
/// in-memory tracking entries (`failure_counts`, `last_refresh_attempts`).
pub fn list_oauth_account_ids(conn: &Connection) -> Result<Vec<i64>> {
    let mut stmt = conn
        .prepare("SELECT id FROM accounts WHERE auth_type = 'oauth'")
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, i64>(0))
        .map_err(crate::error::map_db_error)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(crate::error::map_db_error)?);
    }
    Ok(out)
}

/// List the distinct provider ids that currently have at least one
/// OAuth account. Used by the OAuth refresh scheduler's leak-fix
/// prune pass to drop `provider_mutexes` entries for deleted providers.
pub fn list_oauth_provider_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT DISTINCT provider_id FROM accounts WHERE auth_type = 'oauth'")
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(crate::error::map_db_error)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(crate::error::map_db_error)?);
    }
    Ok(out)
}

/// Map a single SELECT row into an `Account`. Shared by `get` and `list`.
/// The `master_key` is required to decrypt `oauth_provider_specific` (and other
/// OAuth token fields if they were stored encrypted).
fn row_to_account(row: &rusqlite::Row<'_>, master_key: &MasterKey) -> rusqlite::Result<Account> {
    let id: i64 = row.get(0)?;
    let provider_id: String = row.get(1)?;
    let label: Option<String> = row.get(2)?;
    let priority: i32 = row.get(3)?;
    let extra_config_json: Option<String> = row.get(4)?;
    let health_status: String = row.get(5)?;
    let rate_limited_until: Option<String> = row.get(6)?;
    let quota_session_used: Option<i64> = row.get(7)?;
    let quota_session_limit: Option<i64> = row.get(8)?;
    let quota_session_reset_at: Option<String> = row.get(9)?;
    let quota_weekly_used: Option<i64> = row.get(10)?;
    let quota_weekly_limit: Option<i64> = row.get(11)?;
    let quota_weekly_reset_at: Option<String> = row.get(12)?;
    let quota_plan_name: Option<String> = row.get(13)?;
    let quota_last_fetched_at: Option<String> = row.get(14)?;
    let quota_fetch_error: Option<String> = row.get(15)?;
    // quota_model_details is stored as a JSON TEXT column. Parse it into
    // a serde_json::Value so the API response sends a proper JSON array.
    let quota_model_details_raw: Option<String> = row.get(16).unwrap_or(None);
    let auth_type: String = row.get(17)?;
    let email: Option<String> = row.get(18)?;
    let oauth_scope: Option<String> = row.get(19)?;
    let oauth_provider_specific_encrypted: Option<String> = row.get(20)?;
    let expires_at: Option<String> = row.get(21)?;
    let created_at: String = row.get(22)?;
    let quota_model_details: Option<serde_json::Value> = quota_model_details_raw
        .as_deref()
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(s).ok());

    let health_status = HealthStatus::parse(&health_status).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::new(FromStrError(format!("{}", e))),
        )
    })?;

    // Decrypt oauth_provider_specific using master_key
    let oauth_provider_specific =
        decrypt_oauth_provider_specific(oauth_provider_specific_encrypted.as_deref(), master_key);

    Ok(Account {
        id: AccountId(id),
        provider_id: ProviderId::new(provider_id),
        label,
        priority,
        extra_config_json,
        health_status,
        rate_limited_until,
        quota_session_used,
        quota_session_limit,
        quota_session_reset_at,
        quota_weekly_used,
        quota_weekly_limit,
        quota_weekly_reset_at,
        quota_plan_name,
        quota_last_fetched_at,
        quota_fetch_error,
        quota_model_details,
        auth_type,
        email,
        oauth_scope,
        oauth_provider_specific,
        expires_at,
        created_at,
    })
}

#[derive(Debug)]
struct FromStrError(String);
impl std::fmt::Display for FromStrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for FromStrError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::conn::DbPool;
    use crate::db::migrations;
    use crate::providers::{self, AuthType, ProviderFormat};
    use std::path::PathBuf;

    /// Build a fresh in-process pool: temp dir on disk, migrations applied,
    /// a provider seeded so account FK constraints can be satisfied.
    fn fresh_pool() -> (DbPool, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("openproxy-accounts-test-{}-{}-{}", pid, nanos, n));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("accounts.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    /// Seed a provider so accounts can be created against it.
    fn seed_provider(conn: &Connection, id: &str) {
        providers::create(
            conn,
            providers::NewProvider {
                id: &ProviderId::new(id),
                name: id,
                base_url: "https://example.com",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: crate::providers::RateLimitScope::Account,
            },
        )
        .expect("seed provider");
    }

    #[test]
    fn create_and_get() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            Some("sk-test-123"),
            &mk,
            Some("primary"),
            10,
            Some(r#"{"org":"acme"}"#),
        )
        .expect("create");

        let acc = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(acc.id, id);
        assert_eq!(acc.provider_id, ProviderId::new("openrouter"));
        assert_eq!(acc.label.as_deref(), Some("primary"));
        assert_eq!(acc.priority, 10);
        assert_eq!(acc.extra_config_json.as_deref(), Some(r#"{"org":"acme"}"#));
        assert_eq!(acc.health_status, HealthStatus::Healthy);
        assert!(acc.rate_limited_until.is_none());
        assert!(!acc.created_at.is_empty(), "DB stamps created_at");
        assert_eq!(acc.auth_type, "api_key", "default auth_type");

        // Missing id → None, not error.
        assert!(
            get(&conn, AccountId(9999), &mk)
                .expect("get missing")
                .is_none()
        );
    }

    #[test]
    fn create_encrypts_api_key_at_rest() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let plaintext = "sk-supersecret-DO-NOT-LEAK-9f8a";
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            Some(plaintext),
            &mk,
            None,
            100,
            None,
        )
        .expect("create");

        // Read the raw BLOB straight out of SQLite, bypass the typed API.
        let raw: Vec<u8> = conn
            .query_row(
                "SELECT api_key_encrypted FROM accounts WHERE id = ?1",
                params![id.0],
                |r| r.get(0),
            )
            .expect("select blob");

        // The plaintext must not appear anywhere in the ciphertext bytes.
        let raw_str = String::from_utf8_lossy(&raw);
        assert!(
            !raw_str.contains(plaintext),
            "plaintext must not appear in stored blob"
        );
        // And the blob must be at least nonce + tag (12 + 16 bytes) long.
        assert!(raw.len() >= 28, "blob too small: {} bytes", raw.len());
    }

    #[test]
    fn decrypt_api_key_roundtrip() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let plaintext = "sk-roundtrip-xyz";
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            Some(plaintext),
            &mk,
            None,
            100,
            None,
        )
        .expect("create");

        let recovered = decrypt_api_key(&conn, id, &mk).expect("decrypt");
        assert_eq!(recovered, plaintext);

        // Missing id → AccountNotFound.
        let err = decrypt_api_key(&conn, AccountId(424242), &mk).expect_err("missing");
        assert!(matches!(err, CoreError::AccountNotFound(424242)));

        // Wrong key → decryption failure (Internal).
        let other = MasterKey::generate();
        let err = decrypt_api_key(&conn, id, &other).expect_err("wrong key");
        assert!(matches!(err, CoreError::Internal(_)));
    }

    #[test]
    fn list_filters_by_provider() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_provider(&conn, "anthropic");

        let mk = MasterKey::generate();
        for (pid, prio) in [("openrouter", 10), ("openrouter", 20), ("anthropic", 5)] {
            create(
                &conn,
                &ProviderId::new(pid),
                Some("sk-x"),
                &mk,
                None,
                prio,
                None,
            )
            .expect("create");
        }

        let all = list(&conn, None, &mk).expect("list all");
        assert_eq!(all.len(), 3);

        let only_or = list(&conn, Some(&ProviderId::new("openrouter")), &mk).expect("list or");
        assert_eq!(only_or.len(), 2);
        // Ordered by priority ASC.
        assert_eq!(only_or[0].priority, 10);
        assert_eq!(only_or[1].priority, 20);
        for a in &only_or {
            assert_eq!(a.provider_id, ProviderId::new("openrouter"));
        }

        let only_an = list(&conn, Some(&ProviderId::new("anthropic")), &mk).expect("list an");
        assert_eq!(only_an.len(), 1);
        assert_eq!(only_an[0].provider_id, ProviderId::new("anthropic"));

        let none = list(&conn, Some(&ProviderId::new("nope")), &mk).expect("list nope");
        assert!(none.is_empty());
    }

    #[test]
    fn set_health_updates_status() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            Some("sk-x"),
            &mk,
            None,
            100,
            None,
        )
        .expect("create");

        set_health(&conn, id, HealthStatus::Degraded).expect("set degraded");
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(a.health_status, HealthStatus::Degraded);

        set_health(&conn, id, HealthStatus::Unhealthy).expect("set unhealthy");
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(a.health_status, HealthStatus::Unhealthy);

        set_health(&conn, id, HealthStatus::Healthy).expect("back to healthy");
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(a.health_status, HealthStatus::Healthy);

        // Missing id → AccountNotFound.
        let err = set_health(&conn, AccountId(7777), HealthStatus::Healthy).expect_err("missing");
        assert!(matches!(err, CoreError::AccountNotFound(7777)));
    }

    #[test]
    fn set_rate_limited_updates() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            Some("sk-x"),
            &mk,
            None,
            100,
            None,
        )
        .expect("create");

        // Initially None.
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert!(a.rate_limited_until.is_none());

        set_rate_limited_until(&conn, id, Some("2026-06-13T12:34:56Z")).expect("set");
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(
            a.rate_limited_until.as_deref(),
            Some("2026-06-13T12:34:56Z")
        );

        // Clear with None.
        set_rate_limited_until(&conn, id, None).expect("clear");
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert!(a.rate_limited_until.is_none());

        // Missing id → AccountNotFound.
        let err = set_rate_limited_until(&conn, AccountId(12321), Some("x")).expect_err("missing");
        assert!(matches!(err, CoreError::AccountNotFound(12321)));
    }

    #[test]
    fn delete_removes_account() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            Some("sk-x"),
            &mk,
            None,
            100,
            None,
        )
        .expect("create");
        assert!(get(&conn, id, &mk).expect("get").is_some());

        delete(&conn, id).expect("delete");
        assert!(get(&conn, id, &mk).expect("get after delete").is_none());

        // Idempotent: a second delete is a no-op, not an error.
        delete(&conn, id).expect("delete again is fine");
    }

    #[test]
    fn set_quota_roundtrip() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "minimax");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("minimax"),
            Some("sk-quota"),
            &mk,
            Some("quota-test"),
            10,
            None,
        )
        .expect("create");

        // Initially: every quota_* column is NULL.
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert!(a.quota_session_used.is_none());
        assert!(a.quota_session_limit.is_none());
        assert!(a.quota_session_reset_at.is_none());
        assert!(a.quota_weekly_used.is_none());
        assert!(a.quota_weekly_limit.is_none());
        assert!(a.quota_weekly_reset_at.is_none());
        assert!(a.quota_plan_name.is_none());
        assert!(a.quota_last_fetched_at.is_none());
        assert!(a.quota_fetch_error.is_none());

        // Stamp a snapshot.
        let q = crate::quota::AccountQuota {
            session_used: Some(1234),
            session_limit: Some(5000),
            session_reset_at: Some("1700000000".into()),
            weekly_used: Some(80000),
            weekly_limit: Some(500000),
            weekly_reset_at: Some("1700003600".into()),
            plan_name: Some("Coding Plan".into()),
            last_fetched_at: "1700000001".into(),
            fetch_error: None,
            model_details: None,
        };
        set_quota(&conn, id, &q).expect("set_quota");

        // Re-read: every field survives the round-trip.
        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(a.quota_session_used, Some(1234));
        assert_eq!(a.quota_session_limit, Some(5000));
        assert_eq!(a.quota_session_reset_at.as_deref(), Some("1700000000"));
        assert_eq!(a.quota_weekly_used, Some(80000));
        assert_eq!(a.quota_weekly_limit, Some(500000));
        assert_eq!(a.quota_weekly_reset_at.as_deref(), Some("1700003600"));
        assert_eq!(a.quota_plan_name.as_deref(), Some("Coding Plan"));
        assert_eq!(a.quota_last_fetched_at.as_deref(), Some("1700000001"));
        assert!(a.quota_fetch_error.is_none());

        // Also visible through `list`.
        let all = list(&conn, Some(&ProviderId::new("minimax")), &mk).expect("list");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].quota_session_used, Some(1234));
    }

    #[test]
    fn set_quota_records_error() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "minimax");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("minimax"),
            Some("sk-x"),
            &mk,
            None,
            100,
            None,
        )
        .expect("create");

        // A failed quota fetch: all numeric fields stay None, the
        // error message is stamped on, and last_fetched_at is set so
        // the UI can distinguish "tried" from "never tried".
        let q = crate::quota::AccountQuota {
            session_used: None,
            session_limit: None,
            session_reset_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: None,
            last_fetched_at: "1700000099".into(),
            fetch_error: Some("minimax 401".into()),
            model_details: None,
        };
        set_quota(&conn, id, &q).expect("set_quota");

        let a = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(a.quota_fetch_error.as_deref(), Some("minimax 401"));
        assert_eq!(a.quota_last_fetched_at.as_deref(), Some("1700000099"));
        assert!(a.quota_session_used.is_none());
    }

    #[test]
    fn set_quota_missing_account_errors() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "minimax");

        let q = crate::quota::AccountQuota {
            session_used: None,
            session_limit: None,
            session_reset_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: None,
            last_fetched_at: "0".into(),
            fetch_error: None,
            model_details: None,
        };
        let err = set_quota(&conn, AccountId(99999), &q).expect_err("missing");
        assert!(matches!(err, CoreError::AccountNotFound(99999)));
    }

    #[test]
    fn health_status_parse_roundtrip() {
        for (variant, s) in [
            (HealthStatus::Healthy, "healthy"),
            (HealthStatus::Degraded, "degraded"),
            (HealthStatus::Unhealthy, "unhealthy"),
        ] {
            assert_eq!(variant.as_str(), s);
            assert_eq!(HealthStatus::parse(s).expect("parse"), variant);
        }
        assert!(HealthStatus::parse("bogus").is_err());
    }

    // =====================================================================
    // OAuth token encrypt/decrypt roundtrip tests
    // =====================================================================

    #[test]
    fn oauth_access_token_roundtrip() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        let access = "ya29.a0AfH6SMB_test-access-token_12345";
        store_oauth_tokens(
            &conn, id, access, None, &mk, "Bearer", None, None, None, None,
        )
        .expect("store");

        let decrypted = decrypt_access_token(&conn, id, &mk).expect("decrypt");
        assert_eq!(decrypted, access);
    }

    #[test]
    fn oauth_refresh_token_roundtrip() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        let access = "ya29.access";
        let refresh = "1//0test-refresh-token_xyz";
        store_oauth_tokens(
            &conn,
            id,
            access,
            Some(refresh),
            &mk,
            "Bearer",
            None,
            None,
            None,
            None,
        )
        .expect("store");

        let decrypted_rt = decrypt_refresh_token(&conn, id, &mk).expect("decrypt refresh");
        assert_eq!(decrypted_rt.as_deref(), Some(refresh));
    }

    #[test]
    fn oauth_no_refresh_token_returns_none() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        // Store with no refresh token.
        store_oauth_tokens(
            &conn,
            id,
            "access-only",
            None,
            &mk,
            "Bearer",
            None,
            None,
            None,
            None,
        )
        .expect("store");

        let rt = decrypt_refresh_token(&conn, id, &mk).expect("decrypt");
        assert!(rt.is_none());
    }

    #[test]
    fn oauth_empty_access_token() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        store_oauth_tokens(&conn, id, "", None, &mk, "Bearer", None, None, None, None)
            .expect("store empty access token");

        let decrypted = decrypt_access_token(&conn, id, &mk).expect("decrypt");
        assert_eq!(decrypted, "");
    }

    #[test]
    fn oauth_empty_refresh_token() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        store_oauth_tokens(
            &conn,
            id,
            "access",
            Some(""),
            &mk,
            "Bearer",
            None,
            None,
            None,
            None,
        )
        .expect("store");

        let rt = decrypt_refresh_token(&conn, id, &mk).expect("decrypt");
        assert_eq!(rt.as_deref(), Some(""));
    }

    #[test]
    fn oauth_very_long_tokens() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        let long_access = "a".repeat(10_000);
        let long_refresh = "r".repeat(10_000);
        store_oauth_tokens(
            &conn,
            id,
            &long_access,
            Some(&long_refresh),
            &mk,
            "Bearer",
            None,
            None,
            None,
            None,
        )
        .expect("store long tokens");

        let decrypted_a = decrypt_access_token(&conn, id, &mk).expect("decrypt access");
        assert_eq!(decrypted_a, long_access);

        let decrypted_r = decrypt_refresh_token(&conn, id, &mk).expect("decrypt refresh");
        assert_eq!(decrypted_r.as_deref(), Some(long_refresh.as_str()));
    }

    #[test]
    fn oauth_unicode_tokens() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        let unicode_access = "tok_日本語🔑_emoji_🎉_ñ";
        store_oauth_tokens(
            &conn,
            id,
            unicode_access,
            None,
            &mk,
            "Bearer",
            None,
            None,
            None,
            None,
        )
        .expect("store unicode");

        let decrypted = decrypt_access_token(&conn, id, &mk).expect("decrypt");
        assert_eq!(decrypted, unicode_access);
    }

    #[test]
    fn oauth_wrong_key_fails_decrypt() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        store_oauth_tokens(
            &conn,
            id,
            "secret-token",
            Some("secret-refresh"),
            &mk,
            "Bearer",
            None,
            None,
            None,
            None,
        )
        .expect("store");

        let wrong_mk = MasterKey::generate();
        let err = decrypt_access_token(&conn, id, &wrong_mk).unwrap_err();
        assert!(matches!(err, CoreError::Internal(_)));
    }

    #[test]
    fn oauth_access_token_on_missing_account() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let mk = MasterKey::generate();
        let err = decrypt_access_token(&conn, AccountId(99999), &mk).unwrap_err();
        assert!(matches!(err, CoreError::AccountNotFound(99999)));
    }

    #[test]
    fn oauth_refresh_token_on_missing_account() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let mk = MasterKey::generate();
        let err = decrypt_refresh_token(&conn, AccountId(99999), &mk).unwrap_err();
        assert!(matches!(err, CoreError::AccountNotFound(99999)));
    }

    #[test]
    fn oauth_store_tokens_on_missing_account() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let mk = MasterKey::generate();
        let err = store_oauth_tokens(
            &conn,
            AccountId(99999),
            "access",
            None,
            &mk,
            "Bearer",
            None,
            None,
            None,
            None,
        )
        .unwrap_err();
        assert!(matches!(err, CoreError::AccountNotFound(99999)));
    }

    #[test]
    fn oauth_replacing_tokens_overwrites_old() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        // First store.
        store_oauth_tokens(
            &conn,
            id,
            "old-access",
            Some("old-refresh"),
            &mk,
            "Bearer",
            None,
            None,
            None,
            None,
        )
        .expect("store1");

        // Overwrite.
        store_oauth_tokens(
            &conn,
            id,
            "new-access",
            Some("new-refresh"),
            &mk,
            "Bearer",
            None,
            None,
            None,
            None,
        )
        .expect("store2");

        assert_eq!(decrypt_access_token(&conn, id, &mk).unwrap(), "new-access");
        assert_eq!(
            decrypt_refresh_token(&conn, id, &mk).unwrap().as_deref(),
            Some("new-refresh")
        );
    }

    #[test]
    fn store_oauth_tokens_defaults_expires_at_when_none() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        // Store with expires_at = None.
        store_oauth_tokens(
            &conn,
            id,
            "access-token",
            Some("refresh-token"),
            &mk,
            "Bearer",
            None, // expires_at intentionally omitted
            None,
            None,
            None,
        )
        .expect("store");

        // The account should now have a non-NULL expires_at ~1 hour in the future.
        let acc = get(&conn, id, &mk).expect("get").expect("present");
        let expires = acc.expires_at.expect("expires_at should be populated");
        let parsed = chrono::DateTime::parse_from_rfc3339(&expires)
            .expect("valid ISO-8601")
            .with_timezone(&chrono::Utc);
        let now = chrono::Utc::now();
        let diff = parsed.signed_duration_since(now);
        assert!(
            diff.num_seconds() > 3500 && diff.num_seconds() <= 3600,
            "expires_at should be ~1 hour from now, got {}s",
            diff.num_seconds()
        );
    }

    #[test]
    fn store_oauth_tokens_preserves_explicit_expires_at() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        let explicit = "2099-01-01T00:00:00Z";
        store_oauth_tokens(
            &conn,
            id,
            "access-token",
            Some("refresh-token"),
            &mk,
            "Bearer",
            Some(explicit),
            None,
            None,
            None,
        )
        .expect("store");

        let acc = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(acc.expires_at.as_deref(), Some(explicit));
    }

    #[test]
    fn decrypt_api_key_on_oauth_account_returns_validation_error() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        // OAuth account: api_key = None → api_key_encrypted = NULL in DB.
        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        let err = decrypt_api_key(&conn, id, &mk).expect_err("OAuth account has no key");
        assert!(
            matches!(err, CoreError::Validation(ref msg) if msg.contains("no API key")),
            "expected Validation error about missing API key, got: {:?}",
            err
        );
    }

    #[test]
    fn update_api_key_roundtrip() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        // Initially no key (OAuth account).
        let err = decrypt_api_key(&conn, id, &mk).expect_err("no key yet");
        assert!(matches!(err, CoreError::Validation(_)));

        // Set a key.
        let key = "sk-updated-key-abc123";
        update_api_key(&conn, id, Some(key), &mk).expect("set key");
        let recovered = decrypt_api_key(&conn, id, &mk).expect("decrypt after set");
        assert_eq!(recovered, key);

        // Clear the key (back to OAuth).
        update_api_key(&conn, id, None, &mk).expect("clear key");
        let err = decrypt_api_key(&conn, id, &mk).expect_err("cleared");
        assert!(matches!(err, CoreError::Validation(_)));

        // Missing id → AccountNotFound.
        let err =
            update_api_key(&conn, AccountId(99999), Some("x"), &mk).expect_err("missing account");
        assert!(matches!(err, CoreError::AccountNotFound(99999)));
    }

    #[test]
    fn delete_account_nulls_combo_targets_fk() {
        // Bug: deleting an account that is referenced by combo_targets
        // failed with FOREIGN KEY constraint failed because the FK
        // on combo_targets.account_id does NOT have ON DELETE SET NULL.
        // Fix: accounts::delete now NULLs out combo_targets.account_id
        // before deleting the account row.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "minimax");

        let mk = MasterKey::generate();
        let account_id = create(
            &conn,
            &ProviderId::new("minimax"),
            Some("sk-test-minimax"),
            &mk,
            Some("primary"),
            10,
            None,
        )
        .expect("create account");

        // Create a combo with a target that pins this account.
        conn.execute(
            "INSERT INTO combos (id, name, strategy, race_size) VALUES (1, 'test-combo', 'priority', 1)",
            [],
        )
        .expect("insert combo");
        conn.execute(
            "INSERT INTO combo_targets (id, combo_id, provider_id, account_id, upstream_model_id, priority_order) \
             VALUES (1, 1, 'minimax', ?1, 'model-1', 0)",
            params![account_id.0],
        )
        .expect("insert combo_target with account_id");

        // Verify the combo_target references the account.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM combo_targets WHERE account_id = ?1",
                params![account_id.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "combo_target should reference the account");

        // Delete the account — must NOT fail with FK constraint error.
        delete(&conn, account_id).expect("delete account with combo_target reference");

        // The account is gone.
        let gone = get(&conn, account_id, &mk).expect("get").is_none();
        assert!(gone, "account should be deleted");

        // The combo_target still exists, but account_id is now NULL
        // (falls back to automatic account selection).
        let target_account_id: Option<i64> = conn
            .query_row(
                "SELECT account_id FROM combo_targets WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            target_account_id.is_none(),
            "combo_target.account_id should be NULL after account delete"
        );
    }

    #[test]
    fn oauth_store_preserves_existing_refresh_token_when_none() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");

        let mk = MasterKey::generate();
        let id = create(
            &conn,
            &ProviderId::new("openrouter"),
            None,
            &mk,
            None,
            10,
            None,
        )
        .expect("create");

        // 1. Initial store with a refresh token.
        let access1 = "access-1";
        let refresh1 = "refresh-1";
        store_oauth_tokens(
            &conn,
            id,
            access1,
            Some(refresh1),
            &mk,
            "Bearer",
            None,
            None,
            Some("initial-provider-spec"),
            Some("user@domain.com"),
        )
        .expect("store initial");

        // 2. Perform a refresh passing None for refresh_token and other fields.
        let access2 = "access-2";
        store_oauth_tokens(
            &conn, id, access2, None, &mk, "Bearer", None, None, None, None,
        )
        .expect("store refresh");

        // 3. Verify access_token is updated.
        let decrypted_at = decrypt_access_token(&conn, id, &mk).expect("decrypt access");
        assert_eq!(decrypted_at, access2);

        // 4. Verify refresh_token is preserved.
        let decrypted_rt = decrypt_refresh_token(&conn, id, &mk).expect("decrypt refresh");
        assert_eq!(decrypted_rt.as_deref(), Some(refresh1));

        // 5. Verify email and provider specific metadata are preserved.
        let acc = get(&conn, id, &mk).expect("get").expect("present");
        assert_eq!(acc.email.as_deref(), Some("user@domain.com"));
        assert_eq!(
            acc.oauth_provider_specific.as_deref(),
            Some("initial-provider-spec")
        );
    }
}
