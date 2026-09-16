use super::mapping::row_to_api_key;
use super::models::{
    ApiKey, CreateApiKeyInput, LAST_USED_THROTTLE_SECS, UpdateParams, UsageSummary,
    generate_plaintext, hash_key,
};
use crate::error::{CoreError, Result};
use crate::ids::ApiKeyId;
use crate::validation::Validatable;
use openproxy_types::UpdateField;
use rusqlite::{Connection, OptionalExtension, params};

/// Create a new API key. Returns the persisted row plus the plaintext
/// (shown to the user once and never re-derivable).
pub fn create(
    conn: &Connection,
    input: CreateApiKeyInput,
    created_by: &str,
) -> Result<(ApiKey, String)> {
    input.validate()?;
    if input.scopes.is_empty() {
        return Err(CoreError::Validation(
            "scopes must contain at least one entry".into(),
        ));
    }
    let plaintext = generate_plaintext();
    let key_hash = hash_key(&plaintext);
    let key_prefix: String = plaintext.chars().take(12).collect();
    let scopes_json = serde_json::to_string(&input.scopes)
        .map_err(|e| CoreError::Parse(format!("serialize scopes: {e}")))?;
    let allowed_models_json = match &input.allowed_models {
        Some(v) => Some(
            serde_json::to_string(v)
                .map_err(|e| CoreError::Parse(format!("serialize allowed_models: {e}")))?,
        ),
        None => None,
    };
    let allowed_combos_json = match &input.allowed_combos {
        Some(v) => Some(
            serde_json::to_string(v)
                .map_err(|e| CoreError::Parse(format!("serialize allowed_combos: {e}")))?,
        ),
        None => None,
    };
    let blacklisted_providers_json = match &input.blacklisted_providers {
        Some(v) => Some(
            serde_json::to_string(v)
                .map_err(|e| CoreError::Parse(format!("serialize blacklisted_providers: {e}")))?,
        ),
        None => None,
    };
    let blacklisted_models_json = match &input.blacklisted_models {
        Some(v) => Some(
            serde_json::to_string(v)
                .map_err(|e| CoreError::Parse(format!("serialize blacklisted_models: {e}")))?,
        ),
        None => None,
    };

    conn.execute(
        "INSERT INTO api_keys \
            (key_hash, key_prefix, label, scopes_json, allowed_models_json, \
             allowed_combos_json, expires_at, created_by, \
             blacklisted_providers_json, blacklisted_models_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            key_hash,
            key_prefix,
            input.label,
            scopes_json,
            allowed_models_json,
            allowed_combos_json,
            input.expires_at,
            created_by,
            blacklisted_providers_json,
            blacklisted_models_json,
        ],
    )
    .map_err(openproxy_db::error::map_db_error)?;

    let id = ApiKeyId(conn.last_insert_rowid());
    let row = get_by_id(conn, id)?
        .ok_or_else(|| CoreError::Internal("failed to load newly-inserted api key".into()))?;
    Ok((row, plaintext))
}

/// Look up a single API key by id. Returns `Ok(None)` when absent.
pub fn get_by_id(conn: &Connection, id: ApiKeyId) -> Result<Option<ApiKey>> {
    let row = conn
        .query_row(
            "SELECT id, key_hash, key_prefix, label, scopes_json, \
                    allowed_models_json, allowed_combos_json, is_active, \
                    revoked_at, expires_at, last_used_at, created_at, created_by, \
                    blacklisted_providers_json, blacklisted_models_json \
             FROM api_keys WHERE id = ?1",
            params![id.0],
            row_to_api_key,
        )
        .optional()
        .map_err(|e| CoreError::Database {
            message: format!("get api_key {}: {e}", id.0),
            source: Some(std::sync::Arc::new(e)),
        })?;
    Ok(row)
}

