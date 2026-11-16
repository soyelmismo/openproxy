//! Introspection, dynamic column extraction and table blacklist for automatic backups.

use openproxy_db::error::map_db_error;
use openproxy_types::Result;
use rusqlite::Connection;
use std::collections::{BTreeMap, HashSet};

/// Tables that should NEVER be exported in a backup (ephemeral data, telemetry, caches, migrations).
pub const TABLE_BLACKLIST: &[&str] = &[
    "schema_migrations",
    "sqlite_sequence",
    "usage",
    "usage_journal",
    "smart_warmup_history",
    "provider_favicons",
    "notifications",
    "oauth_device_tickets",
    "target_cooldowns",
    "provider_proxy_cooldowns",
    "free_proxies",
    "live_limited_models",
    "model_capabilities_sync",
];

/// Known core tables that have structured Backup structs.
pub const CORE_TABLES: &[&str] = &[
    "providers",
    "accounts",
    "models",
    "combos",
    "combo_targets",
    "proxy_sources",
    "api_keys",
    "app_config",
];

/// Fetch all column names for a table using `PRAGMA table_info`.
pub fn get_table_columns(conn: &Connection, table_name: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info(\"{table_name}\")"))
        .map_err(map_db_error)?;

    let cols = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(map_db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_db_error)?;

    Ok(cols)
}

/// Convert a rusqlite ValueRef into a serde_json::Value.
pub fn val_ref_to_json(val: rusqlite::types::ValueRef<'_>) -> serde_json::Value {
    match val {
        rusqlite::types::ValueRef::Null => serde_json::Value::Null,
        rusqlite::types::ValueRef::Integer(i) => serde_json::json!(i),
        rusqlite::types::ValueRef::Real(f) => serde_json::json!(f),
        rusqlite::types::ValueRef::Text(t) => {
            let s = String::from_utf8_lossy(t);
            serde_json::Value::String(s.into_owned())
        }
        rusqlite::types::ValueRef::Blob(b) => {
            // Encode binary blob as base64 string
            use base64::Engine;
            let encoded = base64::engine::general_purpose::STANDARD.encode(b);
            serde_json::Value::String(format!("base64:{encoded}"))
        }
    }
}

/// Read all columns not in `known_columns` for all rows in `table_name`, keyed by primary key or row order.
/// Returns a map of `id/primary_key` -> `BTreeMap<String, serde_json::Value>`.
pub fn export_extra_columns_by_pk(
    conn: &Connection,
    table_name: &str,
    pk_column: &str,
    known_columns: &[&str],
) -> Result<BTreeMap<String, BTreeMap<String, serde_json::Value>>> {
    let all_cols = get_table_columns(conn, table_name)?;
    let known_set: HashSet<&str> = known_columns.iter().copied().collect();
    let extra_cols: Vec<String> = all_cols
        .into_iter()
        .filter(|c| !known_set.contains(c.as_str()))
        .collect();

    let mut result = BTreeMap::new();
    if extra_cols.is_empty() {
        return Ok(result);
    }

    let cols_sql = format!(
        "SELECT \"{}\", {} FROM \"{}\"",
        pk_column,
        extra_cols
            .iter()
            .map(|c| format!("\"{c}\""))
            .collect::<Vec<_>>()
            .join(", "),
        table_name
    );

    let mut stmt = conn.prepare(&cols_sql).map_err(map_db_error)?;
    let mut rows = stmt.query([]).map_err(map_db_error)?;

    while let Some(row) = rows.next().map_err(map_db_error)? {
        let pk_val = val_ref_to_json(row.get_ref(0).map_err(map_db_error)?);
        let pk_str = match pk_val {
            serde_json::Value::String(s) => s,
            serde_json::Value::Number(n) => n.to_string(),
            _ => continue,
        };

        let mut row_extra = BTreeMap::new();
        for (idx, col_name) in extra_cols.iter().enumerate() {
            let val = val_ref_to_json(row.get_ref(idx + 1).map_err(map_db_error)?);
            if !val.is_null() {
                row_extra.insert(col_name.clone(), val);
            }
        }

        if !row_extra.is_empty() {
            result.insert(pk_str, row_extra);
        }
    }

    Ok(result)
}

/// Security-critical and primary-key columns that `restore_extra_columns` must
/// never overwrite after structured `restore_payload` sanitization/encryption.
pub const PROTECTED_EXTRA_COLUMNS: &[&str] = &[
    "id",
    "key",
    "api_key_encrypted",
    "access_token_encrypted",
    "refresh_token_encrypted",
    "oauth_provider_specific",
    "key_hash",
    "allow_loopback",
];

