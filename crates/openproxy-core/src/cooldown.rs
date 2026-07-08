//! Per-target cooldown registry, persisted in SQLite.
//!
//! When a target fails with a retryable error (5xx, 429, timeout, or
//! connection error — see [`crate::retry::RetryPolicy::is_retryable`]),
//! the pipeline records the failure here and skips the target on
//! subsequent requests until `cooldown_until` is in the past.
//!
//! The in-memory [`crate::circuit_breaker::CircuitBreakerRegistry`]
//! also de-routes unhealthy accounts, but it is per-process and
//! keyed on the *account*, not the *target*. The cooldown registry
//! complements it on three axes:
//!
//! - **Persistence**: the cooldown survives a process restart so a
//!   flapping upstream doesn't get re-tried immediately when the
//!   server comes back up.
//! - **Per-target granularity**: a target is `(provider, model,
//!   account)` — a single account can serve many targets with
//!   different cooldowns. A 4xx on one model shouldn't take down the
//!   account's other models.
//! - **Operator visibility**: the dashboard reads this table to show
//!   a "⏸ cooldown" badge on each parked target and offers a
//!   "Reset cooldown" button to force-clear a row.
//!
//! The cooldown is **target-scoped, not combo-scoped**: a sub-combo's
//! children are independent. The "exhaust a sub-combo before
//! marking the parent" semantic falls out of the existing flatten
//! step in [`crate::pipeline::Pipeline::flatten_targets`] — each
//! flattened child target can enter cooldown on its own, and the
//! parent only surfaces `NoHealthyTargets` when every child of every
//! sub-combo is in cooldown.

use crate::combos::CooldownMode;
use crate::error::{CoreError, Result};
use crate::ids::{ComboId, ComboTargetId};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// In-memory snapshot of one row of `target_cooldowns`. Surfaced by
/// [`list_for_combo`] so the dashboard can render the "⏸ cooldown"
/// badge with the reason string and the absolute expiry timestamp
/// (no per-row format code on the frontend).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Cooldown {
    pub combo_target_id: ComboTargetId,
    pub cooldown_until: DateTime<Utc>,
    pub reason: Option<String>,
    pub failure_count: u32,
}

/// Return `true` if the target has an active cooldown row
/// (`cooldown_until > now`).
///
/// "Active" is the right word: an expired row is still in the table
/// until [`prune_expired`] sweeps it (or the pipeline
/// overwrites/clears it on the next attempt). Until that sweep
/// runs, the row exists but is not honored.
///
/// The `cooldown_until` column is stored as an RFC 3339 string (with
/// the `T` separator and a `+00:00` timezone). To compare it to
/// "now" without the lexicographic surprises of a literal string
/// compare (`T` > ` ` in ASCII, so a `T`-shaped string sorts
/// after a space-shaped `datetime('now')` regardless of actual
/// time), we wrap both sides in SQLite's `datetime()` function,
/// which parses both shapes and returns a canonical
/// `YYYY-MM-DD HH:MM:SS` form on which the comparison is correct.
pub fn is_in_cooldown(conn: &Connection, target_id: ComboTargetId) -> Result<bool> {
    let now = Utc::now();
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM target_cooldowns
             WHERE combo_target_id = ?1
               AND datetime(cooldown_until) > datetime(?2)",
            params![target_id.0, now.to_rfc3339()],
            |r| r.get(0),
        )
        .map_err(crate::error::map_db_error_ctx(format!(
            "is_in_cooldown({})",
            target_id.0
        )))?;
    Ok(count > 0)
}

/// Park `target_id` in cooldown for `cooldown_secs` from now. UPSERT
/// semantics: a second call on the same target bumps `failure_count`
/// and resets `cooldown_until` — so a target that keeps failing
/// stays parked for the configured window from the *last* failure,
/// not from the first one. This is what the spec calls the
/// "exponential-ish" behavior: the cooldown doesn't grow with
/// `failure_count`, but a flapping target that gets tested every
/// `cooldown_secs` will keep getting parked because every probe
/// re-fires the UPSERT.
///
/// Legacy entry point retained for callers that don't have a
/// [`CooldownMode`] in hand. Delegates to [`record_failure_with_mode`]
/// with `mode = Flat`, `max_secs = cooldown_secs`, `factor = 1` —
/// i.e. the pre-migration-000035 behavior (always park for exactly
/// `cooldown_secs`).
pub fn record_failure(
    conn: &Connection,
    target_id: ComboTargetId,
    reason: &str,
    cooldown_secs: u64,
) -> Result<()> {
    record_failure_with_mode(
        conn,
        target_id,
        reason,
        CooldownMode::Flat,
        cooldown_secs,
        // `max_secs = cooldown_secs` makes the cap a no-op for flat
        // mode (the computed value is already `<= max_secs`).
        cooldown_secs,
        // `factor = 1` makes the exponential formula collapse to
        // `base_secs * 1^n = base_secs`, so flat and exponential
        // produce identical results when `factor = 1`.
        1,
    )
}

