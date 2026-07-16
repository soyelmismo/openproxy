//! Persistent storage for OAuth Device Code flow tickets.
//!
//! LOW fix (#12): the Device Code flow is two-phase (POST
//! `/device/code` then poll `/token` until authorized). Before this
//! module, the `device_code` only lived in the HTTP response to the
//! dashboard; a page refresh, server restart, or cache eviction
//! between the two phases silently aborted the flow. We persist the
//! ticket in the `oauth_device_tickets` table (see migration 000027)
//! keyed by `device_code`, so the dashboard can look it up by either
//! `device_code` or `user_code` if it loses one.
//!
//! Lifecycle:
//!   1. `oauth_device_code` HTTP handler calls
//!      [`create_ticket`] with the upstream's `DeviceAuthorizationResponse`.
//!   2. `oauth_device_poll` HTTP handler calls
//!      [`lookup_active`] which returns [`TicketStatus::Active`] for
//!      pending tickets, [`TicketStatus::Expired`] for past
//!      `expires_at`, [`TicketStatus::Consumed`] for already-redeemed
//!      ones, and [`TicketStatus::Unknown`] if no row exists.
//!   3. On successful poll, the handler calls [`mark_consumed`] so
//!      the same device_code can't be redeemed twice.
//!   4. A periodic sweep (added to `start_refresh_scheduler`) calls
//!      [`cleanup_expired`] to keep the table small.

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

/// On-disk shape of a row in `oauth_device_tickets`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceTicket {
    pub id: i64,
    pub provider: String,
    pub device_code: String,
    pub user_code: String,
    pub account_id: Option<AccountId>,
    /// RFC3339 UTC wall-clock. Past `now()` ⇒ expired.
    pub expires_at: String,
    /// RFC3339 UTC; set when [`mark_consumed`] runs.
    pub consumed_at: Option<String>,
}

/// Return shape of [`lookup_active`]. Encodes the four possible states
/// the dashboard can observe on the poll path.
#[derive(Debug)]
pub enum TicketStatus {
    /// Pending; the user has not yet authorized (or denied).
    Active(DeviceTicket),
    /// `device_code` was never persisted, or has been swept.
    Unknown,
    /// `expires_at < now()`.
    Expired,
    /// Already redeemed (`consumed_at IS NOT NULL`). Single-use
    /// enforcement: a second poll with the same `device_code`
    /// returns this so the dashboard can show "already used"
    /// instead of silently returning `ok` again.
    Consumed,
}

/// HARD TTL cap on ticket age. The upstream's `expires_in` is
/// authoritative for the *user-visible* countdown, but we also refuse
/// any ticket older than this on the server side — a leaked or
/// replayed ticket cannot outlive the upstream's TTL by much even if
/// the upstream forgot to enforce it.
const HARD_TTL_SECS: i64 = 600; // 10 minutes

/// Insert a new ticket. `expires_at` is computed from the upstream's
/// `expires_in` clamped to [`HARD_TTL_SECS`] so a malicious upstream
/// can't request a 30-day TTL. Returns the persisted row's `id`.
///
/// Idempotency: if the same `(provider, device_code)` already exists,
/// the existing row's id is returned unchanged. The upstream
/// generates `device_code` with high entropy so a collision is
/// effectively impossible in practice, but the uniqueness constraint
/// + this branch make the call safe under retries.
pub fn create_ticket(
    conn: &Connection,
    provider: &str,
    dar: &crate::oauth::DeviceAuthorizationResponse,
) -> Result<i64> {
    let upstream_secs = match dar.expires_in {
        Some(s) => s as i64,
        None => HARD_TTL_SECS,
    }
    .min(HARD_TTL_SECS);
    let expires_at = (chrono::Utc::now() + chrono::Duration::seconds(upstream_secs))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();

    // INSERT OR IGNORE + RETURNING id is the cleanest single-statement
    // shape that survives a retry. If the row already exists we fall
    // back to a SELECT to recover the id.
    let new_id: i64 = conn
        .query_row(
            "INSERT INTO oauth_device_tickets
                 (provider, device_code, user_code, expires_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(device_code) DO UPDATE
                 SET provider = excluded.provider
             RETURNING id",
            params![provider, dar.device_code, dar.user_code, expires_at],
            |r| r.get::<_, i64>(0),
        )
        .map_err(crate::error::map_db_error)?;
    Ok(new_id)
}

/// Look up a ticket by `device_code` and classify its status. The
/// single-use invariant lives here: `consumed_at IS NOT NULL`
/// short-circuits to `Consumed` before the `expires_at` check so a
/// redeem-then-replay shows the same `Consumed` state regardless of
/// whether the upstream's TTL has passed.
pub fn lookup_active(conn: &Connection, device_code: &str) -> Result<TicketStatus> {
    let row = conn
        .query_row(
            "SELECT id, provider, device_code, user_code, account_id,
                    expires_at, consumed_at
               FROM oauth_device_tickets
              WHERE device_code = ?1",
            params![device_code],
            |r| {
                let account_id: Option<i64> = r.get(4)?;
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    account_id.map(AccountId),
                    r.get::<_, String>(5)?,
                    r.get::<_, Option<String>>(6)?,
                ))
            },
        )
        .optional()
        .map_err(crate::error::map_db_error)?;
    let Some((id, provider, device_code, user_code, account_id, expires_at, consumed_at)) = row
    else {
        return Ok(TicketStatus::Unknown);
    };
    if consumed_at.is_some() {
        return Ok(TicketStatus::Consumed);
    }
    // Parse `expires_at` with chrono (LOW fix #15 generalized) so we
    // compare wall-clocks, not lex order — a stale row with a
    // zero-padded but otherwise broken format would otherwise be
    // ambiguous.
    match chrono::DateTime::parse_from_rfc3339(&expires_at) {
        Ok(dt) if dt <= chrono::Utc::now() => Ok(TicketStatus::Expired),
        Ok(_) => Ok(TicketStatus::Active(DeviceTicket {
            id,
            provider,
            device_code,
            user_code,
            account_id,
            expires_at,
            consumed_at,
        })),
        Err(_) => Ok(TicketStatus::Expired),
    }
}

