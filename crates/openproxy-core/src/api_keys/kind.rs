//! Customer versus personal API keys.
//!
//! `customer` keys are the ones sold through the checkout and governed by a
//! plan. `personal` keys are the operator's own keys for private consumption.
//! Scopes cannot tell them apart (a customer key also carries `chat`), so the
//! distinction lives in its own column and the dashboard switches on it.

use crate::error::{CoreError, Result};
use rusqlite::Connection;
use serde::Serialize;

pub const KIND_PERSONAL: &str = "personal";
pub const KIND_CUSTOMER: &str = "customer";

/// One key's classification, as listed for the dashboard switches.
#[derive(Debug, Clone, Serialize)]
pub struct ApiKeyKindRow {
    pub id: i64,
    pub kind: String,
}

/// Accept only the two valid kinds, trimmed. Anything else is a 400 instead of
/// silently storing a value no query would ever match.
pub fn normalize(raw: &str) -> Result<&'static str> {
    match raw.trim() {
        KIND_PERSONAL => Ok(KIND_PERSONAL),
        KIND_CUSTOMER => Ok(KIND_CUSTOMER),
        other => Err(CoreError::Validation(format!(
            "api key kind must be 'personal' or 'customer', got '{other}'"
        ))),
    }
}

/// Classify a key. Returns the stored kind.
pub fn set(conn: &Connection, id: i64, kind: &str) -> Result<&'static str> {
    let kind = normalize(kind)?;
    let changed = conn
        .execute(
            "UPDATE api_keys SET kind = ?1 WHERE id = ?2",
            rusqlite::params![kind, id],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    if changed == 0 {
        return Err(CoreError::NotFound {
            what: "api key".into(),
            id: id.to_string(),
        });
    }
    Ok(kind)
}

/// Every key's kind, ordered by id. The dashboard merges this small map into
/// the key list instead of growing the list payload for both views.
pub fn list(conn: &Connection) -> Result<Vec<ApiKeyKindRow>> {
    let mut stmt = conn
        .prepare("SELECT id, kind FROM api_keys ORDER BY id")
        .map_err(openproxy_db::error::map_db_error)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(ApiKeyKindRow {
                id: row.get(0)?,
                kind: row.get(1)?,
            })
        })
        .map_err(openproxy_db::error::map_db_error)?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(openproxy_db::error::map_db_error)
}

/// Kind a freshly created key starts with, derived from its creator stamp:
/// the checkout sells keys, so its keys are customers from birth. Manual keys
/// start personal and can be flipped from the dashboard.
pub fn kind_for_creator(created_by: &str) -> &'static str {
    if created_by.starts_with("credits-checkout:") {
        KIND_CUSTOMER
    } else {
        KIND_PERSONAL
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_accepts_only_the_two_kinds() {
        assert_eq!(normalize("personal").unwrap(), KIND_PERSONAL);
        assert_eq!(normalize(" customer ").unwrap(), KIND_CUSTOMER);
        assert!(normalize("admin").is_err());
        assert!(normalize("").is_err());
    }

    #[test]
    fn checkout_keys_are_customers_by_creator() {
        assert_eq!(kind_for_creator("credits-checkout:ORD-1"), KIND_CUSTOMER);
        assert_eq!(kind_for_creator("admin"), KIND_PERSONAL);
    }
}
