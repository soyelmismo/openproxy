use crate::secrets::MasterKey;
use openproxy_types::accounts::{Account, StoreOAuthTokensParams};
use openproxy_types::{AccountId, CoreError, HealthStatus, ProviderId, Result};
use rusqlite::{Connection, OptionalExtension, params};

const DEFAULT_EXPIRES_IN_SECS: i64 = 3600;

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

crate::def_table_select!(
    account_api_key_select,
    "accounts",
    "api_key_encrypted"
);

crate::def_table_select!(
    account_api_key_label_select,
    "accounts",
    "api_key_encrypted, label"
);

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
    account_current_proxy_select,
    "accounts",
    "current_proxy_id"
);

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
            if crate::error::classify_sqlite_error(&e) == crate::error::DbErrorKind::ForeignKeyViolation {
                Err(CoreError::Validation("unknown provider".into()))
            } else {
                Err(crate::error::map_db_error_ctx(format!(
                    "insert account for provider {provider_id}"
                ))(e))
            }
        }
    }
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

pub fn decrypt_api_key(conn: &Connection, id: AccountId, master_key: &MasterKey) -> Result<String> {
    let blob: Option<Vec<u8>> = conn
        .query_row(
            account_api_key_select!("WHERE id = ?1"),
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
        .map_err(crate::error::map_db_error_ctx(format!(
            "select api_key+label for account {}",
            id.0
        )))?;
    let Some((blob, label)) = row else {
        return Err(CoreError::AccountNotFound(id.0));
    };
    let blob = blob
        .ok_or_else(|| CoreError::Validation("account has no API key (OAuth account?)".into()))?;
    let key = master_key.decrypt(&blob)?;
    Ok((key, label))
}

pub fn set_health(conn: &Connection, id: AccountId, health: HealthStatus) -> Result<()> {
    let affected = crate::db_update_field!(
        conn,
        "accounts",
        health_status = health.as_str(),
        WHERE id = id.0,
        format!("update health for account {}", id.0)
    )?;
    if affected == 0 {
        return Err(CoreError::AccountNotFound(id.0));
    }
    Ok(())
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
    if affected == 0 {
        return Err(CoreError::AccountNotFound(id.0));
    }
    Ok(())
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
    let affected = crate::db_update_field!(
        conn,
        "accounts",
        api_key_encrypted = blob,
        WHERE id = id.0,
        format!("update api_key for account {}", id.0)
    )?;
    if affected == 0 {
        return Err(CoreError::AccountNotFound(id.0));
    }
    Ok(())
}

pub fn update_label(conn: &Connection, id: AccountId, label: Option<&str>) -> Result<()> {
    let affected = crate::db_update_field!(
        conn,
        "accounts",
        label = label,
        WHERE id = id.0,
        format!("update label for account {}", id.0)
    )?;
    if affected == 0 {
        return Err(CoreError::AccountNotFound(id.0));
    }
    Ok(())
}

pub fn delete(conn: &Connection, id: AccountId) -> Result<()> {
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
    if affected == 0 {
        return Err(CoreError::AccountNotFound(id.0));
    }
    Ok(())
}

fn encrypt_oauth_provider_specific(value: &str, master_key: &MasterKey) -> Result<String> {
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

pub fn store_oauth_tokens(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
    params: StoreOAuthTokensParams<'_>,
) -> Result<()> {
    let StoreOAuthTokensParams {
        access_token,
        refresh_token,
        token_type,
        expires_at,
        scope,
        provider_specific,
        email,
    } = params;
    let access_blob = master_key.encrypt(access_token)?;
    let refresh_blob = refresh_token.map(|rt| master_key.encrypt(rt)).transpose()?;

    let provider_specific_encrypted = provider_specific
        .map(|ps| encrypt_oauth_provider_specific(ps, master_key))
        .transpose()?;

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
                email = COALESCE(?7, email), \
                label = COALESCE(NULLIF(label, ''), ?7) \
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
        |row| row_to_account(row, master_key),
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

fn row_to_account(row: &rusqlite::Row<'_>, master_key: &MasterKey) -> rusqlite::Result<Account> {
    let oauth_provider_specific_encrypted: Option<String> = row.get(20)?;
    let oauth_provider_specific =
        decrypt_oauth_provider_specific(oauth_provider_specific_encrypted.as_deref(), master_key);

    crate::map_row_struct!(row, Account {
        id: @id(0, AccountId),
        provider_id: @id_str(1, ProviderId),
        label: 2,
        priority: 3,
        extra_config_json: 4,
        health_status: @enum_parse(5, HealthStatus),
        rate_limited_until: 6,
        quota_session_used: 7,
        quota_session_limit: 8,
        quota_session_reset_at: 9,
        quota_weekly_used: 10,
        quota_weekly_limit: 11,
        quota_weekly_reset_at: 12,
        quota_plan_name: 13,
        quota_last_fetched_at: 14,
        quota_fetch_error: 15,
        quota_model_details: @json(16),
        auth_type: 17,
        email: 18,
        oauth_scope: 19,
        oauth_provider_specific: @expr(oauth_provider_specific),
        expires_at: 21,
        created_at: 22,
        current_proxy_id: 23,
    })
}

pub struct RawAccount {
    pub api_key_encrypted: Option<Vec<u8>>,
    pub label: Option<String>,
    pub access_token_encrypted: Option<Vec<u8>>,
    pub refresh_token_encrypted: Option<Vec<u8>>,
    pub expires_at: Option<String>,
    pub oauth_provider_specific: Option<String>,
    pub quota_session_reset_at: Option<String>,
    pub quota_model_details: Option<String>,
}

pub struct KiroMeta {
    pub region: Option<String>,
    pub profile_arn: Option<String>,
}

pub type AccountsMetaMaps = (
    std::collections::HashMap<i64, RawAccount>,
    std::collections::HashMap<i64, KiroMeta>,
    std::collections::HashMap<i64, String>,
);

pub fn get_accounts_meta(conn: &Connection, account_ids: &[AccountId]) -> Result<AccountsMetaMaps> {
    if account_ids.is_empty() {
        return Ok((
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        ));
    }

    type AccountRowTuple = (
        i64,
        Option<Vec<u8>>,
        Option<String>,
        Option<Vec<u8>>,
        Option<Vec<u8>>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );

    let rows: Vec<AccountRowTuple> = crate::batch::query_in_chunks_by(
        conn,
        account_meta_select!("WHERE id IN ({})"),
        account_ids,
        crate::batch::DEFAULT_CHUNK_SIZE,
        |id| id.0,
        |r| crate::map_row_tuple!(r => (0, 1, 2, 3, 4, 5, 6, 7, 8)),
    )
    .map_err(crate::error::map_db_error_ctx("batch query accounts"))?;

    let mut raw_map = std::collections::HashMap::with_capacity(rows.len());
    let mut kiro_map = std::collections::HashMap::new();
    let mut ag_map = std::collections::HashMap::new();

    for (id_val, api_key, label, access, refresh, expires, oauth_prov, _email, extra_json) in rows {
        if let Some(ref oauth_json) = oauth_prov
            && let Ok(meta) = serde_json::from_str::<serde_json::Value>(oauth_json)
            && let Some(pid) = meta
                .get("projectId")
                .or_else(|| meta.get("project_id"))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
        {
            ag_map.insert(id_val, pid.to_string());
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

        if let Some(cfg_str) = extra_json
            && let Ok(val) = serde_json::from_str::<serde_json::Value>(&cfg_str)
        {
            let region = val
                .get("region")
                .or(val.get("aws_region"))
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string);
            let profile_arn = val
                .get("profile_arn")
                .or(val.get("aws_role_arn"))
                .and_then(|v| v.as_str())
                .map(std::string::ToString::to_string);
            if region.is_some() || profile_arn.is_some() {
                kiro_map.insert(
                    id_val,
                    KiroMeta {
                        region,
                        profile_arn,
                    },
                );
            }
        }
    }

    for id in account_ids {
        if !raw_map.contains_key(&id.0) {
            return Err(CoreError::Validation(format!("account {} not found", id.0)));
        }
    }

    Ok((raw_map, kiro_map, ag_map))
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

    let mut meta = if let Some(json_str) = current_json_opt {
        serde_json::from_str::<serde_json::Value>(&json_str)
            .unwrap_or_else(|_| serde_json::json!({}))
    } else {
        serde_json::json!({})
    };

    if let Some(obj) = meta.as_object_mut() {
        obj.insert(
            "projectId".to_string(),
            serde_json::Value::String(new_project_id.to_string()),
        );
    }

    let new_json_str = serde_json::to_string(&meta).unwrap_or_else(|_| "{}".to_string());

    conn.execute(
        "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
        params![new_json_str, account_id],
    )
    .map_err(crate::error::map_db_error_ctx("update account"))?;

    Ok(())
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
