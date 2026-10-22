//! Account CRUD and key management.

use std::collections::HashSet;

use crate::secrets::MasterKey;
use openproxy_types::accounts::Account;
use openproxy_types::{AccountId, CoreError, HealthStatus, ProviderId, Result};
use rusqlite::{Connection, OptionalExtension, params};

crate::def_table_select!(account_api_key_select, "accounts", "api_key_encrypted");

crate::def_table_select!(
    account_api_key_label_select,
    "accounts",
    "api_key_encrypted, label"
);

crate::def_table_select!(account_current_proxy_select, "accounts", "current_proxy_id");

crate::def_table_select!(account_id_select, "accounts", "id");

/// Generate a short prefix-suffix summary for an API key (e.g. `sk-pro...7654`).
pub fn summarize_api_key(key: &str) -> String {
    let trimmed = key.trim();
    let char_count = trimmed.chars().count();
    if char_count <= 10 {
        return trimmed.to_string();
    }
    let prefix: String = trimmed.chars().take(6).collect();
    let suffix: String = trimmed.chars().skip(char_count.saturating_sub(4)).collect();
    format!("{prefix}...{suffix}")
}

pub fn create(
    conn: &Connection,
    provider_id: &ProviderId,
    api_key: Option<&str>,
    master_key: &MasterKey,
    label: Option<&str>,
    priority: i32,
    extra_config_json: Option<&str>,
) -> Result<AccountId> {
    let blob = api_key.map(|key| master_key.encrypt(key)).transpose()?;

    let result = conn.execute(
        "INSERT INTO accounts(provider_id, api_key_encrypted, label, priority, extra_config_json) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            provider_id.as_str(),
            blob,
            label,
            priority,
            extra_config_json,
        ],
    );

    match result {
        Ok(_) => {
            let rowid = conn.last_insert_rowid();
            Ok(AccountId(rowid))
        }
        Err(e) => {
            if crate::error::classify_sqlite_error(&e)
                == crate::error::DbErrorKind::ForeignKeyViolation
            {
                Err(CoreError::Validation("unknown provider".into()))
            } else {
                Err(crate::error::map_db_error_ctx(format!(
                    "insert account for provider {provider_id}"
                ))(e))
            }
        }
    }
}

/// Priority step used by [`next_priority_for_provider`] and
/// [`reorder_accounts`], keeping the "lower is higher priority" convention with
/// gaps so a single insert never collides with an existing row.
pub const PRIORITY_STEP: i32 = 10;

/// Next priority for a new account of `provider_id`: the highest existing
/// priority plus one step, or the base step when the provider has no accounts
/// yet. Guarantees the new account lands last instead of sharing a value with
/// its siblings.
pub fn next_priority_for_provider(conn: &Connection, provider_id: &ProviderId) -> Result<i32> {
    let max: Option<i32> = conn
        .query_row(
            "SELECT MAX(priority) FROM accounts WHERE provider_id = ?1",
            params![provider_id.as_str()],
            |row| row.get(0),
        )
        .map_err(|e| {
            crate::error::map_db_error_ctx(format!(
                "select max priority for provider {provider_id}"
            ))(e)
        })?;
    Ok(max.map_or(PRIORITY_STEP, |m| m.saturating_add(PRIORITY_STEP)))
}

pub fn get(conn: &Connection, id: AccountId, master_key: &MasterKey) -> Result<Option<Account>> {
    crate::db_query_one!(
        conn,
        account_select!("WHERE id = ?1"),
        params![id.0],
        |row| row_to_account(row, master_key),
        format!("get account {}", id.0)
    )
}

pub fn list(
    conn: &Connection,
    provider: Option<&ProviderId>,
    master_key: &MasterKey,
) -> Result<Vec<Account>> {
    let sql = match provider {
        Some(_) => account_select!("WHERE provider_id = ?1 ORDER BY priority ASC, id ASC"),
        None => account_select!("ORDER BY priority ASC, id ASC"),
    };

    crate::db_query_all!(
        conn,
        sql,
        rusqlite::params_from_iter(provider.map(|p| p.as_str())),
        |row| row_to_account(row, master_key),
        "list accounts"
    )
}

/// Returns all decrypted API keys currently associated with `provider_id` as a `HashSet<String>`.
/// Accounts with NULL or un-decryptable keys are ignored.
pub fn list_api_keys_for_provider(
    conn: &Connection,
    provider_id: &ProviderId,
    master_key: &MasterKey,
) -> Result<HashSet<String>> {
    let blobs: Vec<Vec<u8>> = crate::db_query_all!(
        conn,
        account_api_key_select!("WHERE provider_id = ?1 AND api_key_encrypted IS NOT NULL"),
        params![provider_id.as_str()],
        |row| row.get(0),
        "list api key blobs for provider"
    )?;

    let mut keys = HashSet::with_capacity(blobs.len());
    for blob in blobs {
        if let Ok(key) = master_key.decrypt(&blob) {
            keys.insert(key);
        }
    }
    Ok(keys)
}

