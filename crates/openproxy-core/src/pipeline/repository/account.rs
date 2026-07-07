use crate::error::Result;
use rusqlite::Connection;
use std::collections::HashMap;

pub struct RawAccount {
    pub api_key_encrypted: Option<Vec<u8>>,
    pub label: Option<String>,
    pub access_token_encrypted: Option<Vec<u8>>,
    pub refresh_token_encrypted: Option<Vec<u8>>,
    pub expires_at: Option<String>,
}

pub struct KiroMeta {
    pub region: Option<String>,
    pub profile_arn: Option<String>,
}

pub fn get_accounts_meta(
    conn: &Connection,
    account_ids: &[crate::ids::AccountId],
) -> Result<(
    HashMap<i64, RawAccount>,
    HashMap<i64, KiroMeta>,
    HashMap<i64, String>,
)> {
    let mut accounts_map = HashMap::new();
    let mut kiro_map = HashMap::new();
    let mut antigravity_map = HashMap::new();

    if account_ids.is_empty() {
        return Ok((accounts_map, kiro_map, antigravity_map));
    }

    let placeholders = std::iter::repeat_n("?", account_ids.len())
        .collect::<Vec<_>>()
        .join(",");

    let query = format!(
        "SELECT id, api_key_encrypted, label, access_token_encrypted, refresh_token_encrypted, expires_at FROM accounts WHERE id IN ({})",
        placeholders
    );
    if let Ok(mut stmt) = conn.prepare_cached(&query) {
        let ids: Vec<&dyn rusqlite::ToSql> = account_ids
            .iter()
            .map(|id| &id.0 as &dyn rusqlite::ToSql)
            .collect();
        if let Ok(rows) = stmt.query_map(&*ids, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<Vec<u8>>>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<Vec<u8>>>(3)?,
                r.get::<_, Option<Vec<u8>>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        }) {
            for row in rows.flatten() {
                accounts_map.insert(
                    row.0,
                    RawAccount {
                        api_key_encrypted: row.1,
                        label: row.2,
                        access_token_encrypted: row.3,
                        refresh_token_encrypted: row.4,
                        expires_at: row.5,
                    },
                );
            }
        }
    }

    // Kiro
    let kiro_query = format!(
        "SELECT account_id, region, profile_arn FROM executor_kiro WHERE account_id IN ({})",
        placeholders
    );
    if let Ok(mut stmt) = conn.prepare_cached(&kiro_query) {
        let ids: Vec<&dyn rusqlite::ToSql> = account_ids
            .iter()
            .map(|id| &id.0 as &dyn rusqlite::ToSql)
            .collect();
        if let Ok(rows) = stmt.query_map(&*ids, |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        }) {
            for row in rows.flatten() {
                kiro_map.insert(
                    row.0,
                    KiroMeta {
                        region: row.1,
                        profile_arn: row.2,
                    },
                );
            }
        }
    }

    // Antigravity
    let ag_query = format!(
        "SELECT account_id, project_id FROM executor_antigravity WHERE account_id IN ({})",
        placeholders
    );
    if let Ok(mut stmt) = conn.prepare_cached(&ag_query) {
        let ids: Vec<&dyn rusqlite::ToSql> = account_ids
            .iter()
            .map(|id| &id.0 as &dyn rusqlite::ToSql)
            .collect();
        if let Ok(rows) =
            stmt.query_map(&*ids, |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        {
            for row in rows.flatten() {
                antigravity_map.insert(row.0, row.1);
            }
        }
    }

    Ok((accounts_map, kiro_map, antigravity_map))
}
