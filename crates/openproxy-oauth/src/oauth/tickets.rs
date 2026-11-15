//! Persistent storage for OAuth Device Code flow tickets.
//!
//! The flow is two-phase (POST `/device/code`, then poll `/token` until
//! authorized). The `device_code` lives in `oauth_device_tickets` (migration
//! 000134) so a page refresh, server restart, or cache eviction between
//! phases does not abort it.
//!
//! Security (audit H-4): the `device_code` is a bearer secret — whoever
//! holds it can redeem the authorization — so it is never stored in the
//! clear anymore. The row keeps a SHA-256 hash (the lookup key) plus a
//! MasterKey-sealed blob (AAD-bound to this column), so the value is both
//! unsearchable at rest and recoverable by the operator.
//!
//! `oauth_device_code` creates the ticket, `oauth_device_poll` looks it up
//! and marks it consumed, and a periodic sweep in `start_refresh_scheduler`
//! cleans up expired rows.

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use base64::Engine as _;
use openproxy_db::secrets::MasterKey;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// AAD context binding every sealed `device_code` blob to this column
/// (mirrors the `oauth_provider_specific` scheme).
pub const DEVICE_CODE_CTX: &str = "oauth_device_tickets.device_code";

/// SHA-256 of the device code, hex-encoded. This is the only value SQL ever
/// compares; the plaintext is never a query operand.
pub fn device_code_hash(device_code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(device_code.as_bytes());
    let digest = hasher.finalize();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Seal a device code with the MasterKey (recoverable counterpart of
/// [`device_code_hash`]).
pub fn seal_device_code(device_code: &str, master_key: &MasterKey) -> Result<String> {
    let blob = master_key.encrypt_ctx(device_code, DEVICE_CODE_CTX)?;
    Ok(base64::engine::general_purpose::STANDARD.encode(blob))
}

/// Reveal a sealed device code. `None` when the blob does not authenticate
/// for this column (tampered, wrong key) — fail-closed.
pub fn reveal_device_code(sealed_b64: &str, master_key: &MasterKey) -> Option<String> {
    let blob = base64::engine::general_purpose::STANDARD
        .decode(sealed_b64)
        .ok()?;
    master_key.decrypt_ctx(&blob, DEVICE_CODE_CTX).ok()
}

/// On-disk shape of a row in `oauth_device_tickets`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceTicket {
    pub id: i64,
    pub provider: String,
    /// Recovered from the sealed blob (empty when the blob does not open).
    /// The SQL lookup key is the hash — this field exists so operators and
    /// tooling can still see WHICH code a pending ticket holds.
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
/// Idempotent on the device code: an existing row returns its id unchanged,
/// which makes the call safe under retries. The row stores the code's hash
/// (lookup) and a MasterKey-sealed blob (recovery) — never the plaintext.
pub fn create_ticket(
    conn: &Connection,
    master_key: &MasterKey,
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

    let dc_hash = device_code_hash(&dar.device_code);
    let dc_sealed = seal_device_code(&dar.device_code, master_key)?;

    // upsert + RETURNING id in one statement so a retry keeps the same row
    let new_id: i64 = conn
        .query_row(
            "INSERT INTO oauth_device_tickets
                 (provider, device_code_hash, device_code_encrypted, user_code, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(device_code_hash) DO UPDATE
                 SET provider = excluded.provider
             RETURNING id",
            params![provider, dc_hash, dc_sealed, dar.user_code, expires_at],
            |r| r.get::<_, i64>(0),
        )
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(new_id)
}

fn map_ticket_row(
    master_key: &MasterKey,
) -> impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<DeviceTicket> + '_ {
    move |r| {
        let account_id: Option<i64> = r.get(4)?;
        let sealed: String = r.get(2)?;
        Ok(DeviceTicket {
            id: r.get(0)?,
            provider: r.get(1)?,
            // Recoverable, fail-closed: a blob that does not open leaves the
            // field empty without affecting the ticket's state machine.
            device_code: reveal_device_code(&sealed, master_key).unwrap_or_default(),
            user_code: r.get(3)?,
            account_id: account_id.map(AccountId),
            expires_at: r.get(5)?,
            consumed_at: r.get(6)?,
        })
    }
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

/// Look up a ticket by `device_code` and classify its status. The lookup
/// hashes the supplied code; the plaintext never reaches SQL.
///
/// Single-use invariant: `consumed_at IS NOT NULL` short-circuits to `Consumed`
/// before the `expires_at` check, so a redeem-then-replay reads as `Consumed`
/// regardless of whether the upstream TTL has passed.
pub fn lookup_active(
    conn: &Connection,
    master_key: &MasterKey,
    device_code: &str,
) -> Result<TicketStatus> {
    let dc_hash = device_code_hash(device_code);
    let row = conn
        .query_row(
            "SELECT id, provider, device_code_encrypted, user_code, account_id,
                    expires_at, consumed_at
               FROM oauth_device_tickets
              WHERE device_code_hash = ?1",
            params![dc_hash],
            map_ticket_row(master_key),
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
    let dc_hash = device_code_hash(device_code);
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let rows = conn
        .execute(
            "UPDATE oauth_device_tickets
                SET consumed_at = ?1
              WHERE device_code_hash = ?2
                AND consumed_at IS NULL",
            params![now, dc_hash],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    if rows == 0 {
        return Err(CoreError::not_found("oauth_device_ticket", device_code));
    }
    let id: i64 = conn
        .query_row(
            "SELECT id FROM oauth_device_tickets WHERE device_code_hash = ?1",
            params![dc_hash],
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

    use openproxy_db::MasterKey;
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
    fn hash_is_deterministic_and_opaque() {
        let a = device_code_hash("DEV-1");
        let b = device_code_hash("DEV-1");
        assert_eq!(a, b);
        assert_ne!(a, "DEV-1", "the hash must not contain the code");
        assert_eq!(a.len(), 64, "sha256 hex");
    }

    #[test]
    fn seal_round_trips_and_binds_to_the_column() {
        let mk = MasterKey::generate().unwrap();
        let sealed = seal_device_code("DEV-SECRET", &mk).unwrap();
        assert_eq!(
            reveal_device_code(&sealed, &mk).as_deref(),
            Some("DEV-SECRET")
        );
        // A blob replayed into another column must not open.
        let other = mk.encrypt_ctx("DEV-SECRET", "some.other.column").unwrap();
        let other_b64 = base64::engine::general_purpose::STANDARD.encode(other);
        assert_eq!(reveal_device_code(&other_b64, &mk), None);
    }

    #[test]
    fn stored_row_never_contains_the_plaintext() {
        let conn = fresh_conn();
        let mk = MasterKey::generate().unwrap();
        create_ticket(
            &conn,
            &mk,
            "kiro",
            &sample_dar("DEV-PLAINTEXT-CHECK", "U-1"),
        )
        .expect("create");
        let raw: String = conn
            .query_row(
                "SELECT provider || '|' || device_code_hash || '|' || device_code_encrypted
                       || '|' || user_code
                    FROM oauth_device_tickets LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !raw.contains("DEV-PLAINTEXT-CHECK"),
            "no column may hold the plaintext device code: {raw}"
        );
    }

    #[test]
    fn create_then_lookup_is_active_and_recovers_the_code() {
        let conn = fresh_conn();
        let mk = MasterKey::generate().unwrap();
        let id = create_ticket(&conn, &mk, "kiro", &sample_dar("DEV-1", "USER-1")).expect("create");
        assert!(id > 0);
        let status = lookup_active(&conn, &mk, "DEV-1").expect("lookup");
        let TicketStatus::Active(t) = status else {
            panic!("expected Active, got {status:?}");
        };
        assert_eq!(t.provider, "kiro");
        assert_eq!(t.user_code, "USER-1");
        assert_eq!(t.device_code, "DEV-1", "sealed blob must be recoverable");
        assert!(t.account_id.is_none());
        assert!(t.consumed_at.is_none());
    }

    #[test]
    fn create_is_idempotent_on_device_code() {
        let conn = fresh_conn();
        let mk = MasterKey::generate().unwrap();
        let id1 =
            create_ticket(&conn, &mk, "kiro", &sample_dar("DEV-2", "USER-A")).expect("create");
        let id2 =
            create_ticket(&conn, &mk, "kiro", &sample_dar("DEV-2", "USER-B")).expect("create");
        assert_eq!(id1, id2, "same device_code must yield same row id");
    }

    #[test]
    fn lookup_unknown_returns_unknown() {
        let conn = fresh_conn();
        let mk = MasterKey::generate().unwrap();
        let status = lookup_active(&conn, &mk, "NEVER-EXISTED").expect("lookup");
        let TicketStatus::Unknown = status else {
            panic!("expected Unknown, got {status:?}");
        };
    }

    #[test]
    fn mark_consumed_blocks_subsequent_lookup() {
        let conn = fresh_conn();
        let mk = MasterKey::generate().unwrap();
        create_ticket(&conn, &mk, "kiro", &sample_dar("DEV-3", "USER-3")).expect("create");
        mark_consumed(&conn, "DEV-3").expect("consume");
        let status = lookup_active(&conn, &mk, "DEV-3").expect("lookup");
        let TicketStatus::Consumed = status else {
            panic!("expected Consumed, got {status:?}");
        };
    }

    #[test]
    fn mark_consumed_twice_errors() {
        let conn = fresh_conn();
        let mk = MasterKey::generate().unwrap();
        create_ticket(&conn, &mk, "kiro", &sample_dar("DEV-4", "USER-4")).expect("create");
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
        let mk = MasterKey::generate().unwrap();
        // bypass the create_ticket clamp by inserting directly with an
        // expires_at in the past (hash + sealed blob, like the real writer)
        let sealed = seal_device_code("DEV-EXPIRED", &mk).unwrap();
        conn.execute(
            "INSERT INTO oauth_device_tickets
                 (provider, device_code_hash, device_code_encrypted, user_code, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                "kiro",
                device_code_hash("DEV-EXPIRED"),
                sealed,
                "USER-X",
                "2000-01-01T00:00:00Z"
            ],
        )
        .expect("insert expired");
        let status = lookup_active(&conn, &mk, "DEV-EXPIRED").expect("lookup");
        let TicketStatus::Expired = status else {
            panic!("expected Expired, got {status:?}");
        };
    }

    #[test]
    fn cleanup_expired_deletes_old_rows() {
        let conn = fresh_conn();
        let mk = MasterKey::generate().unwrap();
        let seal = |dc: &str| seal_device_code(dc, &mk).unwrap();
        conn.execute(
            "INSERT INTO oauth_device_tickets
                 (provider, device_code_hash, device_code_encrypted, user_code, expires_at, created_at)
             VALUES ('kiro', ?1, ?2, 'U-1', '2000-01-01T00:00:00Z', '2000-01-01T00:00:00Z'),
                    ('kiro', ?3, ?4, 'U-2', '2000-01-01T00:00:00Z', '2000-01-01T00:00:00Z'),
                    ('kiro', ?5, ?6, 'U-3', '2099-01-01T00:00:00Z',
                     strftime('%Y-%m-%dT%H:%M:%SZ','now'))",
            params![
                device_code_hash("OLD-1"),
                seal("OLD-1"),
                device_code_hash("OLD-2"),
                seal("OLD-2"),
                device_code_hash("FUT-1"),
                seal("FUT-1"),
            ],
        )
        .expect("insert");
        let n = cleanup_expired(&conn).expect("cleanup");
        assert_eq!(n, 2, "expected exactly the 2 expired rows deleted");
        assert!(matches!(
            lookup_active(&conn, &mk, "FUT-1").expect("lookup"),
            TicketStatus::Active(_)
        ));
    }

    #[test]
    fn expires_in_above_hard_ttl_is_clamped() {
        let conn = fresh_conn();
        let mk = MasterKey::generate().unwrap();
        // expires_in = 24h must clamp to HARD_TTL_SECS (10 min).
        let dar = crate::oauth::DeviceAuthorizationResponse {
            device_code: "DEV-CLAMP".into(),
            user_code: "USER-CLAMP".into(),
            verification_uri: "https://example.com".into(),
            verification_uri_complete: None,
            expires_in: Some(86_400),
            interval: Some(5),
        };
        create_ticket(&conn, &mk, "kiro", &dar).expect("create");
        let status = lookup_active(&conn, &mk, "DEV-CLAMP").expect("lookup");
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

    #[test]
    fn tampered_blob_fails_closed_but_state_survives() {
        // A row whose sealed blob was corrupted must not break the poll
        // state machine: the ticket still classifies, the code field is
        // simply not recoverable.
        let conn = fresh_conn();
        let mk = MasterKey::generate().unwrap();
        let other_key = MasterKey::generate().unwrap();
        let sealed_with_other_key = seal_device_code("DEV-5", &other_key).unwrap();
        conn.execute(
            "INSERT INTO oauth_device_tickets
                 (provider, device_code_hash, device_code_encrypted, user_code, expires_at)
             VALUES ('kiro', ?1, ?2, 'U-5', '2099-01-01T00:00:00Z')",
            params![device_code_hash("DEV-5"), sealed_with_other_key],
        )
        .unwrap();
        let status = lookup_active(&conn, &mk, "DEV-5").expect("lookup");
        let TicketStatus::Active(t) = status else {
            panic!("expected Active, got {status:?}");
        };
        assert_eq!(
            t.device_code, "",
            "an unopenable blob must not surface the plaintext"
        );
    }
}
