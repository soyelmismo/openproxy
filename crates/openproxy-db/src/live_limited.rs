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

// ============================================================
// GAP-6: Adversarial tests for live_limited_models
// ============================================================
#[cfg(test)]
mod adversarial_tests {
    use super::*;
    use crate::migrations;
    use openproxy_types::ids::{AccountId, ModelId};
    use rusqlite::Connection;

    fn fresh_db() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open in-memory");
        migrations::run(&mut conn).expect("migrations");
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

    // --- mark_limited with various until_ts formats ---

    #[test]
    fn adv_mark_limited_rfc3339_with_offset() {
        // Timestamp with timezone offset (not UTC) — the comparison
        // logic should still work or gracefully fail to false.
        let conn = fresh_db();
        let aid = AccountId(1);
        let mid = ModelId::new("gemini-2.5");
        // In the future (Pacific +12)
        let until = "2099-12-31T23:59:59+12:00";
        mark_limited(&conn, aid, &mid, until, "RESOURCE_EXHAUSTED").expect("mark");
        // is_limited should still work — parse_timestamp converts +12:00 → UTC
        assert!(
            is_limited(&conn, aid, &mid).expect("is_limited"),
            "future timestamp with +12:00 offset should be active"
        );
    }

    #[test]
    fn adv_mark_limited_with_garbage_until_ts() {
        // Garbage string as until_ts — the DB accepts any string,
        // but is_limited() treats unparseable as inactive (defensive).
        let conn = fresh_db();
        let aid = AccountId(1);
        let mid = ModelId::new("gemini-2.5");
        mark_limited(&conn, aid, &mid, "not-a-date-at-all", "RESOURCE_EXHAUSTED").expect("mark");
        assert!(
            !is_limited(&conn, aid, &mid).expect("is_limited"),
            "garbage until_ts must be treated as inactive"
        );
        // But has_row must still be true (row exists even with garbage).
        assert!(
            has_row(&conn, aid, &mid).expect("has_row"),
            "row with garbage until_ts still exists"
        );
    }

    #[test]
    fn adv_mark_limited_with_empty_until_ts() {
        let conn = fresh_db();
        let aid = AccountId(1);
        let mid = ModelId::new("gemini-2.5");
        mark_limited(&conn, aid, &mid, "", "RESOURCE_EXHAUSTED").expect("mark");
        assert!(
            !is_limited(&conn, aid, &mid).expect("is_limited"),
            "empty until_ts must be treated as inactive"
        );
    }

    // --- clear_for_account edge cases ---

    #[test]
    fn adv_clear_for_account_unknown_account_returns_zero() {
        let conn = fresh_db();
        // AccountId(999) does not exist — no FK violation (just no rows).
        assert_eq!(
            clear_for_account(&conn, AccountId(999)).expect("clear"),
            0
        );
    }

    #[test]
    fn adv_clear_for_account_mixed_expired_and_active() {
        let conn = fresh_db();
        let aid = AccountId(1);
        let m1 = ModelId::new("gemini-2.5");
        let m2 = ModelId::new("gemini-1.5");
        let m3 = ModelId::new("gemini-pro");

        let expired = (chrono::Utc::now() - chrono::Duration::minutes(5)).to_rfc3339();
        let active = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();

        mark_limited(&conn, aid, &m1, &expired, "RESOURCE_EXHAUSTED").expect("m1");
        mark_limited(&conn, aid, &m2, &active, "RESOURCE_EXHAUSTED").expect("m2");
        mark_limited(&conn, aid, &m3, &expired, "RESOURCE_EXHAUSTED").expect("m3");

        let n = clear_for_account(&conn, aid).expect("clear");
        assert_eq!(n, 2, "only expired rows deleted");

        // Active row survives
        assert!(
            is_limited(&conn, aid, &m2).expect("m2 still active"),
            "active row must survive clear"
        );
        // Expired rows gone
        assert!(
            !is_limited(&conn, aid, &m1).expect("m1 gone"),
            "expired m1 must be deleted"
        );
        assert!(!has_row(&conn, aid, &m1).expect("m1 row gone"));
        assert!(
            !has_row(&conn, aid, &m3).expect("m3 row gone"),
            "expired m3 must be deleted"
        );
    }

    // --- mark_limited upserts on conflict ---

