//! Per-(account, model) "live-limited" sentinels.
//!
//! See `docs/specs/antigravity-gaps-p2.md` §4 (GAP-6) for the full
//! rationale. The four public functions are pure CRUD on the
//! `live_limited_models` table; callers are responsible for choosing
//! the TTL.

use openproxy_types::error::Result;
use openproxy_types::ids::{AccountId, ModelId};
use rusqlite::{Connection, OptionalExtension, params};

/// Centralised SQLite table name.
///
/// AGENTS.md §6 prohibits hardcoded table names in queries. Every
/// statement below references this constant so the migration, the
/// `MIGRATIONS` array, and any future `pragma_table_info` /
/// `DELETE FROM sqlite_sequence` calls all reference a single source
/// of truth.
pub const TABLE_LIVE_LIMITED: &str = "live_limited_models";

/// Insert (or refresh) a live-limit row.
///
/// `until_ts` is a chronologically-comparable RFC 3339 string (same
/// convention as `target_cooldowns.cooldown_until`).
pub fn mark_limited(
    conn: &Connection,
    account_id: AccountId,
    model_id: &ModelId,
    until_ts: &str,
    reason: &str,
) -> Result<()> {
    conn.execute(
        &format!(
            "INSERT INTO {TABLE_LIVE_LIMITED} (account_id, model_id, until_ts, reason) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(account_id, model_id) DO UPDATE SET \
                 until_ts = excluded.until_ts, \
                 reason   = excluded.reason"
        ),
        params![account_id.0, model_id.as_str(), until_ts, reason],
    )
    .map_err(crate::error::map_db_error)?;
    Ok(())
}

/// Remove all live-limit rows for an account whose `until_ts` has
/// already passed. Active rows (`until_ts > now`) are preserved so
/// that a `mark_limited` racing after a quota refresh is not silently
/// wiped (see §4.4 "Race condition" in
/// `docs/specs/antigravity-gaps-p2.md`).
///
/// Returns the number of rows deleted (0 if the account had no
/// expired rows).
pub fn clear_for_account(conn: &Connection, account_id: AccountId) -> Result<usize> {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let n = conn
        .execute(
            &format!(
                "DELETE FROM {TABLE_LIVE_LIMITED} \
                 WHERE account_id = ?1 AND until_ts <= ?2"
            ),
            params![account_id.0, now],
        )
        .map_err(crate::error::map_db_error)?;
    Ok(n)
}

/// `true` if `(account_id, model_id)` has a live-limit row whose
/// `until_ts` is still in the future. A row whose `until_ts` we
/// cannot parse is treated as inactive (defensive — better to let a
/// request through than to permanently exclude).
pub fn is_limited(
    conn: &Connection,
    account_id: AccountId,
    model_id: &ModelId,
) -> Result<bool> {
    let row: Option<String> = conn
        .query_row(
            &format!(
                "SELECT until_ts FROM {TABLE_LIVE_LIMITED} \
                 WHERE account_id = ?1 AND model_id = ?2"
            ),
            params![account_id.0, model_id.as_str()],
            |r| r.get(0),
        )
        .optional()
        .map_err(crate::error::map_db_error)?;
    let Some(until_str) = row else {
        return Ok(false);
    };
    let Ok(until_dt) = openproxy_types::timestamp::parse_timestamp(&until_str) else {
        return Ok(false);
    };
    Ok(chrono::Utc::now() < until_dt)
}