pub fn decrypt_api_key(conn: &Connection, id: AccountId, master_key: &MasterKey) -> Result<String> {
    let blob: Option<Vec<u8>> = conn
        .query_row(
            account_api_key_select!("WHERE id = ?1"),
            params![id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| crate::error::map_db_error_ctx(format!(
            "select api_key_encrypted for account {}",
            id.0
        ))(e))?
        .ok_or(CoreError::AccountNotFound(id.0))?;

    let blob = blob
        .ok_or_else(|| CoreError::Validation("account has no API key (OAuth account?)".into()))?;
    master_key.decrypt(&blob)
}

pub fn decrypt_api_key_and_label(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
) -> Result<(String, Option<String>)> {
    let row: Option<(Option<Vec<u8>>, Option<String>)> = conn
        .query_row(
            account_api_key_label_select!("WHERE id = ?1"),
            params![id.0],
            |r| crate::map_row_tuple!(r => (0, 1)),
        )
        .optional()
        .map_err(|e| {
            crate::error::map_db_error_ctx(format!("select api_key+label for account {}", id.0))(e)
        })?;
    let Some((blob, label)) = row else {
        return Err(CoreError::AccountNotFound(id.0));
    };
    let blob = blob
        .ok_or_else(|| CoreError::Validation("account has no API key (OAuth account?)".into()))?;
    let key = master_key.decrypt(&blob)?;
    Ok((key, label))
}

pub fn set_health(conn: &Connection, id: AccountId, health: HealthStatus) -> Result<()> {
    set_health_with_error(conn, id, health, None)
}

pub fn set_health_with_error(
    conn: &Connection,
    id: AccountId,
    health: HealthStatus,
    error: Option<&str>,
) -> Result<()> {
    crate::crud::execute_check_affected(
        conn,
        "UPDATE accounts SET health_status = ?1, quota_fetch_error = ?2, rate_limited_until = NULL WHERE id = ?3",
        params![health.as_str(), error, id.0],
        format!("update health for account {}", id.0),
        || CoreError::AccountNotFound(id.0),
    )
}

pub fn set_rate_limited_until(
    conn: &Connection,
    id: AccountId,
    iso_ts: Option<&str>,
) -> Result<()> {
    let affected = crate::db_update_field!(
        conn,
        "accounts",
        rate_limited_until = iso_ts,
        WHERE id = id.0,
        format!("update rate_limited_until for account {}", id.0)
    )?;
    crate::crud::check_affected(affected, || CoreError::AccountNotFound(id.0))
}

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
    crate::crud::execute_check_affected(
        conn,
        "UPDATE accounts SET \
            api_key_encrypted = ?1, \
            health_status = 'healthy', \
            rate_limited_until = NULL, \
            quota_fetch_error = NULL \
         WHERE id = ?2",
        params![blob, id.0],
        format!("update api_key for account {}", id.0),
        || CoreError::AccountNotFound(id.0),
    )
}

pub fn update_label(conn: &Connection, id: AccountId, label: Option<&str>) -> Result<()> {
    let affected = crate::db_update_field!(
        conn,
        "accounts",
        label = label,
        WHERE id = id.0,
        format!("update label for account {}", id.0)
    )?;
    crate::crud::check_affected(affected, || CoreError::AccountNotFound(id.0))
}

pub fn delete(conn: &Connection, id: AccountId) -> Result<()> {
    conn.execute(
        "UPDATE combo_targets SET account_id = NULL WHERE account_id = ?1",
        params![id.0],
    )
    .map_err(|e| {
        crate::error::map_db_error_ctx(format!(
            "null combo_targets.account_id for account {}",
            id.0
        ))(e)
    })?;
    crate::db_delete_by_id!(conn, "accounts", id.0, format!("delete account {}", id.0))?;
    Ok(())
}