/// Park `target_id` in cooldown, choosing the duration based on the
/// combo's [`CooldownMode`] and the per-combo overrides.
///
/// For [`CooldownMode::Flat`]: `cooldown_until = now + base_secs`.
/// This is the legacy behavior — every retryable failure parks the
/// target for the same flat window.
///
/// For [`CooldownMode::Exponential`]: read the *current*
/// `failure_count` from the existing row (0 if there is no row
/// yet), compute
/// `cooldown_secs = min(base_secs * factor^failure_count, max_secs)`,
/// then UPSERT with the computed `cooldown_until` and the
/// incremented `failure_count`. The `failure_count - 1` exponent
/// from the spec is realized as "read the count BEFORE the UPSERT
/// so the first failure (count=0 → exponent=0) parks for
/// `base_secs * factor^0 = base_secs`, the second failure
/// (count=1 → exponent=1) parks for `base_secs * factor`, etc.
///
/// The `base_secs` / `max_secs` / `factor` parameters are the
/// *resolved* values: the caller is responsible for picking the
/// combo-level override or the global `[cooldown]` default before
/// calling. Passing the resolved values in (instead of the combo
/// struct) keeps this function's signature stable and lets the
/// pipeline reuse it for the legacy flat path without a per-call
/// DB lookup.
///
/// UPSERT semantics: a second call on the same target bumps
/// `failure_count` and resets `cooldown_until` to the newly-
/// computed value. The spec calls this "exponential-ish": the
/// window grows with `failure_count`, but a target that comes
/// back online and is then re-tried successfully is un-parked by
/// [`clear`] on the success path.
pub fn record_failure_with_mode(
    conn: &Connection,
    target_id: ComboTargetId,
    reason: &str,
    mode: CooldownMode,
    base_secs: u64,
    max_secs: u64,
    factor: u32,
) -> Result<()> {
    // Compute the cooldown duration based on the mode. For Flat we
    // always use `base_secs`; for Exponential we read the current
    // `failure_count` BEFORE the UPSERT and grow the window by
    // `factor^failure_count`, capped at `max_secs`.
    let cooldown_secs: u64 = match mode {
        CooldownMode::Flat => base_secs,
        CooldownMode::Exponential => {
            // Read the current `failure_count`. A missing row (no
            // prior failure) reads back as 0, which collapses the
            // formula to `base_secs * factor^0 = base_secs` — the
            // same as the first flat cooldown. That's the spec's
            // intent: "the first failure is `base_secs`, each
            // subsequent failure multiplies by `factor`".
            let current_count: u64 = conn
                .query_row(
                    "SELECT failure_count FROM target_cooldowns \
                     WHERE combo_target_id = ?1",
                    params![target_id.0],
                    |r| r.get::<_, i64>(0),
                )
                .optional()
                .map_err(|e| CoreError::Database {
                    message: format!(
                        "record_failure_with_mode: read failure_count({}): {}",
                        target_id.0, e
                    ),
                    source: Some(Box::new(e)),
                })?
                .map(|v: i64| v.max(0) as u64)
                .unwrap_or(0);

            // `factor^current_count`, defended against overflow and
            // a zero factor (which would collapse every cooldown
            // after the first to 0). A zero factor is treated as 1
            // so the exponential mode degrades gracefully to flat
            // if the operator (or a hand-edited row) sets `factor
            // = 0`.
            let f = if factor == 0 { 1u32 } else { factor };
            let multiplier = checked_pow_u64(f, current_count);
            let grown = base_secs.saturating_mul(multiplier);
            grown.min(max_secs)
        }
    };

    let until = Utc::now() + chrono::Duration::seconds(cooldown_secs as i64);
    conn.execute(
        "INSERT INTO target_cooldowns (combo_target_id, cooldown_until, reason, failure_count, created_at, updated_at)
         VALUES (?1, ?2, ?3, 1, datetime('now'), datetime('now'))
         ON CONFLICT(combo_target_id) DO UPDATE SET
           cooldown_until = excluded.cooldown_until,
           reason = excluded.reason,
           failure_count = failure_count + 1,
           updated_at = datetime('now')",
        params![target_id.0, until.to_rfc3339(), reason],
    )
    .map_err(crate::error::map_db_error_ctx(format!("record_failure_with_mode({})", target_id.0)))?;
    Ok(())
}

