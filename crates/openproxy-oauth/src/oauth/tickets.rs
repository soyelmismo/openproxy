//! Persistent storage for OAuth Device Code flow tickets.
//!
//! The flow is two-phase (POST `/device/code`, then poll `/token` until
//! authorized). The `device_code` lives in `oauth_device_tickets` (migration
//! 000027) keyed by `device_code`, so a page refresh, server restart, or cache
//! eviction between phases does not abort it, and the dashboard can look the
//! ticket up by `user_code` if it loses the `device_code`.
//!
//! `oauth_device_code` creates the ticket, `oauth_device_poll` looks it up and
//! marks it consumed, and a periodic sweep in `start_refresh_scheduler` cleans
//! up expired rows.

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

/// Insert a ticket, clamping the upstream's `expires_in` to [`HARD_TTL_SECS`] so
/// a malicious upstream cannot request a 30-day TTL. Returns the persisted
/// row's `id`.
///
/// Idempotent on `device_code`: an existing row returns its id unchanged, which
/// makes the call safe under retries.
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

    // upsert + RETURNING id in one statement so a retry keeps the same row
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
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(new_id)
}

fn map_ticket_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<DeviceTicket> {
    let account_id: Option<i64> = r.get(4)?;
    Ok(DeviceTicket {
        id: r.get(0)?,
        provider: r.get(1)?,
        device_code: r.get(2)?,
        user_code: r.get(3)?,
        account_id: account_id.map(AccountId),
        expires_at: r.get(5)?,
        consumed_at: r.get(6)?,
    })
}

fn classify_ticket_status(ticket: DeviceTicket) -> TicketStatus {
    if ticket.consumed_at.is_some() {
        return TicketStatus::Consumed;
    }
    match openproxy_types::timestamp::parse_timestamp(&ticket.expires_at) {
        Ok(dt) if dt > chrono::Utc::now() => TicketStatus::Active(ticket),
        _ => TicketStatus::Expired,
    }
}

/// Look up a ticket by `device_code` and classify its status.
///
/// Single-use invariant: `consumed_at IS NOT NULL` short-circuits to `Consumed`
/// before the `expires_at` check, so a redeem-then-replay reads as `Consumed`
/// regardless of whether the upstream TTL has passed.
pub fn lookup_active(conn: &Connection, device_code: &str) -> Result<TicketStatus> {
    let row = conn
        .query_row(
            "SELECT id, provider, device_code, user_code, account_id,
                    expires_at, consumed_at
               FROM oauth_device_tickets
              WHERE device_code = ?1",
            params![device_code],
            map_ticket_row,
        )
        .optional()
        .map_err(openproxy_db::error::map_db_error)?;

    Ok(row.map_or(TicketStatus::Unknown, classify_ticket_status))
}

/// Mark a ticket consumed, returning its row id (the one [`create_ticket`]
/// returned). A missing `device_code` is `Err(NotFound)`, which the handler
/// surfaces as a 404.
///
/// The WHERE clause asserts `consumed_at IS NULL` so two racing polls cannot
/// both mark the ticket. The loser updates 0 rows and gets `Err(NotFound)`,
/// which the handler surfaces as a 409.
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
        .map_err(openproxy_db::error::map_db_error)?;
    if rows == 0 {
        return Err(CoreError::not_found("oauth_device_ticket", device_code));
    }
    let id: i64 = conn
        .query_row(
            "SELECT id FROM oauth_device_tickets WHERE device_code = ?1",
            params![device_code],
            |r| r.get(0),
        )
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(id)
}

/// Delete tickets past `expires_at` or older than the hard TTL. The
/// `created_at` cap reclaims rows even when `expires_at` is malformed. Returns
/// the deleted row count.
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
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    use rusqlite::Connection;

    fn fresh_conn() -> Connection {
        // private in-memory DB: bypasses DbPool's long-held writer guard
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
        let status = lookup_active(&conn, "DEV-1").expect("lookup");
        let TicketStatus::Active(t) = status else {
            panic!("expected Active, got {status:?}");
        };
        assert_eq!(t.provider, "kiro");
        assert_eq!(t.user_code, "USER-1");
        assert!(t.account_id.is_none());
        assert!(t.consumed_at.is_none());
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
        let status = lookup_active(&conn, "NEVER-EXISTED").expect("lookup");
        let TicketStatus::Unknown = status else {
            panic!("expected Unknown, got {status:?}");
        };
    }

    #[test]
    fn mark_consumed_blocks_subsequent_lookup() {
        let conn = fresh_conn();
        create_ticket(&conn, "kiro", &sample_dar("DEV-3", "USER-3")).expect("create");
        mark_consumed(&conn, "DEV-3").expect("consume");
        let status = lookup_active(&conn, "DEV-3").expect("lookup");
        let TicketStatus::Consumed = status else {
            panic!("expected Consumed, got {status:?}");
        };
    }

    #[test]
    fn mark_consumed_twice_errors() {
        let conn = fresh_conn();
        create_ticket(&conn, "kiro", &sample_dar("DEV-4", "USER-4")).expect("create");
        mark_consumed(&conn, "DEV-4").expect("first consume");
        let res = mark_consumed(&conn, "DEV-4");
        // The raw OAuth body reaches these helpers, so a missing parameter must
        // reach the caller as a typed error rather than a panic.
        let Err(CoreError::NotFound { .. }) = res else {
            panic!("expected NotFound on double consume, got {res:?}");
        };
    }

    #[test]
    fn expired_ticket_returns_expired_status() {
        let conn = fresh_conn();
        // bypass the create_ticket clamp by inserting directly with an
        // expires_at in the past
        conn.execute(
            "INSERT INTO oauth_device_tickets
                 (provider, device_code, user_code, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
            params!["kiro", "DEV-EXPIRED", "USER-X", "2000-01-01T00:00:00Z"],
        )
        .expect("insert expired");
        let status = lookup_active(&conn, "DEV-EXPIRED").expect("lookup");
        let TicketStatus::Expired = status else {
            panic!("expected Expired, got {status:?}");
        };
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
        let status = lookup_active(&conn, "DEV-CLAMP").expect("lookup");
        let TicketStatus::Active(t) = status else {
            panic!("expected Active, got {status:?}");
        };
        let dt = openproxy_types::timestamp::parse_timestamp(&t.expires_at)
            .expect("parse expires_at")
            .with_timezone(&chrono::Utc);
        let lifetime_secs = (dt - chrono::Utc::now()).num_seconds();
        assert!(
            lifetime_secs <= HARD_TTL_SECS + 2, // +2 for clock skew
            "expires_in=86400 must clamp to <= {HARD_TTL_SECS} sec; got {lifetime_secs}"
        );
    }
}
