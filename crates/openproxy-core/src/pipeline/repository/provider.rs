use crate::error::Result;
use rusqlite::Connection;
use std::collections::HashMap;

pub fn get_providers_auth_type(
    conn: &Connection,
    provider_ids: &[crate::ids::ProviderId],
) -> Result<HashMap<String, String>> {
    let mut providers_map = HashMap::new();
    if provider_ids.is_empty() {
        return Ok(providers_map);
    }

    let placeholders = std::iter::repeat_n("?", provider_ids.len())
        .collect::<Vec<_>>()
        .join(",");

    let query = format!(
        "SELECT id, auth_type FROM providers WHERE id IN ({})",
        placeholders
    );
    if let Ok(mut stmt) = conn.prepare_cached(&query) {
        let ids: Vec<&dyn rusqlite::ToSql> = provider_ids
            .iter()
            .map(|id| &id.0 as &dyn rusqlite::ToSql)
            .collect();
        if let Ok(rows) = stmt.query_map(&*ids, |r| {
            let id: String = r.get(0)?;
            let auth_type_str: String = r.get(1)?;
            Ok((id, auth_type_str))
        }) {
            for row in rows.flatten() {
                providers_map.insert(row.0, row.1);
            }
        }
    }

    Ok(providers_map)
}
