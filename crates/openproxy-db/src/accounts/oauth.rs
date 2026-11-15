//! OAuth tokens and provider metadata management for accounts.

use crate::secrets::MasterKey;
use openproxy_types::accounts::{Account, StoreOAuthTokensParams};
use openproxy_types::{AccountId, CoreError, Result};
use rusqlite::{Connection, OptionalExtension, params};

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

/// AAD context that binds every `oauth_provider_specific` blob to the
/// column it lives in (audit H-8).
pub const PROVIDER_SPECIFIC_CTX: &str = "accounts.oauth_provider_specific";

pub fn encrypt_oauth_provider_specific(value: &str, master_key: &MasterKey) -> Result<String> {
    let blob = master_key.encrypt_ctx(value, PROVIDER_SPECIFIC_CTX)?;
    Ok(base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        &blob,
    ))
}

/// Decrypt a stored `oauth_provider_specific` value.
///
/// Security (audit H-10): this used to fall back to returning the stored
/// string verbatim whenever base64 decoding or AES-GCM decryption failed,
/// which made a tampered or attacker-planted plaintext value indistinguishable
/// from a legitimate one. It is now fail-closed: only values that authenticate
/// as blobs sealed for this column (v1, AAD-bound) or legacy v0 blobs decrypt;
/// anything else yields `None`.
///
/// Legacy plaintext-JSON rows (written by paths that used to store the column
/// unencrypted) are adopted into encrypted blobs once at boot by
/// [`adopt_plaintext_oauth_provider_specific`].
pub fn decrypt_oauth_provider_specific(
    encrypted_b64: Option<&str>,
    master_key: &MasterKey,
) -> Option<String> {
    let b64 = encrypted_b64?;
    let blob = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).ok()?;
    master_key.decrypt_ctx(&blob, PROVIDER_SPECIFIC_CTX).ok()
}

/// One-shot boot repair (audit H-10): adopt legacy plaintext-JSON
/// `oauth_provider_specific` rows into MasterKey-sealed blobs so that
/// [`decrypt_oauth_provider_specific`] can stay fail-closed.
///
/// A row is adopted when it does not decrypt as a blob but parses as JSON
/// (the shape every writer of this column produces). Rows that neither
/// decrypt nor parse are counted as quarantined and left untouched: reads
/// treat them as "no provider metadata" instead of trusting the raw value.
/// Returns `(adopted, quarantined)`.
pub fn adopt_plaintext_oauth_provider_specific(
    conn: &Connection,
    master_key: &MasterKey,
) -> Result<(usize, usize)> {
    let mut stmt = conn
        .prepare(
            "SELECT id, oauth_provider_specific FROM accounts \
             WHERE oauth_provider_specific IS NOT NULL",
        )
        .map_err(crate::error::map_db_error_ctx("scan provider_specific"))?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(crate::error::map_db_error_ctx("scan provider_specific"))?
        .collect::<rusqlite::Result<_>>()
        .map_err(crate::error::map_db_error_ctx("scan provider_specific"))?;
    drop(stmt);

    let mut adopted = 0usize;
    let mut quarantined = 0usize;
    for (id, raw) in rows {
        if raw.is_empty() {
            continue;
        }
        // Already a sealed blob (v0 or v1): nothing to do.
        if decrypt_oauth_provider_specific(Some(&raw), master_key).is_some() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(_) => {
                let sealed = encrypt_oauth_provider_specific(&raw, master_key)?;
                conn.execute(
                    "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
                    params![sealed, id],
                )
                .map_err(crate::error::map_db_error_ctx("adopt provider_specific"))?;
                adopted += 1;
            }
            Err(_) => {
                quarantined += 1;
                tracing::warn!(
                    account_id = id,
                    "oauth_provider_specific neither decrypts nor parses as JSON; \
                     reads will treat it as absent (fail-closed)"
                );
            }
        }
    }
    Ok((adopted, quarantined))
}

/// `expires_at = NULL` is the designed "deadline unknown" state: undated
/// accounts are tracked by `list_undated_oauth_accounts` when a refresh
/// token exists, and tokens with no renewal path keep working until the
/// upstream itself rejects them. No deadline is ever fabricated here.
pub fn store_oauth_tokens(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
    params: StoreOAuthTokensParams<'_>,
) -> Result<()> {
    super::bump_epoch();
    let access_blob = master_key.encrypt(params.access_token)?;
    let refresh_blob = params
        .refresh_token
        .map(|rt| master_key.encrypt(rt))
        .transpose()?;

    let provider_specific_encrypted = params
        .provider_specific
        .map(|ps| encrypt_oauth_provider_specific(ps, master_key))
        .transpose()?;

    crate::crud::execute_check_affected(
        conn,
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
            params.expires_at,
            params.scope,
            provider_specific_encrypted,
            params.email,
            id.0,
        ],
        format!("store_oauth_tokens for account {}", id.0),
        || CoreError::AccountNotFound(id.0),
    )
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
        .map_err(|e| {
            crate::error::map_db_error_ctx(format!("select access_token for account {}", id.0))(e)
        })?
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
        .map_err(|e| {
            crate::error::map_db_error_ctx(format!("select refresh_token for account {}", id.0))(e)
        })?
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