/// Look up by hash. Used by the chat handler on every authenticated request.
pub fn get_by_hash(conn: &Connection, key_hash: &str) -> Result<Option<ApiKey>> {
    let row = conn
        .query_row(
            "SELECT id, key_hash, key_prefix, label, scopes_json, \
                    allowed_models_json, allowed_combos_json, is_active, \
                    revoked_at, expires_at, last_used_at, created_at, created_by, \
                    blacklisted_providers_json, blacklisted_models_json \
             FROM api_keys WHERE key_hash = ?1",
            params![key_hash],
            row_to_api_key,
        )
        .optional()
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(row)
}

/// Count the number of *active* API keys.
pub fn count_active(conn: &Connection) -> Result<u64> {
    let n: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM api_keys WHERE is_active = 1",
            [],
            |row| row.get(0),
        )
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(n.max(0) as u64)
}

/// List every API key, newest first.
pub fn list(conn: &Connection) -> Result<Vec<ApiKey>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, key_hash, key_prefix, label, scopes_json, \
                    allowed_models_json, allowed_combos_json, is_active, \
                    revoked_at, expires_at, last_used_at, created_at, created_by, \
                    blacklisted_providers_json, blacklisted_models_json \
             FROM api_keys ORDER BY id DESC",
        )
        .map_err(openproxy_db::error::map_db_error)?;
    let rows = stmt
        .query_map([], row_to_api_key)
        .map_err(openproxy_db::error::map_db_error)?;
    rows.map(|r| r.map_err(openproxy_db::error::map_db_error))
        .collect()
}

/// Soft-revoke: mark the key as inactive and stamp `revoked_at`.
pub fn revoke(conn: &Connection, id: ApiKeyId) -> Result<()> {
    let affected = conn
        .execute(
            "UPDATE api_keys \
             SET is_active = 0, \
                 revoked_at = COALESCE(revoked_at, datetime('now')) \
             WHERE id = ?1",
            params![id.0],
        )
        .map_err(|e| CoreError::Database {
            message: format!("revoke api_key {}: {e}", id.0),
            source: Some(std::sync::Arc::new(e)),
        })?;
    if affected == 0 {
        return Err(CoreError::Internal(format!("api_key {} not found", id.0)));
    }
    Ok(())
}

/// Hard delete an API key by id.
pub fn hard_delete(conn: &Connection, id: ApiKeyId) -> Result<()> {
    conn.execute("DELETE FROM api_keys WHERE id = ?1", params![id.0])
        .map_err(|e| CoreError::Database {
            message: format!("delete api_key {}: {e}", id.0),
            source: Some(std::sync::Arc::new(e)),
        })?;
    Ok(())
}

fn update_key_hash_row(
    conn: &Connection,
    id: ApiKeyId,
    key_hash: &str,
    key_prefix: &str,
) -> Result<()> {
    let affected = conn
        .execute(
            "UPDATE api_keys \
             SET key_hash = ?1, key_prefix = ?2, \
                 is_active = 1, revoked_at = NULL, last_used_at = NULL \
             WHERE id = ?3",
            params![key_hash, key_prefix, id.0],
        )
        .map_err(|e| CoreError::Database {
            message: format!("regenerate api_key {}: {e}", id.0),
            source: Some(std::sync::Arc::new(e)),
        })?;
    if affected == 0 {
        return Err(CoreError::Internal(format!("api_key {} not found", id.0)));
    }
    Ok(())
}

/// Issue a new plaintext and re-hash the row.
pub fn regenerate(conn: &Connection, id: ApiKeyId) -> Result<(ApiKey, String)> {
    let plaintext = generate_plaintext();
    let key_hash = hash_key(&plaintext);
    let key_prefix: String = plaintext.chars().take(12).collect();

    update_key_hash_row(conn, id, &key_hash, &key_prefix)?;

    let row = get_by_id(conn, id)?
        .ok_or_else(|| CoreError::Internal("regenerated api_key vanished".into()))?;
    Ok((row, plaintext))
}

