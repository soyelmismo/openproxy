//! OAuth tokens and provider metadata management for accounts.

use crate::secrets::MasterKey;
use openproxy_types::accounts::{Account, StoreOAuthTokensParams};
use openproxy_types::{AccountId, CoreError, Result};
use rusqlite::{Connection, OptionalExtension, params};

const DEFAULT_EXPIRES_IN_SECS: i64 = 3600;

crate::def_table_select!(
    account_access_token_select,
    "accounts",
    "access_token_encrypted"
);

crate::def_table_select!(
    account_refresh_token_select,
    "accounts",
    "refresh_token_encrypted"
);

crate::def_table_select!(
    account_refresh_tokens_select,
    "accounts",
    "id, refresh_token_encrypted"
);

crate::def_table_select!(
    account_meta_select,
    "accounts",
    "id, api_key_encrypted, label, access_token_encrypted, refresh_token_encrypted, expires_at, oauth_provider_specific, email, extra_config_json"
);

crate::def_table_select!(
    account_oauth_specific_select,
    "accounts",
    "oauth_provider_specific"
);

crate::def_table_select!(
    account_select,
    "accounts",
    "id, provider_id, label, priority, extra_config_json, \
     health_status, rate_limited_until, \
     quota_session_used, quota_session_limit, quota_session_reset_at, \
     quota_weekly_used, quota_weekly_limit, quota_weekly_reset_at, \
     quota_plan_name, quota_last_fetched_at, quota_fetch_error, \
     quota_model_details, \
     auth_type, email, oauth_scope, oauth_provider_specific, expires_at, \
     created_at, current_proxy_id"
);

pub fn encrypt_oauth_provider_specific(value: &str, master_key: &MasterKey) -> Result<String> {
    let blob = master_key.encrypt(value)?;
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &blob,
    ))
}

pub fn decrypt_oauth_provider_specific(
    encrypted_b64: Option<&str>,
    master_key: &MasterKey,
) -> Option<String> {
    let b64 = encrypted_b64?;
    match base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64) {
        Ok(blob) => match master_key.decrypt(&blob) {
            Ok(decrypted) => Some(decrypted),
            Err(_) => Some(b64.to_string()),
        },
        Err(_) => Some(b64.to_string()),
    }
}

fn resolve_oauth_expiry(expires_at: Option<&str>) -> String {
    expires_at.map_or_else(
        || {
            (chrono::Utc::now() + chrono::Duration::seconds(DEFAULT_EXPIRES_IN_SECS))
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        },
        str::to_string,
    )
}

