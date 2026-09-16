use super::models::ApiKey;
use crate::ids::ApiKeyId;
use rusqlite::Row;
use std::fmt;

pub(crate) fn parse_required_json<T: serde::de::DeserializeOwned>(
    raw: &str,
    col_idx: usize,
    desc: &str,
) -> rusqlite::Result<T> {
    serde_json::from_str(raw).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            col_idx,
            rusqlite::types::Type::Text,
            Box::new(SimpleErr(format!("decode {desc}: {e}"))),
        )
    })
}

pub(crate) fn parse_optional_json<T: serde::de::DeserializeOwned>(
    raw: Option<String>,
    col_idx: usize,
    desc: &str,
) -> rusqlite::Result<Option<T>> {
    match raw {
        Some(s) if !s.is_empty() => Ok(Some(parse_required_json(&s, col_idx, desc)?)),
        _ => Ok(None),
    }
}

pub(crate) fn parse_json_filter(raw: Option<String>) -> Option<Vec<String>> {
    raw.filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
}

pub(crate) fn row_to_api_key(row: &Row<'_>) -> rusqlite::Result<ApiKey> {
    let id: i64 = row.get(0)?;
    let key_hash: String = row.get(1)?;
    let key_prefix: Option<String> = row.get(2)?;
    let label: Option<String> = row.get(3)?;
    let scopes_json: String = row.get(4)?;
    let allowed_models_json: Option<String> = row.get(5)?;
    let allowed_combos_json: Option<String> = row.get(6)?;
    let is_active: i64 = row.get(7)?;
    let revoked_at: Option<String> = row.get(8)?;
    let expires_at: Option<String> = row.get(9)?;
    let last_used_at: Option<String> = row.get(10)?;
    let created_at: String = row.get(11)?;
    let created_by: Option<String> = row.get(12)?;
    let blacklisted_providers_json: Option<String> = row.get(13)?;
    let blacklisted_models_json: Option<String> = row.get(14)?;

    let scopes: Vec<String> = parse_required_json(&scopes_json, 4, "scopes_json")?;
    let allowed_models = parse_optional_json(allowed_models_json, 5, "allowed_models_json")?;
    let allowed_combos = parse_optional_json(allowed_combos_json, 6, "allowed_combos_json")?;
    let blacklisted_providers = parse_json_filter(blacklisted_providers_json);
    let blacklisted_models = parse_json_filter(blacklisted_models_json);

    Ok(ApiKey {
        id: ApiKeyId(id),
        key_hash,
        key_prefix,
        label,
        scopes,
        allowed_models,
        allowed_combos,
        blacklisted_providers,
        blacklisted_models,
        is_active: is_active != 0,
        revoked_at,
        expires_at,
        last_used_at,
        created_at,
        created_by,
    })
}

#[derive(Debug)]
struct SimpleErr(String);
impl std::fmt::Display for SimpleErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SimpleErr {}