/// `true` if there is any row for `(account_id, model_id)`, regardless
/// of whether the `until_ts` has passed. Used by callers that need the
/// raw "exists" signal (e.g. observability / debugging).
pub fn has_row(
    conn: &Connection,
    account_id: AccountId,
    model_id: &ModelId,
) -> Result<bool> {
    let n: i64 = conn
        .query_row(
            &format!(
                "SELECT COUNT(*) FROM {TABLE_LIVE_LIMITED} \
                 WHERE account_id = ?1 AND model_id = ?2"
            ),
            params![account_id.0, model_id.as_str()],
            |r| r.get(0),
        )
        .map_err(crate::error::map_db_error)?;
    Ok(n > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations;
    use openproxy_types::ids::{AccountId, ModelId};
    use rusqlite::Connection;

    fn fresh_db() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory");
        migrations::run(&mut conn).expect("migrations");
        // Seed one provider + account so the FK on live_limited_models holds.
        conn.execute(
            "INSERT INTO providers(id, name, base_url, auth_type, format) \
             VALUES ('antigravity', 'Antigravity', 'https://x', 'oauth', 'openai')",
            [],
        )
        .expect("seed provider");
        conn.execute(
            "INSERT INTO accounts(provider_id, label) VALUES ('antigravity', 'a1')",
            [],
        )
        .expect("seed account");
        conn
    }

    #[test]
    fn mark_limited_then_is_limited() {
        let conn = fresh_db();
        let aid = AccountId(1);
        let mid = ModelId::new("gemini-2.5");
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        mark_limited(&conn, aid, &mid, &until, "RESOURCE_EXHAUSTED").expect("mark");
        assert!(is_limited(&conn, aid, &mid).expect("is_limited"));
        assert!(has_row(&conn, aid, &mid).expect("has_row"));
    }

    #[test]
    fn is_limited_false_when_until_in_past() {
        let conn = fresh_db();
        let aid = AccountId(1);
        let mid = ModelId::new("gemini-2.5");
        let until = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();
        mark_limited(&conn, aid, &mid, &until, "RESOURCE_EXHAUSTED").expect("mark");
        // Row exists, but the TTL has elapsed → NOT live-limited.
        assert!(!is_limited(&conn, aid, &mid).expect("is_limited"));
        assert!(has_row(&conn, aid, &mid).expect("has_row"));
    }

    #[test]
    fn clear_for_account_returns_rowcount() {
        // Two expired rows → cleared. A non-existent account → 0.
        let conn = fresh_db();
        let aid = AccountId(1);
        let m1 = ModelId::new("gemini-2.5");
        let m2 = ModelId::new("gemini-1.5");
        let expired = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        mark_limited(&conn, aid, &m1, &expired, "RESOURCE_EXHAUSTED").expect("mark1");
        mark_limited(&conn, aid, &m2, &expired, "RESOURCE_EXHAUSTED").expect("mark2");
        assert_eq!(clear_for_account(&conn, aid).expect("clear"), 2);
        assert_eq!(clear_for_account(&conn, aid).expect("clear again"), 0);
        assert_eq!(
            clear_for_account(&conn, AccountId(999)).expect("unknown"),
            0
        );
    }

    #[test]
    fn clear_for_account_preserves_active_rows() {
        // mark_limited after a refresh: the new row has a future until_ts.
        // clear_for_account must NOT delete it (race-correctness, N2 fix).
        let conn = fresh_db();
        let aid = AccountId(1);
        let mid = ModelId::new("gemini-2.5");
        let active = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        mark_limited(&conn, aid, &mid, &active, "RESOURCE_EXHAUSTED").expect("mark");
        assert_eq!(clear_for_account(&conn, aid).expect("clear"), 0);
        assert!(is_limited(&conn, aid, &mid).expect("still limited"));
        assert!(has_row(&conn, aid, &mid).expect("row remains"));
    }

    #[test]
    fn cascade_delete_removes_rows() {
        // ON DELETE CASCADE on the FK → removing the account drops the
        // live-limit rows.
        let conn = fresh_db();
        let aid = AccountId(1);
        let mid = ModelId::new("gemini-2.5");
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        mark_limited(&conn, aid, &mid, &until, "RESOURCE_EXHAUSTED").expect("mark");
        assert!(is_limited(&conn, aid, &mid).expect("is_limited"));
        conn.execute("DELETE FROM accounts WHERE id = 1", [])
            .expect("delete account");
        assert!(!is_limited(&conn, aid, &mid).expect("is_limited after"));
        assert!(!has_row(&conn, aid, &mid).expect("has_row after"));
    }

    // Compile-time witness: every SQL site in this module references the
    // constant instead of a literal "live_limited_models". This is a
    // documentation-level guarantee, but if someone re-introduces a
    // literal the test below won't catch it (Rust has no string-literal
    // lint for SQL); the constant is still the single source of truth.
    #[test]
    fn constant_matches_migration_filename() {
        assert_eq!(TABLE_LIVE_LIMITED, "live_limited_models");
        // The result type is `CoreError`; just confirm we can name it
        // so a future rename of CoreError doesn't silently break us.
        use openproxy_types::error::CoreError;
        let _ = CoreError::Validation("witness".into());
    }
}