/// OAuth accounts with no recorded deadline but a refresh token to renew with.
///
/// [`list_expiring_oauth_accounts`] requires `expires_at IS NOT NULL`, so a row
/// written without one is invisible to the scheduler even though its access
/// token may already be dead. This is the complementary query: rows whose
/// deadline is unknown rather than absent, which must be checked instead of
/// skipped.
pub fn list_undated_oauth_accounts(
    conn: &Connection,
    master_key: &MasterKey,
) -> Result<Vec<Account>> {
    crate::db_query_all!(
        conn,
        account_select!(
            "WHERE auth_type = 'oauth' \
               AND expires_at IS NULL \
               AND refresh_token_encrypted IS NOT NULL \
             ORDER BY priority ASC, id ASC"
        ),
        [],
        |row| super::crud::row_to_account(row, master_key),
        "list undated oauth accounts"
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
    master_key: &MasterKey,
    account_id: i64,
    new_project_id: &str,
) -> Result<()> {
    super::bump_epoch();
    let current_json_opt: Option<String> = conn
        .query_row(
            account_oauth_specific_select!("WHERE id = ?1"),
            params![account_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx("query account"))?
        .flatten();

    // Security (audit H-10): this used to parse the stored value as raw JSON
    // — for an encrypted blob that parse always failed, so the update
    // REPLACED the whole sealed payload with a plaintext `{"project_id":…}`
    // (destroying every other field and leaving credentials at rest in the
    // clear). The current value is now decrypted through the fail-closed
    // helper, mutated as a JSON map (preserving fields this writer does not
    // know about), and re-sealed for this column.
    let current_json_opt = current_json_opt
        .as_deref()
        .and_then(|s| decrypt_oauth_provider_specific(Some(s), master_key));

    let mut obj: serde_json::Map<String, serde_json::Value> = current_json_opt
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    // Migration 000065 semantics: rename a legacy camelCase `projectId`.
    if let Some(v) = obj.remove("projectId")
        && !obj.contains_key("project_id")
    {
        obj.insert("project_id".to_string(), v);
    }
    obj.insert(
        "project_id".to_string(),
        serde_json::Value::from(new_project_id),
    );
    let new_json_str = serde_json::to_string(&obj).map_err(|e| {
        CoreError::Internal(format!(
            "update_antigravity_project_id serialize for account {account_id}: {e}"
        ))
    })?;
    let sealed = encrypt_oauth_provider_specific(&new_json_str, master_key)?;

    conn.execute(
        "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
        params![sealed, account_id],
    )
    .map_err(crate::error::map_db_error_ctx("update account"))?;

    Ok(())
}

pub fn update_codex_reset_credits(
    conn: &Connection,
    master_key: &MasterKey,
    account_id: i64,
    reset_credits: u32,
) -> Result<()> {
    super::bump_epoch();
    let current_json_opt: Option<String> = conn
        .query_row(
            account_oauth_specific_select!("WHERE id = ?1"),
            params![account_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx("query account"))?
        .flatten();

    // Security (audit H-10): same root cause as
    // `update_antigravity_project_id` — read through the fail-closed decrypt,
    // mutate, and write back sealed for this column.
    let current_json_opt = current_json_opt
        .as_deref()
        .and_then(|s| decrypt_oauth_provider_specific(Some(s), master_key));

    let mut obj: serde_json::Map<String, serde_json::Value> = current_json_opt
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    obj.insert(
        "reset_credits".to_string(),
        serde_json::Value::from(reset_credits),
    );
    let new_json_str = serde_json::to_string(&obj).map_err(|e| {
        CoreError::Internal(format!(
            "update_codex_reset_credits serialize for account {account_id}: {e}"
        ))
    })?;
    let sealed = encrypt_oauth_provider_specific(&new_json_str, master_key)?;

    conn.execute(
        "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
        params![sealed, account_id],
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
        .map_err(|e| {
            crate::error::map_db_error_ctx(format!("read_provider_meta for account {account_id}"))(
                e,
            )
        })?;

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

/// Writes a known deadline for an OAuth account.
///
/// Only fills a gap: `expires_at IS NULL` in the predicate means a deadline that
/// a concurrent refresh already recorded is never overwritten with an older one.
pub fn set_oauth_expires_at(conn: &Connection, account_id: i64, expires_at: &str) -> Result<()> {
    super::bump_epoch();
    conn.execute(
        "UPDATE accounts SET expires_at = ?1 WHERE id = ?2 AND expires_at IS NULL",
        params![expires_at, account_id],
    )
    .map_err(crate::error::map_db_error_ctx("set oauth expires_at"))?;
    Ok(())
}

/// Marks an OAuth account's deadline as already reached.
///
/// This is what a failed refresh must do. Erasing the deadline (`expires_at =
/// NULL`) hides the account from every refresh path at once: the scheduler's
/// query requires a non-null deadline, and the pipeline reads null as "fresh
/// forever". One failed refresh therefore stranded the credentials permanently,
/// and the refresh token then lapsed server-side from disuse — the account looked
/// fine until a request finally came back 401.
///
/// Keeping the deadline in the past leaves the row due. Pacing comes from the
/// scheduler's failure backoff, not from hiding the row.
pub fn mark_oauth_expiry_due(conn: &Connection, account_id: i64) -> Result<()> {
    super::bump_epoch();
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    conn.execute(
        "UPDATE accounts SET expires_at = ?1 WHERE id = ?2",
        params![now, account_id],
    )
    .map_err(crate::error::map_db_error_ctx("mark oauth expires_at due"))?;
    Ok(())
}