/// Dynamic restore of extra columns on an existing row via UPDATE if column exists in target table.
pub fn restore_extra_columns(
    tx: &rusqlite::Transaction<'_>,
    table_name: &str,
    pk_col: &str,
    pk_val: &rusqlite::types::ToSqlOutput<'_>,
    extra_fields: &BTreeMap<String, serde_json::Value>,
    existing_table_cols: &HashSet<String>,
) -> Result<()> {
    let safe_table = table_name.replace('"', "\"\"");
    let safe_pk = pk_col.replace('"', "\"\"");
    for (col_name, json_val) in extra_fields {
        // Never allow extra_fields to overwrite primary keys, encrypted credentials, or SSRF flags
        if col_name == pk_col || PROTECTED_EXTRA_COLUMNS.contains(&col_name.as_str()) {
            continue;
        }
        // Only attempt to set if the column actually exists in the local database schema
        if !existing_table_cols.contains(col_name) {
            continue;
        }

        let safe_col = col_name.replace('"', "\"\"");
        let update_sql =
            format!("UPDATE \"{safe_table}\" SET \"{safe_col}\" = ?1 WHERE \"{safe_pk}\" = ?2");

        match json_val {
            serde_json::Value::Null => {
                tx.execute(
                    &update_sql,
                    rusqlite::params![rusqlite::types::Null, pk_val],
                )
                .map_err(map_db_error)?;
            }
            serde_json::Value::Bool(b) => {
                tx.execute(&update_sql, rusqlite::params![*b as i64, pk_val])
                    .map_err(map_db_error)?;
            }
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    tx.execute(&update_sql, rusqlite::params![i, pk_val])
                        .map_err(map_db_error)?;
                } else if let Some(f) = n.as_f64() {
                    tx.execute(&update_sql, rusqlite::params![f, pk_val])
                        .map_err(map_db_error)?;
                }
            }
            serde_json::Value::String(s) => {
                tx.execute(&update_sql, rusqlite::params![s, pk_val])
                    .map_err(map_db_error)?;
            }
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                let serialized = json_val.to_string();
                tx.execute(&update_sql, rusqlite::params![serialized, pk_val])
                    .map_err(map_db_error)?;
            }
        }
    }

    Ok(())
}

/// Dynamic restore of extra unmapped tables.
pub fn restore_extra_tables(
    tx: &rusqlite::Transaction<'_>,
    extra_tables: &BTreeMap<String, Vec<serde_json::Value>>,
) -> Result<()> {
    for (table_name, rows) in extra_tables {
        // Security (audit H-15): the blacklist guarded the EXPORT side only —
        // a crafted or replayed bundle could write rows into tables that are
        // excluded by design (telemetry, ephemeral state, device tickets),
        // forging internal state. The restore honors the same policy.
        if TABLE_BLACKLIST.contains(&table_name.as_str()) {
            continue;
        }
        let cols = match get_table_columns(tx, table_name) {
            Ok(c) if !c.is_empty() => c,
            _ => continue, // Target DB does not have this table, skip gracefully
        };
        let col_set: HashSet<String> = cols.into_iter().collect();

        for row in rows {
            let Some(obj) = row.as_object() else {
                continue;
            };

            let matching_cols: Vec<String> = obj
                .keys()
                .filter(|k| col_set.contains(*k))
                .cloned()
                .collect();

            if matching_cols.is_empty() {
                continue;
            }

            let placeholders: Vec<String> =
                (1..=matching_cols.len()).map(|i| format!("?{i}")).collect();
            // Security (audit H-15): bundle-controlled identifiers are
            // quoted with doubled embedded quotes, so a table or column name
            // carrying `"` cannot escape its quoted position in the SQL text.
            // (The PRAGMA table_info gate and rusqlite's single-statement
            // prepare already reject multi-statement payloads; this closes
            // the remaining identifier-injection surface for good.)
            let quoted_table = table_name.replace('"', "\"\"");
            let insert_sql = format!(
                "INSERT OR REPLACE INTO \"{quoted_table}\" ({}) VALUES ({})",
                matching_cols
                    .iter()
                    .map(|c| format!("\"{}\"", c.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(", "),
                placeholders.join(", ")
            );

            let mut stmt = tx.prepare(&insert_sql).map_err(map_db_error)?;
            let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

            for col in &matching_cols {
                let val = &obj[col];
                match val {
                    serde_json::Value::Null => params_vec.push(Box::new(rusqlite::types::Null)),
                    serde_json::Value::Bool(b) => params_vec.push(Box::new(*b as i64)),
                    serde_json::Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            params_vec.push(Box::new(i));
                        } else if let Some(f) = n.as_f64() {
                            params_vec.push(Box::new(f));
                        } else {
                            params_vec.push(Box::new(rusqlite::types::Null));
                        }
                    }
                    serde_json::Value::String(s) => {
                        // ROOT FIX (asymmetric serialization, audit A6-02):
                        // the export path encodes every BLOB as the string
                        // "base64:<…>" (see `val_ref_to_json`), but the
                        // restore used to insert that marker string as TEXT.
                        // Money-table blobs — above all the AES-GCM ciphertext
                        // of an anonymous order's minted key — came back as
                        // text, so after a routine backup/restore the reveal
                        // of an already-PAID order failed ("Invalid column
                        // type Text"), leaving the buyer with no self-service
                        // access to what they paid for. Decode the marker
                        // symmetrically; a failed decode keeps the string as
                        // it was so non-blob text columns are unaffected.
                        if let Some(encoded) = s.strip_prefix("base64:") {
                            use base64::Engine;
                            match base64::engine::general_purpose::STANDARD.decode(encoded) {
                                Ok(bytes) => {
                                    params_vec.push(Box::new(bytes));
                                }
                                Err(_) => {
                                    tracing::warn!(
                                        column = %col,
                                        "backup restore: invalid base64 blob marker kept as text"
                                    );
                                    params_vec.push(Box::new(s.clone()));
                                }
                            }
                        } else {
                            params_vec.push(Box::new(s.clone()));
                        }
                    }
                    serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                        params_vec.push(Box::new(val.to_string()));
                    }
                }
            }

            let slice: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
            let _ = stmt.execute(slice.as_slice());
        }
    }

    Ok(())
}