    #[test]
    fn adv_mark_limited_upsert_overwrites() {
        let conn = fresh_db();
        let aid = AccountId(1);
        let mid = ModelId::new("gemini-2.5");

        let t1 = (chrono::Utc::now() + chrono::Duration::minutes(1)).to_rfc3339();
        let t2 = (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339();

        mark_limited(&conn, aid, &mid, &t1, "REASON_A").expect("mark t1");
        mark_limited(&conn, aid, &mid, &t2, "REASON_B").expect("mark t2 (upsert)");

        // Should be t2's row now.
        let row: String = conn
            .query_row(
                "SELECT until_ts FROM live_limited_models WHERE account_id = ?1 AND model_id = ?2",
                params![aid.0, mid.as_str()],
                |r| r.get(0),
            )
            .expect("read back");
        assert_eq!(row, t2, "upsert must overwrite until_ts");
    }

    // --- ON DELETE CASCADE works for multiple rows ---

    #[test]
    fn adv_cascade_delete_clears_multiple_rows() {
        let conn = fresh_db();
        let aid = AccountId(1);
        let m1 = ModelId::new("model-a");
        let m2 = ModelId::new("model-b");
        let m3 = ModelId::new("model-c");
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();

        mark_limited(&conn, aid, &m1, &until, "X").expect("m1");
        mark_limited(&conn, aid, &m2, &until, "X").expect("m2");
        mark_limited(&conn, aid, &m3, &until, "X").expect("m3");

        assert!(has_row(&conn, aid, &m1).expect("all exist"));
        assert!(has_row(&conn, aid, &m2).expect("all exist"));
        assert!(has_row(&conn, aid, &m3).expect("all exist"));

        conn.execute("DELETE FROM accounts WHERE id = 1", [])
            .expect("delete account");

        assert!(!has_row(&conn, aid, &m1).expect("cascade m1"));
        assert!(!has_row(&conn, aid, &m2).expect("cascade m2"));
        assert!(!has_row(&conn, aid, &m3).expect("cascade m3"));
    }

    // --- is_limited on non-existent (account, model) ---

    #[test]
    fn adv_is_limited_nonexistent_returns_false() {
        let conn = fresh_db();
        assert!(
            !is_limited(&conn, AccountId(1), &ModelId::new("nonexistent"))
                .expect("no panic"),
            "non-existent pair must be false"
        );
    }

    // --- Multiple models on same account ---

    #[test]
    fn adv_different_models_independent_limited_state() {
        let conn = fresh_db();
        let aid = AccountId(1);
        let m1 = ModelId::new("model-a");
        let m2 = ModelId::new("model-b");

        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        mark_limited(&conn, aid, &m1, &until, "X").expect("m1");

        assert!(
            is_limited(&conn, aid, &m1).expect("m1 limited"),
            "m1 should be limited"
        );
        assert!(
            !is_limited(&conn, aid, &m2).expect("m2 not limited"),
            "m2 should NOT be limited"
        );
    }

    // --- clear_for_account only affects one account ---

    #[test]
    fn adv_clear_for_account_does_not_cross_accounts() {
        let conn = fresh_db();
        let a1 = AccountId(1);
        // Create a second account
        conn.execute(
            "INSERT INTO accounts(provider_id, label) VALUES ('antigravity', 'a2')",
            [],
        )
        .expect("seed a2");
        let a2 = AccountId(2);

        let mid = ModelId::new("gemini-2.5");
        let until = (chrono::Utc::now() - chrono::Duration::minutes(1)).to_rfc3339();

        mark_limited(&conn, a1, &mid, &until, "X").expect("mark a1");
        mark_limited(&conn, a2, &mid, &until, "X").expect("mark a2");

        let n = clear_for_account(&conn, a1).expect("clear a1");
        assert_eq!(n, 1, "only a1's expired row should be deleted, got {n}");

        // a1 is gone, a2 is still there (even though expired too)
        assert!(!has_row(&conn, a1, &mid).expect("a1 gone"));
        assert!(
            has_row(&conn, a2, &mid).expect("a2 still present"),
            "clear_for_account must not cross account boundaries"
        );
    }

    // --- mark_limited on non-existent account (FK violation) ---

    #[test]
    fn adv_mark_limited_nonexistent_account_fails() {
        let conn = fresh_db();
        let aid = AccountId(999);
        let mid = ModelId::new("model-x");
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        let result = mark_limited(&conn, aid, &mid, &until, "X");
        assert!(
            result.is_err(),
            "mark_limited on non-existent account must fail (FK constraint)"
        );
    }

    // --- until_ts boundary: exactly now ---

    #[test]
    fn adv_until_ts_exactly_now_is_expired() {
        // "until_ts <= now" → if until_ts equals now, the row is expired.
        // We can't guarantee exact timing, but we use a timestamp in the past
        // to test the boundary condition.
        let conn = fresh_db();
        let aid = AccountId(1);
        let mid = ModelId::new("gemini-2.5");
        let until = (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339();
        mark_limited(&conn, aid, &mid, &until, "X").expect("mark");
        // 1 second in the past → should be expired
        assert!(!is_limited(&conn, aid, &mid).expect("1s-past is expired"));
    }

    // --- Stress: mark_limited same (account, model) many times ---

    #[test]
    fn adv_mark_limited_same_pair_stress() {
        let conn = fresh_db();
        let aid = AccountId(1);
        let mid = ModelId::new("gemini-2.5");

        for i in 0..100 {
            let until = (chrono::Utc::now()
                + chrono::Duration::minutes(i))
                .to_rfc3339();
            mark_limited(&conn, aid, &mid, &until, "X").expect("mark");
        }
        // UPSERT means only 1 row exists, with the latest until_ts.
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM live_limited_models WHERE account_id = ?1 AND model_id = ?2",
                params![aid.0, mid.as_str()],
                |r| r.get(0),
            )
            .expect("count");
        assert_eq!(count, 1, "UPSERT must keep exactly 1 row, got {count}");
    }
}