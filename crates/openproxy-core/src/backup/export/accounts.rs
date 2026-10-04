//! Account backup export and credential decryption helpers.

use openproxy_db::MasterKey;
use openproxy_db::error::map_db_error;
use openproxy_types::backup::BackupAccount;
use openproxy_types::{ProviderId, Result};
use rusqlite::Connection;

/// Decrypt an encrypted BLOB using [`MasterKey`], ignoring errors defensively.
fn decrypt_blob_opt(master_key: &MasterKey, blob_opt: Option<Vec<u8>>) -> Option<String> {
    let blob = blob_opt?;
    if blob.is_empty() {
        return None;
    }
    master_key.decrypt(&blob).ok()
}

/// Decrypt an OAuth token encrypted BLOB.
fn decrypt_token_opt(master_key: &MasterKey, blob_opt: Option<Vec<u8>>) -> Option<String> {
    decrypt_blob_opt(master_key, blob_opt)
}

/// Decrypt oauth_provider_specific string if present.
fn decrypt_oauth_specific(master_key: &MasterKey, val_opt: Option<String>) -> Option<String> {
    let val = val_opt?;
    if val.is_empty() {
        return None;
    }
    // If it is base64 encoded ciphertext:
    if let Ok(blob) = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &val)
        && let Ok(decrypted) = master_key.decrypt(&blob)
    {
        Some(decrypted)
    } else {
        Some(val)
    }
}

pub(super) fn export_accounts(
    conn: &Connection,
    master_key: &MasterKey,
) -> Result<Vec<BackupAccount>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, provider_id, api_key_encrypted, label, priority, \
             extra_config_json, health_status, rate_limited_until, auth_type, \
             email, oauth_scope, oauth_provider_specific, expires_at, \
             access_token_encrypted, refresh_token_encrypted, current_proxy_id \
             FROM accounts ORDER BY id",
        )
        .map_err(map_db_error)?;

    let accounts = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            let provider_id_str: String = row.get(1)?;
            let api_key_encrypted: Option<Vec<u8>> = row.get(2)?;
            let label: Option<String> = row.get(3)?;
            let priority: i32 = row.get(4)?;
            let extra_config_json: Option<String> = row.get(5)?;
            let health_status: String = row.get(6)?;
            let rate_limited_until: Option<String> = row.get(7)?;
            let auth_type: Option<String> = row.get(8)?;
            let email: Option<String> = row.get(9)?;
            let oauth_scope: Option<String> = row.get(10)?;
            let oauth_provider_specific: Option<String> = row.get(11)?;
            let expires_at: Option<String> = row.get(12)?;
            let access_token_encrypted: Option<Vec<u8>> = row.get(13)?;
            let refresh_token_encrypted: Option<Vec<u8>> = row.get(14)?;
            let current_proxy_id: Option<String> = row.get(15)?;

            let api_key = decrypt_blob_opt(master_key, api_key_encrypted);
            let access_token = decrypt_token_opt(master_key, access_token_encrypted);
            let refresh_token = decrypt_token_opt(master_key, refresh_token_encrypted);
            let oauth_specific = decrypt_oauth_specific(master_key, oauth_provider_specific);

            Ok(BackupAccount {
                id,
                provider_id: ProviderId::new(provider_id_str),
                api_key,
                label,
                priority,
                extra_config_json,
                health_status,
                rate_limited_until,
                auth_type,
                email,
                oauth_scope,
                oauth_provider_specific: oauth_specific,
                expires_at,
                access_token,
                refresh_token,
                current_proxy_id,
            })
        })
        .map_err(map_db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_db_error)?;

    Ok(accounts)
}