/// Export any table in the database that is not in `TABLE_BLACKLIST` and not in `CORE_TABLES`.
pub fn export_extra_tables(conn: &Connection) -> Result<BTreeMap<String, Vec<serde_json::Value>>> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
        .map_err(map_db_error)?;

    let all_tables = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(map_db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_db_error)?;

    let blacklist: HashSet<&str> = TABLE_BLACKLIST.iter().copied().collect();
    let core_set: HashSet<&str> = CORE_TABLES.iter().copied().collect();

    let mut extra_tables = BTreeMap::new();

    for tbl in all_tables {
        if blacklist.contains(tbl.as_str()) || core_set.contains(tbl.as_str()) {
            continue;
        }

        let cols = get_table_columns(conn, &tbl)?;
        if cols.is_empty() {
            continue;
        }

        let select_sql = format!(
            "SELECT {} FROM \"{}\"",
            cols.iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", "),
            tbl
        );

        let mut query_stmt = conn.prepare(&select_sql).map_err(map_db_error)?;
        let mut rows = query_stmt.query([]).map_err(map_db_error)?;
        let mut row_list = Vec::new();

        while let Some(row) = rows.next().map_err(map_db_error)? {
            let mut obj = serde_json::Map::new();
            for (idx, col) in cols.iter().enumerate() {
                let v = val_ref_to_json(row.get_ref(idx).map_err(map_db_error)?);
                obj.insert(col.clone(), v);
            }
            row_list.push(serde_json::Value::Object(obj));
        }

        if !row_list.is_empty() {
            extra_tables.insert(tbl, row_list);
        }
    }

    Ok(extra_tables)
}

#[cfg(test)]
mod restore_hardening_tests {
    use super::*;

    /// Audit H-15: blacklisted tables (telemetry, ephemeral state, device
    /// tickets) must be skipped on RESTORE, not only on export — a crafted
    /// bundle could otherwise forge rows in tables the backup design
    /// excludes.
    #[test]
    fn blacklisted_tables_are_skipped_on_restore() {
        let mut conn = Connection::open_in_memory().expect("open");
        conn.execute_batch(
            "CREATE TABLE usage (id INTEGER PRIMARY KEY, payload TEXT);
             CREATE TABLE oauth_device_tickets (ticket TEXT PRIMARY KEY);",
        )
        .expect("schema");
        let tx = conn.transaction().expect("tx");

        let mut bundle = BTreeMap::new();
        bundle.insert(
            "usage".to_string(),
            vec![serde_json::json!({"id": 1, "payload": "forged"})],
        );
        bundle.insert(
            "oauth_device_tickets".to_string(),
            vec![serde_json::json!({"ticket": "forged"})],
        );

        restore_extra_tables(&tx, &bundle).expect("restore");
        tx.commit().expect("commit");

        let usage_rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
            .expect("count");
        assert_eq!(
            usage_rows, 0,
            "blacklisted table 'usage' must not be written"
        );
        let tickets: i64 = conn
            .query_row("SELECT COUNT(*) FROM oauth_device_tickets", [], |r| {
                r.get(0)
            })
            .expect("count");
        assert_eq!(tickets, 0, "blacklisted table must not be written");
    }

    /// Audit H-15: bundle-controlled identifiers with embedded double quotes
    /// must be rejected (PRAGMA gate) — never interpolated raw into the SQL.
    #[test]
    fn quote_carrying_table_names_are_rejected_not_injected() {
        let mut conn = Connection::open_in_memory().expect("open");
        conn.execute_batch("CREATE TABLE real_table (id INTEGER PRIMARY KEY);")
            .expect("schema");
        let tx = conn.transaction().expect("tx");

        let mut bundle = BTreeMap::new();
        bundle.insert(
            "x\") VALUES (1); DROP TABLE real_table; --".to_string(),
            vec![serde_json::json!({"id": 1})],
        );

        // The PRAGMA gate cannot parse the payload as a table, so the table
        // is skipped gracefully — and real_table must survive untouched.
        restore_extra_tables(&tx, &bundle).expect("restore");
        tx.commit().expect("commit");
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='real_table'",
                [],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(tables, 1, "no SQL injection through identifiers");
    }
}