/// Stamp `last_used_at` on the row, throttled by `LAST_USED_THROTTLE_SECS`.
pub fn touch_last_used(conn: &Connection, id: ApiKeyId) -> Result<()> {
    let affected = conn
        .execute(
            "UPDATE api_keys SET last_used_at = datetime('now') \
             WHERE id = ?1 \
               AND (last_used_at IS NULL \
                    OR (julianday('now') - julianday(last_used_at)) * 86400 > ?2)",
            params![id.0, LAST_USED_THROTTLE_SECS],
        )
        .map_err(|e| CoreError::Database {
            message: format!("touch_last_used api_key {}: {e}", id.0),
            source: Some(std::sync::Arc::new(e)),
        })?;
    let _ = affected;
    Ok(())
}

fn serialize_update_field<T: serde::Serialize>(
    field: UpdateField<&[T]>,
    name: &str,
) -> Result<UpdateField<String>> {
    match field {
        UpdateField::Ignore => Ok(UpdateField::Ignore),
        UpdateField::Reset => Ok(UpdateField::Reset),
        UpdateField::Set(v) => {
            let s = serde_json::to_string(v)
                .map_err(|e| CoreError::Parse(format!("serialize {name}: {e}")))?;
            Ok(UpdateField::Set(s))
        }
    }
}

fn build_update_json_clauses(
    scopes_json: Option<String>,
    allowed_models_json: UpdateField<String>,
    allowed_combos_json: UpdateField<String>,
    blacklisted_providers_json: UpdateField<String>,
    blacklisted_models_json: UpdateField<String>,
    sets: &mut Vec<&'static str>,
    bound: &mut Vec<Box<dyn rusqlite::ToSql>>,
) {
    if let Some(s) = scopes_json {
        sets.push("scopes_json = ?");
        bound.push(Box::new(s));
    }
    if let Some(om) = allowed_models_json.into_option() {
        sets.push("allowed_models_json = ?");
        bound.push(Box::new(om));
    }
    if let Some(oc) = allowed_combos_json.into_option() {
        sets.push("allowed_combos_json = ?");
        bound.push(Box::new(oc));
    }
    if let Some(bp) = blacklisted_providers_json.into_option() {
        sets.push("blacklisted_providers_json = ?");
        bound.push(Box::new(bp));
    }
    if let Some(bm) = blacklisted_models_json.into_option() {
        sets.push("blacklisted_models_json = ?");
        bound.push(Box::new(bm));
    }
}

fn build_update_scalar_clauses(
    params: UpdateParams<'_>,
    sets: &mut Vec<&'static str>,
    bound: &mut Vec<Box<dyn rusqlite::ToSql>>,
) {
    if let Some(label_value) = params.label {
        sets.push("label = ?");
        bound.push(Box::new(label_value.to_string()));
    }
    if let Some(active) = params.is_active {
        sets.push("is_active = ?");
        bound.push(Box::new(active as i64));
        if !active {
            sets.push("revoked_at = COALESCE(revoked_at, datetime('now'))");
        } else {
            sets.push("revoked_at = NULL");
        }
    }
    match params.expires_at {
        UpdateField::Ignore => {}
        UpdateField::Reset => {
            sets.push("expires_at = ?");
            bound.push(Box::new(None::<String>));
        }
        UpdateField::Set(v) => {
            sets.push("expires_at = ?");
            bound.push(Box::new(Some(v.to_string())));
        }
    }
}

fn verify_api_key_exists(conn: &Connection, id: ApiKeyId) -> Result<()> {
    let present: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM api_keys WHERE id = ?1",
            params![id.0],
            |r| r.get(0),
        )
        .map_err(|e| CoreError::Database {
            message: format!("count api_key {}: {e}", id.0),
            source: Some(std::sync::Arc::new(e)),
        })?;
    if present == 0 {
        return Err(CoreError::Internal(format!("api_key {} not found", id.0)));
    }
    Ok(())
}