/// Mark a ticket as consumed. Returns the row id of the updated row
/// (i.e. the same id `create_ticket` returned) so the caller can log
/// it. Returns `Err(CoreError::NotFound)` if the `device_code` does
/// not exist — the caller should treat that as a 404, not silently
/// succeed.
///
/// The WHERE clause asserts `consumed_at IS NULL` so a racing second
/// poll cannot both observe the ticket as Active and then both
/// succeed at marking it. The losing caller gets 0 rows updated and
/// returns `Err(NotFound)`, which the handler surfaces as a 409.
pub fn mark_consumed(conn: &Connection, device_code: &str) -> Result<i64> {
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let rows = conn
        .execute(
            "UPDATE oauth_device_tickets
                SET consumed_at = ?1
              WHERE device_code = ?2
                AND consumed_at IS NULL",
            params![now, device_code],
        )
        .map_err(crate::error::map_db_error)?;
    if rows == 0 {
        return Err(CoreError::NotFound {
            what: "oauth_device_ticket".into(),
            id: device_code.into(),
        });
    }
    let id: i64 = conn
        .query_row(
            "SELECT id FROM oauth_device_tickets WHERE device_code = ?1",
            params![device_code],
            |r| r.get(0),
        )
        .map_err(crate::error::map_db_error)?;
    Ok(id)
}