/// Reorder the accounts of `provider_id`, rewriting `priority` from the
/// submitted order: the first id receives `PRIORITY_STEP`, the next
/// `2 * PRIORITY_STEP`, and so on, matching the `ORDER BY priority ASC, id ASC`
/// convention of [`list`].
///
/// `ids` must be exactly the set of the provider's account ids — a partial
/// list is rejected rather than silently reordering a subset and leaving
/// duplicates behind.
pub fn reorder_accounts(
    conn: &Connection,
    provider_id: &ProviderId,
    ids: &[AccountId],
) -> Result<()> {
    crate::error::with_busy_retry("reorder_accounts", || {
        let existing: Vec<i64> = crate::db_query_all!(
            conn,
            account_id_select!("WHERE provider_id = ?1"),
            params![provider_id.as_str()],
            |row| row.get(0),
            "list account ids for reorder"
        )?;

        let mut seen = std::collections::HashSet::with_capacity(ids.len());
        for id in ids {
            if !seen.insert(id.0) {
                return Err(CoreError::Validation(format!(
                    "duplicate account id {} in reorder",
                    id.0
                )));
            }
            if !existing.contains(&id.0) {
                return Err(CoreError::Validation(format!(
                    "account {} does not belong to provider {}",
                    id.0, provider_id
                )));
            }
        }
        if seen.len() != existing.len() {
            return Err(CoreError::Validation(format!(
                "reorder expects {} account ids for provider {}, got {}",
                existing.len(),
                provider_id,
                seen.len()
            )));
        }

        let tx = conn
            .unchecked_transaction()
            .map_err(crate::error::map_db_error)?;
        {
            let mut stmt = tx
                .prepare_cached("UPDATE accounts SET priority = ?1 WHERE id = ?2")
                .map_err(crate::error::map_db_error)?;
            for (i, id) in ids.iter().enumerate() {
                let priority = ((i as i64 + 1) * PRIORITY_STEP as i64) as i32;
                stmt.execute(params![priority, id.0]).map_err(|e| {
                    crate::error::map_db_error_ctx(format!("set priority for account {}", id.0))(e)
                })?;
            }
        }
        tx.commit().map_err(crate::error::map_db_error)?;
        Ok(())
    })
}

pub fn update_current_proxy(
    conn: &Connection,
    id: AccountId,
    proxy_id: Option<&str>,
) -> Result<()> {
    let affected = crate::db_update_field!(
        conn,
        "accounts",
        current_proxy_id = proxy_id,
        WHERE id = id.0,
        format!("update current_proxy_id for account {}", id.0)
    )?;
    crate::crud::check_affected(affected, || CoreError::AccountNotFound(id.0))
}

pub fn get_current_proxy_id(conn: &Connection, account_id: AccountId) -> Result<Option<String>> {
    conn.query_row(
        account_current_proxy_select!("WHERE id = ?1"),
        params![account_id.0],
        |row| row.get(0),
    )
    .optional()
    .map(|opt| opt.flatten())
    .map_err(crate::error::map_db_error_ctx("get current_proxy_id"))
}

pub fn clear_current_proxy_id(conn: &Connection, account_id: AccountId) -> Result<()> {
    conn.execute(
        "UPDATE accounts SET current_proxy_id = NULL WHERE id = ?1",
        params![account_id.0],
    )
    .map_err(crate::error::map_db_error_ctx("clear current_proxy_id"))?;
    Ok(())
}

pub(crate) fn row_to_account(
    row: &rusqlite::Row<'_>,
    master_key: &MasterKey,
) -> rusqlite::Result<Account> {
    let oauth_provider_specific_encrypted: Option<String> = row.get(20)?;
    let oauth_provider_specific = super::oauth::decrypt_oauth_provider_specific(
        oauth_provider_specific_encrypted.as_deref(),
        master_key,
    )
    .map(String::into_boxed_str);

    crate::map_row_struct!(row, Account {
        id: @id(0, AccountId),
        provider_id: @id_str(1, ProviderId),
        label: @opt_box_str(2),
        priority: 3,
        extra_config_json: @opt_box_str(4),
        health_status: @enum_parse(5, HealthStatus),
        rate_limited_until: @opt_box_str(6),
        quota_session_used: 7,
        quota_session_limit: 8,
        quota_session_reset_at: @opt_box_str(9),
        quota_weekly_used: 10,
        quota_weekly_limit: 11,
        quota_weekly_reset_at: @opt_box_str(12),
        quota_plan_name: @opt_box_str(13),
        quota_last_fetched_at: @opt_box_str(14),
        quota_fetch_error: @opt_box_str(15),
        quota_model_details: @json(16),
        auth_type: @box_str(17),
        email: @opt_box_str(18),
        oauth_scope: @opt_box_str(19),
        oauth_provider_specific: @expr(oauth_provider_specific),
        expires_at: @opt_box_str(21),
        created_at: @box_str(22),
        current_proxy_id: @opt_box_str(23),
        quota_pools: @json(24),
    })
}