pub fn update(conn: &Connection, id: ApiKeyId, params: UpdateParams<'_>) -> Result<()> {
    if let Some(s) = params.scopes
        && s.is_empty()
    {
        return Err(CoreError::Validation(
            "scopes must contain at least one entry".into(),
        ));
    }
    let scopes_json = params
        .scopes
        .map(|s| {
            serde_json::to_string(s).map_err(|e| CoreError::Parse(format!("serialize scopes: {e}")))
        })
        .transpose()?;
    let allowed_models_json = serialize_update_field(params.allowed_models, "allowed_models")?;
    let allowed_combos_json = serialize_update_field(params.allowed_combos, "allowed_combos")?;
    let blacklisted_providers_json =
        serialize_update_field(params.blacklisted_providers, "blacklisted_providers")?;
    let blacklisted_models_json =
        serialize_update_field(params.blacklisted_models, "blacklisted_models")?;

    let mut sets = Vec::new();
    let mut bound = Vec::new();
    build_update_scalar_clauses(params, &mut sets, &mut bound);
    build_update_json_clauses(
        scopes_json,
        allowed_models_json,
        allowed_combos_json,
        blacklisted_providers_json,
        blacklisted_models_json,
        &mut sets,
        &mut bound,
    );

    if sets.is_empty() {
        return verify_api_key_exists(conn, id);
    }

    let mut sql = String::with_capacity(40 + sets.len() * 20);
    sql.push_str("UPDATE api_keys SET ");
    for (i, set) in sets.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str(set);
    }
    sql.push_str(" WHERE id = ?");
    bound.push(Box::new(id.0));
    let param_refs: Vec<&dyn rusqlite::ToSql> = bound
        .iter()
        .map(|b| b.as_ref() as &dyn rusqlite::ToSql)
        .collect();

    let affected = conn
        .execute(&sql, rusqlite::params_from_iter(param_refs))
        .map_err(|e| CoreError::Database {
            message: format!("update api_key {}: {e}", id.0),
            source: Some(std::sync::Arc::new(e)),
        })?;
    if affected == 0 {
        return Err(CoreError::Internal(format!("api_key {} not found", id.0)));
    }
    Ok(())
}

fn fetch_key_usage_stats(conn: &Connection, id: ApiKeyId) -> Result<(i64, i64, i64, f64)> {
    conn.query_row(
        "SELECT \
             COUNT(*), \
             COUNT(DISTINCT request_id), \
             SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END), \
             COALESCE(SUM(cost_usd), 0.0) \
         FROM usage WHERE api_key_id = ?1",
        params![id.0],
        |r| {
            let total: i64 = r.get(0)?;
            let unique: i64 = r.get(1)?;
            let errors: Option<i64> = r.get(2)?;
            let cost: f64 = r.get(3)?;
            Ok((total, unique, errors.unwrap_or(0), cost))
        },
    )
    .map_err(|e| CoreError::Database {
        message: format!("usage_summary for api_key {}: {e}", id.0),
        source: Some(std::sync::Arc::new(e)),
    })
}

pub fn usage_summary(conn: &Connection, id: ApiKeyId) -> Result<UsageSummary> {
    let row = fetch_key_usage_stats(conn, id)?;
    let last_used_at: Option<String> = conn
        .query_row(
            "SELECT last_used_at FROM api_keys WHERE id = ?1",
            params![id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| CoreError::Database {
            message: format!("select last_used_at for api_key {}: {e}", id.0),
            source: Some(std::sync::Arc::new(e)),
        })?
        .flatten();

    Ok(UsageSummary {
        total_rows: row.0.max(0) as u64,
        unique_requests: row.1.max(0) as u64,
        errors: row.2.max(0) as u64,
        total_cost_usd: row.3,
        last_used_at,
    })
}