/// Delete tickets whose `expires_at` is older than `now()` OR whose
/// `created_at` is older than the hard TTL (defense in depth — even
/// if `expires_at` is malformed, the created_at cap will reclaim
/// the row). Returns the number of deleted rows.
pub fn cleanup_expired(conn: &Connection) -> Result<usize> {
    let now = chrono::Utc::now();
    let now_str = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let hard_cutoff_str = (now - chrono::Duration::seconds(HARD_TTL_SECS))
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let rows = conn
        .execute(
            "DELETE FROM oauth_device_tickets
              WHERE expires_at < ?1
                 OR created_at < ?2",
            params![now_str, hard_cutoff_str],
        )
        .map_err(crate::error::map_db_error)?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_db::migrations;
    use rusqlite::Connection;

    fn fresh_conn() -> Connection {
        // Open a private :memory: DB so each test sees a clean schema
        // without going through DbPool's writer guard (which holds
        // the connection). Migrations run directly on this handle.
        let mut conn = Connection::open_in_memory().expect("in-memory rusqlite conn");
        openproxy_db::migrations::run(&mut conn).expect("migrations");
        conn
    }

    fn sample_dar(device_code: &str, user_code: &str) -> crate::oauth::DeviceAuthorizationResponse {
        crate::oauth::DeviceAuthorizationResponse {
            device_code: device_code.into(),
            user_code: user_code.into(),
            verification_uri: "https://example.com/activate".into(),
            verification_uri_complete: None,
            expires_in: Some(60),
            interval: Some(5),
        }
    }

    #[test]
    fn create_then_lookup_is_active() {
        let conn = fresh_conn();
        let id = create_ticket(&conn, "kiro", &sample_dar("DEV-1", "USER-1")).expect("create");
        assert!(id > 0);
        match lookup_active(&conn, "DEV-1").expect("lookup") {
            TicketStatus::Active(t) => {
                assert_eq!(t.provider, "kiro");
                assert_eq!(t.user_code, "USER-1");
                assert!(t.account_id.is_none());
                assert!(t.consumed_at.is_none());
            }
            other => panic!("expected Active, got {:?}", other),
        }
    }

    #[test]
    fn create_is_idempotent_on_device_code() {
        let conn = fresh_conn();
        let id1 = create_ticket(&conn, "kiro", &sample_dar("DEV-2", "USER-A")).expect("create");
        let id2 = create_ticket(&conn, "kiro", &sample_dar("DEV-2", "USER-B")).expect("create");
        assert_eq!(id1, id2, "same device_code must yield same row id");
    }

    #[test]
    fn lookup_unknown_returns_unknown() {
        let conn = fresh_conn();
        match lookup_active(&conn, "NEVER-EXISTED").expect("lookup") {
            TicketStatus::Unknown => {}
            other => panic!("expected Unknown, got {:?}", other),
        }
    }

    #[test]
    fn mark_consumed_blocks_subsequent_lookup() {
        let conn = fresh_conn();
        create_ticket(&conn, "kiro", &sample_dar("DEV-3", "USER-3")).expect("create");
        mark_consumed(&conn, "DEV-3").expect("consume");
        // A second poll must NOT see Active.
        match lookup_active(&conn, "DEV-3").expect("lookup") {
            TicketStatus::Consumed => {}
            other => panic!("expected Consumed, got {:?}", other),
        }
    }

    #[test]
    fn mark_consumed_twice_errors() {
        let conn = fresh_conn();
        create_ticket(&conn, "kiro", &sample_dar("DEV-4", "USER-4")).expect("create");
        mark_consumed(&conn, "DEV-4").expect("first consume");
        // Second call must return NotFound because the WHERE clause
        // asserts consumed_at IS NULL.
        match mark_consumed(&conn, "DEV-4") {
            Err(CoreError::NotFound { .. }) => {}
            other => panic!("expected NotFound on double consume, got {:?}", other),
        }
    }

    #[test]
    fn expired_ticket_returns_expired_status() {
        let conn = fresh_conn();
        // Bypass the create_ticket clamp by inserting directly with
        // an expires_at in the past.
        conn.execute(
            "INSERT INTO oauth_device_tickets
                 (provider, device_code, user_code, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params!["kiro", "DEV-EXPIRED", "USER-X", "2000-01-01T00:00:00Z"],
        )
        .expect("insert expired");
        match lookup_active(&conn, "DEV-EXPIRED").expect("lookup") {
            TicketStatus::Expired => {}
            other => panic!("expected Expired, got {:?}", other),
        }
    }

    #[test]
    fn cleanup_expired_deletes_old_rows() {
        let conn = fresh_conn();
        conn.execute(
            "INSERT INTO oauth_device_tickets
                 (provider, device_code, user_code, expires_at, created_at)
             VALUES ('kiro', 'OLD-1', 'U-1', '2000-01-01T00:00:00Z',
                     '2000-01-01T00:00:00Z'),
                    ('kiro', 'OLD-2', 'U-2', '2000-01-01T00:00:00Z',
                     '2000-01-01T00:00:00Z'),
                    ('kiro', 'FUT-1', 'U-3',
                     '2099-01-01T00:00:00Z',
                     strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
            [],
        )
        .expect("insert");
        let n = cleanup_expired(&conn).expect("cleanup");
        assert_eq!(n, 2, "expected exactly the 2 expired rows deleted");
        // The future row must still be present.
        assert!(matches!(
            lookup_active(&conn, "FUT-1").expect("lookup"),
            TicketStatus::Active(_)
        ));
    }

    #[test]
    fn expires_in_above_hard_ttl_is_clamped() {
        let conn = fresh_conn();
        // expires_in = 24h must clamp to HARD_TTL_SECS (10 min).
        let dar = crate::oauth::DeviceAuthorizationResponse {
            device_code: "DEV-CLAMP".into(),
            user_code: "USER-CLAMP".into(),
            verification_uri: "https://example.com".into(),
            verification_uri_complete: None,
            expires_in: Some(86_400),
            interval: Some(5),
        };
        create_ticket(&conn, "kiro", &dar).expect("create");
        let t = match lookup_active(&conn, "DEV-CLAMP").expect("lookup") {
            TicketStatus::Active(t) => t,
            other => panic!("expected Active, got {:?}", other),
        };
        let dt = chrono::DateTime::parse_from_rfc3339(&t.expires_at)
            .expect("parse expires_at")
            .with_timezone(&chrono::Utc);
        let lifetime_secs = (dt - chrono::Utc::now()).num_seconds();
        assert!(
            lifetime_secs <= HARD_TTL_SECS + 2, // +2 for clock skew
            "expires_in=86400 must clamp to <= {} sec; got {}",
            HARD_TTL_SECS,
            lifetime_secs
        );
    }
}