/// Compute `base^exp` for `u64` with saturating arithmetic. Used by
/// [`record_failure_with_mode`] to grow the cooldown window with
/// `failure_count`. Capped at `u64::MAX` so an out-of-control
/// `failure_count` (e.g. a flapping target that's been parked for
/// hours) doesn't overflow and panic.
fn checked_pow_u64(base: u32, exp: u64) -> u64 {
    let mut acc: u64 = 1;
    let mut b = base as u64;
    let mut e = exp;
    while e > 0 {
        if e & 1 == 1 {
            acc = acc.saturating_mul(b);
        }
        e >>= 1;
        if e > 0 {
            b = b.saturating_mul(b);
        }
    }
    acc
}

/// Remove any active cooldown for `target_id`. Called on the success
/// path so a target that comes back online is no longer parked.
///
/// Idempotent: a missing row is a silent no-op (the delete returns
/// 0 affected rows, which is fine). The function does not check
/// whether the row was actually active — the operator may also
/// call this from the dashboard's "Reset cooldown" button to clear
/// an entry that's still in its window.
pub fn clear(conn: &Connection, target_id: ComboTargetId) -> Result<()> {
    conn.execute(
        "DELETE FROM target_cooldowns WHERE combo_target_id = ?1",
        params![target_id.0],
    )
    .map_err(crate::error::map_db_error_ctx(format!(
        "clear({})",
        target_id.0
    )))?;
    Ok(())
}

/// Sweep expired cooldowns. Returns the number of rows deleted.
///
/// Cheap enough to run on a 60-second timer from the server's
/// startup. The index on `cooldown_until` keeps the scan a tight
/// range even with thousands of parked targets.
///
/// Like [`is_in_cooldown`], the comparison wraps both sides in
/// `datetime()` to dodge the lexicographic surprise of comparing
/// RFC 3339 strings against `datetime('now')`.
pub fn prune_expired(conn: &Connection) -> Result<usize> {
    let now = Utc::now();
    let n = conn
        .execute(
            "DELETE FROM target_cooldowns WHERE datetime(cooldown_until) <= datetime(?1)",
            params![now.to_rfc3339()],
        )
        .map_err(crate::error::map_db_error)?;
    Ok(n)
}

/// List active cooldowns for every target of `combo_id`. The
/// `combo_targets` join narrows the result to one combo's set of
/// targets, and the `datetime(cooldown_until) > datetime('now')`
/// filter is the same active-row rule [`is_in_cooldown`] uses.
///
/// Ordering: by `cooldown_until ASC` so the "expiring soonest" rows
/// come first — the dashboard renders them in the same order they
/// would naturally come back online.
pub fn list_for_combo(conn: &Connection, combo_id: ComboId) -> Result<Vec<Cooldown>> {
    let mut stmt = conn
        .prepare(
            "SELECT tc.combo_target_id, tc.cooldown_until, tc.reason, tc.failure_count
             FROM target_cooldowns tc
             JOIN combo_targets ct ON ct.id = tc.combo_target_id
             WHERE ct.combo_id = ?1
               AND datetime(tc.cooldown_until) > datetime('now')
             ORDER BY datetime(tc.cooldown_until) ASC",
        )
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map(params![combo_id.0], |row| {
            let until_str: String = row.get(1)?;
            let until = DateTime::parse_from_rfc3339(&until_str)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            Ok(Cooldown {
                combo_target_id: ComboTargetId(row.get::<_, i64>(0)?),
                cooldown_until: until,
                reason: row.get(2)?,
                failure_count: row.get::<_, i64>(3)? as u32,
            })
        })
        .map_err(crate::error::map_db_error)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(crate::error::map_db_error)?);
    }
    Ok(out)
}

/// Build a `combo_target_id -> Cooldown` index from a flat list.
/// Cheap; the dashboard's targets endpoint builds this once per
/// request and uses the map to enrich each row in O(1).
pub fn index_by_target(rows: Vec<Cooldown>) -> std::collections::HashMap<ComboTargetId, Cooldown> {
    rows.into_iter().map(|c| (c.combo_target_id, c)).collect()
}

/// Convenience wrapper for callers that have a single `combo_id` and
/// just want a map. The single-statement lookup avoids the round-
/// trip of building the Vec first when only the index is needed.
pub fn index_for_combo(
    conn: &Connection,
    combo_id: ComboId,
) -> Result<std::collections::HashMap<ComboTargetId, Cooldown>> {
    Ok(index_by_target(list_for_combo(conn, combo_id)?))
}