pub fn store_oauth_tokens(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
    params: StoreOAuthTokensParams<'_>,
) -> Result<()> {
    let access_blob = master_key.encrypt(params.access_token)?;
    let refresh_blob = params
        .refresh_token
        .map(|rt| master_key.encrypt(rt))
        .transpose()?;

    let provider_specific_encrypted = params
        .provider_specific
        .map(|ps| encrypt_oauth_provider_specific(ps, master_key))
        .transpose()?;

    let expires_at_resolved = resolve_oauth_expiry(params.expires_at);

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
                email = COALESCE(?7, email), \
                label = COALESCE(NULLIF(label, ''), ?7), \
                health_status = 'healthy', \
                rate_limited_until = NULL, \
                quota_fetch_error = NULL \
             WHERE id = ?8",
            params![
                access_blob,
                refresh_blob,
                params.token_type,
                expires_at_resolved,
                params.scope,
                provider_specific_encrypted,
                params.email,
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

pub fn decrypt_access_token(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
) -> Result<String> {
    let blob: Option<Vec<u8>> = conn
        .query_row(
            account_access_token_select!("WHERE id = ?1"),
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

pub fn decrypt_refresh_token(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
) -> Result<Option<String>> {
    let blob: Option<Vec<u8>> = conn
        .query_row(
            account_refresh_token_select!("WHERE id = ?1"),
            params![id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx(format!(
            "select refresh_token for account {}",
            id.0
        )))?
        .ok_or(CoreError::AccountNotFound(id.0))?;

    blob.map(|b| master_key.decrypt(&b)).transpose()
}

pub fn decrypt_refresh_tokens(
    conn: &Connection,
    ids: &[AccountId],
    master_key: &MasterKey,
) -> Result<std::collections::HashMap<AccountId, Result<Option<String>>>> {
    if ids.is_empty() {
        return Ok(std::collections::HashMap::new());
    }
    let rows = crate::batch::query_in_chunks_by(
        conn,
        account_refresh_tokens_select!("WHERE id IN ({})"),
        ids,
        crate::batch::DEFAULT_CHUNK_SIZE,
        |id| id.0,
        |row| {
            let (id, blob): (i64, Option<Vec<u8>>) =
                crate::map_row_tuple!(row => ((0, i64), (1, Option<Vec<u8>>)))?;
            let token = blob.map(|b| master_key.decrypt(&b)).transpose();
            Ok((AccountId(id), token))
        },
    )
    .map_err(crate::error::map_db_error)?;

    Ok(rows.into_iter().collect())
}

pub fn list_expiring_oauth_accounts(
    conn: &Connection,
    within_seconds: i64,
    master_key: &MasterKey,
) -> Result<Vec<Account>> {
    let threshold = (chrono::Utc::now() + chrono::Duration::seconds(within_seconds))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    crate::db_query_all!(
        conn,
        account_select!(
            "WHERE auth_type = 'oauth' \
               AND expires_at IS NOT NULL \
               AND expires_at <= ?1 \
             ORDER BY priority ASC, id ASC"
        ),
        params![threshold],
        |row| super::crud::row_to_account(row, master_key),
        "list expiring oauth accounts"
    )
}

pub fn list_oauth_account_ids(conn: &Connection) -> Result<Vec<i64>> {
    crate::db_query_all!(
        conn,
        "SELECT id FROM accounts WHERE auth_type = 'oauth'",
        [],
        |r| r.get::<_, i64>(0),
        "list oauth account ids"
    )
}

pub fn list_oauth_provider_ids(conn: &Connection) -> Result<Vec<String>> {
    crate::db_query_all!(
        conn,
        "SELECT DISTINCT provider_id FROM accounts WHERE auth_type = 'oauth'",
        [],
        |r| r.get::<_, String>(0),
        "list oauth provider ids"
    )
}

pub struct RawAccount {
    pub api_key_encrypted: Option<Box<[u8]>>,
    pub label: Option<Box<str>>,
    pub access_token_encrypted: Option<Box<[u8]>>,
    pub refresh_token_encrypted: Option<Box<[u8]>>,
    pub expires_at: Option<Box<str>>,
    pub oauth_provider_specific: Option<Box<str>>,
    pub quota_session_reset_at: Option<Box<str>>,
    pub quota_model_details: Option<Box<str>>,
}

pub struct KiroMeta {
    pub region: Option<Box<str>>,
    pub profile_arn: Option<Box<str>>,
}

pub type AccountsMetaMaps = (
    std::collections::HashMap<i64, RawAccount>,
    std::collections::HashMap<i64, KiroMeta>,
);

pub fn read_provider_meta_batch<T>(
    conn: &Connection,
    master_key: Option<&MasterKey>,
    account_ids: &[i64],
) -> Result<std::collections::HashMap<i64, T>>
where
    T: serde::de::DeserializeOwned,
{
    let mut out: std::collections::HashMap<i64, T> =
        std::collections::HashMap::with_capacity(account_ids.len());
    if account_ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<(i64, Option<String>)> = crate::batch::query_in_chunks_by(
        conn,
        "SELECT id, oauth_provider_specific FROM accounts WHERE id IN ({})",
        account_ids,
        crate::batch::DEFAULT_CHUNK_SIZE,
        |id| *id,
        |r| {
            let id: i64 = r.get(0)?;
            let raw: Option<String> = r.get(1)?;
            Ok((id, raw))
        },
    )
    .map_err(crate::error::map_db_error_ctx("batch read_provider_meta"))?;

    for (id, raw) in rows {
        let Some(raw) = raw.filter(|s| !s.is_empty()) else {
            continue;
        };
        let plaintext = match master_key {
            Some(mk) => decrypt_oauth_provider_specific(Some(&raw), mk),
            None => Some(raw),
        };
        let Some(plaintext) = plaintext else {
            continue;
        };
        let value: T = match serde_json::from_str::<T>(&plaintext) {
            Ok(v) => v,
            Err(_) if master_key.is_some() => continue,
            Err(e) => {
                return Err(CoreError::Parse(format!(
                    "read_provider_meta_batch deserialize for account {id}: {e}"
                )));
            }
        };
        out.insert(id, value);
    }
    Ok(out)
}

fn extract_json_field(val: &serde_json::Value, primary: &str, secondary: &str) -> Option<Box<str>> {
    val.get(primary)
        .or_else(|| val.get(secondary))
        .and_then(|v| v.as_str())
        .map(Into::into)
}

fn extract_kiro_meta(extra_json: Option<&str>) -> Option<KiroMeta> {
    let cfg_str = extra_json?;
    let val: serde_json::Value = serde_json::from_str(cfg_str).ok()?;
    let region = extract_json_field(&val, "region", "aws_region");
    let profile_arn = extract_json_field(&val, "profile_arn", "aws_role_arn");

    if region.is_some() || profile_arn.is_some() {
        Some(KiroMeta {
            region,
            profile_arn,
        })
    } else {
        None
    }
}

type AccountRowTuple = (
    i64,
    Option<Box<[u8]>>,
    Option<Box<str>>,
    Option<Box<[u8]>>,
    Option<Box<[u8]>>,
    Option<Box<str>>,
    Option<Box<str>>,
    Option<Box<str>>,
    Option<Box<str>>,
);

fn populate_account_meta_maps(rows: Vec<AccountRowTuple>) -> AccountsMetaMaps {
    let mut raw_map = std::collections::HashMap::with_capacity(rows.len());
    let mut kiro_map = std::collections::HashMap::new();

    for (id_val, api_key, label, access, refresh, expires, oauth_prov, _email, extra_json) in rows {
        if let Some(kiro) = extract_kiro_meta(extra_json.as_deref()) {
            kiro_map.insert(id_val, kiro);
        }

        raw_map.insert(
            id_val,
            RawAccount {
                api_key_encrypted: api_key,
                label,
                access_token_encrypted: access,
                refresh_token_encrypted: refresh,
                expires_at: expires,
                oauth_provider_specific: oauth_prov,
                quota_session_reset_at: None,
                quota_model_details: None,
            },
        );
    }

    (raw_map, kiro_map)
}

fn validate_all_accounts_found(
    account_ids: &[AccountId],
    raw_map: &std::collections::HashMap<i64, RawAccount>,
) -> Result<()> {
    for id in account_ids {
        if !raw_map.contains_key(&id.0) {
            return Err(CoreError::Validation(format!("account {} not found", id.0)));
        }
    }
    Ok(())
}

pub fn get_accounts_meta(conn: &Connection, account_ids: &[AccountId]) -> Result<AccountsMetaMaps> {
    if account_ids.is_empty() {
        return Ok((
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        ));
    }

    let rows: Vec<AccountRowTuple> = crate::batch::query_in_chunks_by(
        conn,
        account_meta_select!("WHERE id IN ({})"),
        account_ids,
        crate::batch::DEFAULT_CHUNK_SIZE,
        |id| id.0,
        |r| {
            Ok((
                r.get(0)?,
                r.get::<_, Option<Vec<u8>>>(1)?.map(Vec::into_boxed_slice),
                r.get::<_, Option<String>>(2)?.map(String::into_boxed_str),
                r.get::<_, Option<Vec<u8>>>(3)?.map(Vec::into_boxed_slice),
                r.get::<_, Option<Vec<u8>>>(4)?.map(Vec::into_boxed_slice),
                r.get::<_, Option<String>>(5)?.map(String::into_boxed_str),
                r.get::<_, Option<String>>(6)?.map(String::into_boxed_str),
                r.get::<_, Option<String>>(7)?.map(String::into_boxed_str),
                r.get::<_, Option<String>>(8)?.map(String::into_boxed_str),
            ))
        },
    )
    .map_err(crate::error::map_db_error_ctx("batch query accounts"))?;

    let (raw_map, kiro_map) = populate_account_meta_maps(rows);
    validate_all_accounts_found(account_ids, &raw_map)?;

    Ok((raw_map, kiro_map))
}

pub fn update_antigravity_project_id(
    conn: &Connection,
    account_id: i64,
    new_project_id: &str,
) -> Result<()> {
    let current_json_opt: Option<String> = conn
        .query_row(
            account_oauth_specific_select!("WHERE id = ?1"),
            params![account_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx("query account"))?
        .flatten();

    let mut meta: AntigravityMeta = current_json_opt
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(AntigravityMeta { project_id: None });
    meta.project_id = Some(new_project_id.to_string());
    let new_json_str = serde_json::to_string(&meta).map_err(|e| {
        CoreError::Internal(format!(
            "update_antigravity_project_id serialize for account {account_id}: {e}"
        ))
    })?;

    conn.execute(
        "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
        params![new_json_str, account_id],
    )
    .map_err(crate::error::map_db_error_ctx("update account"))?;

    Ok(())
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct AntigravityMeta {
    #[serde(alias = "projectId", alias = "project_id")]
    pub project_id: Option<String>,
}

pub fn read_provider_meta<T>(
    conn: &Connection,
    master_key: Option<&MasterKey>,
    account_id: i64,
) -> Result<Option<T>>
where
    T: serde::de::DeserializeOwned,
{
    let raw: Option<Option<String>> = conn
        .query_row(
            account_oauth_specific_select!("WHERE id = ?1"),
            params![account_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx(format!(
            "read_provider_meta for account {account_id}"
        )))?;

    let Some(raw) = raw.flatten() else {
        return Ok(None);
    };
    if raw.is_empty() {
        return Ok(None);
    }
    let plaintext = match master_key {
        Some(mk) => decrypt_oauth_provider_specific(Some(&raw), mk),
        None => Some(raw),
    };
    let Some(plaintext) = plaintext else {
        return Ok(None);
    };
    match serde_json::from_str::<T>(&plaintext) {
        Ok(v) => Ok(Some(v)),
        Err(_) if master_key.is_some() => Ok(None),
        Err(e) => Err(CoreError::Parse(format!(
            "read_provider_meta deserialize for account {account_id}: {e}"
        ))),
    }
}

pub fn read_antigravity_project(
    conn: &Connection,
    master_key: Option<&MasterKey>,
    account_id: i64,
) -> Result<Option<String>> {
    let meta: Option<AntigravityMeta> = read_provider_meta(conn, master_key, account_id)?;
    Ok(meta
        .and_then(|m| m.project_id)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty()))
}