/// Read a single cooldown row by `combo_target_id` regardless of
/// whether it's currently active. Used by the admin endpoint that
/// powers the dashboard's "cooldown reason" tooltip so we can also
/// surface the row's `failure_count` even when the row is past its
/// `cooldown_until` (it'll be swept at the next prune tick).
pub fn get_for_target(conn: &Connection, target_id: ComboTargetId) -> Result<Option<Cooldown>> {
    let row = conn
        .query_row(
            "SELECT combo_target_id, cooldown_until, reason, failure_count
             FROM target_cooldowns
             WHERE combo_target_id = ?1",
            params![target_id.0],
            |row| {
                let until_str: String = row.get(1)?;
                let until = DateTime::parse_from_rfc3339(&until_str)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                Ok(Cooldown {
                    combo_target_id: ComboTargetId(row.get::<_, i64>(0)?),
                    cooldown_until: until,
                    reason: row.get(2)?,
                    failure_count: row.get::<_, i64>(3)? as u32,
                })
            },
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx(format!(
            "get_for_target({})",
            target_id.0
        )))?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::combos;
    use crate::db::conn::DbPool;
    use crate::db::migrations;
    use crate::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId};
    use crate::providers::{self, AuthType, ProviderFormat};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    fn fresh_pool() -> (DbPool, PathBuf) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("openproxy-cooldown-test-{}-{}-{}", pid, nanos, n));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("cooldown.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    /// Seed a provider, a model, a combo, and a target. Returns the
    /// target's id so individual tests can fire the cooldown
    /// functions on it without re-running the boilerplate.
    fn seed_target(conn: &Connection) -> (ComboId, ComboTargetId) {
        providers::create(
            conn,
            providers::NewProvider {
                id: &ProviderId::new("p"),
                name: "p",
                base_url: "https://example.com",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
            },
        )
        .expect("seed provider");
        conn.execute(
            "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm', 'openai')",
            [],
        )
        .expect("seed model");
        let model_rowid: i64 = conn
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("last_insert_rowid");
        let combo_id =
            combos::create_combo(conn, "c", combos::Strategy::Priority, 1).expect("combo");
        let target_id = combos::add_target(
            conn,
            combos::AddTargetInput {
                combo_id,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(ModelRowId(model_rowid)),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add target");
        (combo_id, target_id)
    }

    #[test]
    fn record_failure_creates_cooldown_row() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (_combo, target) = seed_target(&conn);

        record_failure(&conn, target, "boom", 60).expect("record");
        let row = get_for_target(&conn, target)
            .expect("get")
            .expect("present");
        assert_eq!(row.failure_count, 1);
        assert_eq!(row.reason.as_deref(), Some("boom"));
    }

    #[test]
    fn record_failure_increments_count_on_upsert() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (_combo, target) = seed_target(&conn);

        record_failure(&conn, target, "first", 60).expect("first");
        record_failure(&conn, target, "second", 60).expect("second");
        record_failure(&conn, target, "third", 60).expect("third");
        let row = get_for_target(&conn, target)
            .expect("get")
            .expect("present");
        assert_eq!(row.failure_count, 3);
        assert_eq!(row.reason.as_deref(), Some("third"));
    }

    #[test]
    fn clear_removes_cooldown() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (_combo, target) = seed_target(&conn);
        record_failure(&conn, target, "x", 60).expect("record");
        clear(&conn, target).expect("clear");
        assert!(get_for_target(&conn, target).expect("get").is_none());
    }

    #[test]
    fn is_in_cooldown_true_when_until_in_future() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (_combo, target) = seed_target(&conn);
        record_failure(&conn, target, "x", 60).expect("record");
        assert!(is_in_cooldown(&conn, target).expect("check"));
    }

    #[test]
    fn is_in_cooldown_false_when_until_in_past() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (_combo, target) = seed_target(&conn);
        // Manually insert a row whose cooldown_until is already in the past.
        let past = (Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
        conn.execute(
            "INSERT INTO target_cooldowns(combo_target_id, cooldown_until, reason, failure_count)
             VALUES (?1, ?2, 'past', 1)",
            params![target.0, past],
        )
        .expect("insert past");
        assert!(!is_in_cooldown(&conn, target).expect("check"));
    }

    #[test]
    fn prune_expired_removes_only_expired() {
        // Pool 1: one target, one active row, one expired row
        // (a second target would collide on the same provider
        // "p" because `seed_target` always creates it).
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (_combo, target_a) = seed_target(&conn);
        // Active row: cooldown_until in the future.
        record_failure(&conn, target_a, "future", 60).expect("record");
        // The unique-PK on `combo_target_id` would force a UPSERT
        // if we tried to insert a second row on `target_a` here.
        // The way to test the expired-vs-active mix on a single
        // target is to overwrite the existing row with a past
        // timestamp — we use raw SQL for that.
        let past = (Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
        conn.execute(
            "UPDATE target_cooldowns SET cooldown_until = ?1, reason = 'past' WHERE combo_target_id = ?2",
            params![past, target_a.0],
        )
        .expect("update past");
        // Prune pool1: should remove 1 expired row.
        let n1 = prune_expired(&conn).expect("prune 1");
        assert_eq!(n1, 1, "expired row in pool1 was swept");
        // The active row is gone too (we just overwrote it).
        assert!(!is_in_cooldown(&conn, target_a).expect("after sweep"));

        // Pool 2: verify the inverse case — a fresh active row
        // survives a sweep with zero changes.
        let (pool2, _path2) = fresh_pool();
        let conn2 = pool2.writer();
        let (_combo2, target_b) = seed_target(&conn2);
        record_failure(&conn2, target_b, "future2", 60).expect("record future2");
        let n2 = prune_expired(&conn2).expect("prune 2");
        assert_eq!(n2, 0, "no expired rows in pool2");
        assert!(is_in_cooldown(&conn2, target_b).expect("active check 2"));
    }

    #[test]
    fn list_for_combo_returns_active_cooldowns() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let (combo_id, target) = seed_target(&conn);
        record_failure(&conn, target, "x", 60).expect("record");
        let list = list_for_combo(&conn, combo_id).expect("list");
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].combo_target_id, target);

        // Expired row on the same target is filtered out (UPSERT
        // semantics: the new record_failure replaced the old row, so
        // we re-record with a -1s cooldown to simulate expiry).
        clear(&conn, target).expect("clear");
        // Insert an expired row directly so the test doesn't depend
        // on the wall clock.
        let past = (Utc::now() - chrono::Duration::seconds(5)).to_rfc3339();
        conn.execute(
            "INSERT INTO target_cooldowns(combo_target_id, cooldown_until, reason, failure_count)
             VALUES (?1, ?2, 'past', 1)",
            params![target.0, past],
        )
        .expect("insert past");
        let list = list_for_combo(&conn, combo_id).expect("list");
        assert!(list.is_empty(), "expired row is not in the active list");
    }

    #[test]
    fn index_by_target_groups_one_entry_per_target() {
        let rows = vec![
            Cooldown {
                combo_target_id: ComboTargetId(1),
                cooldown_until: Utc::now(),
                reason: None,
                failure_count: 1,
            },
            Cooldown {
                combo_target_id: ComboTargetId(2),
                cooldown_until: Utc::now(),
                reason: None,
                failure_count: 1,
            },
        ];
        let idx = index_by_target(rows);
        assert_eq!(idx.len(), 2);
        assert!(idx.contains_key(&ComboTargetId(1)));
        assert!(idx.contains_key(&ComboTargetId(2)));
    }

    #[test]
    fn test_index_for_combo() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute(
            "CREATE TABLE target_cooldowns (
                combo_target_id INTEGER PRIMARY KEY,
                cooldown_until TEXT NOT NULL,
                reason TEXT,
                failure_count INTEGER NOT NULL
            )",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TABLE combo_targets (
                id INTEGER PRIMARY KEY,
                combo_id INTEGER NOT NULL
            )",
            [],
        )
        .unwrap();

        let combo_id = ComboId(1);
        let target_id = ComboTargetId(10);
        conn.execute(
            "INSERT INTO combo_targets (id, combo_id) VALUES (?1, ?2)",
            params![target_id.0, combo_id.0],
        )
        .unwrap();

        let until = Utc::now() + chrono::Duration::try_minutes(5).unwrap();
        conn.execute(
            "INSERT INTO target_cooldowns (combo_target_id, cooldown_until, reason, failure_count)
             VALUES (?1, ?2, ?3, ?4)",
            params![target_id.0, until.to_rfc3339(), "test error", 1],
        )
        .unwrap();

        let idx = index_for_combo(&conn, combo_id).unwrap();
        assert_eq!(idx.len(), 1);
        let cd = idx.get(&target_id).unwrap();
        assert_eq!(cd.combo_target_id, target_id);
        assert_eq!(cd.reason, Some("test error".to_string()));
        assert_eq!(cd.failure_count, 1);
    }

    #[test]
    fn account_id_imported_path() {
        // Smoke: ensure the AccountId re-export is reachable (it is used
        // by callers that mix target and account scoping). This keeps
        // the import chain honest.
        let _ = AccountId(1);
    }
}
