//! Single-use, short-lived tickets for the `/admin/ws` handshake.
//!
//! Browsers cannot set an `Authorization` header on `new WebSocket()`,
//! so the dashboard used to put the manage-scope API key in the query
//! string (`?token=`). Every reverse proxy in front of the server logs
//! the request line, so the long-lived secret ended up in plaintext in
//! world-readable access logs. Instead, the SPA now:
//!
//! 1. `POST /admin/api/ws-ticket` with the usual Bearer header → gets an
//!    opaque ticket bound to the authenticated key.
//! 2. Opens `/admin/ws?ticket=<ticket>`. The handler consumes the ticket
//!    (one use), checks it has not expired, and re-validates the
//!    underlying API key (still active, still `manage`).
//!
//! A leaked ticket is worthless after [`WsTicketStore::TTL`] or after its
//! single use, whichever comes first.

use openproxy_types::ids::ApiKeyId;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Length of the random alphanumeric ticket (62-symbol alphabet → ≈190 bits).
const TICKET_LEN: usize = 32;

/// Minimum seconds between periodic expired ticket sweeps during issue().
const PRUNE_INTERVAL_SECS: u64 = 15;

#[inline]
fn current_time_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[derive(Debug, Clone, Copy)]
struct Ticket {
    key_id: ApiKeyId,
    expires_at: Instant,
}

/// In-memory ticket registry. Cheap to clone via `Arc` in `AppState`.
#[derive(Debug)]
pub struct WsTicketStore {
    tickets: dashmap::DashMap<String, Ticket>,
    last_prune_secs: AtomicU64,
}

impl Default for WsTicketStore {
    fn default() -> Self {
        Self {
            tickets: dashmap::DashMap::new(),
            last_prune_secs: AtomicU64::new(current_time_secs()),
        }
    }
}

impl WsTicketStore {
    /// How long an unused ticket stays valid. Long enough for a browser to
    /// open the socket right after the `POST`, short enough that a logged
    /// ticket is useless by the time anyone reads the log.
    pub const TTL: Duration = Duration::from_secs(30);

    /// Hard ceiling on outstanding tickets. Every issue call prunes
    /// expired entries first; if a client still manages to exceed this,
    /// issuing fails closed instead of growing memory unbounded.
    pub const MAX_OUTSTANDING: usize = 1024;

    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a ticket bound to `key_id`. Returns `None` when the store is
    /// saturated (only possible under abuse: a legitimate dashboard holds
    /// at most one outstanding ticket at a time).
    pub fn issue(&self, key_id: ApiKeyId) -> Option<String> {
        let now = current_time_secs();
        let last_prune = self.last_prune_secs.load(Ordering::Relaxed);
        if self.tickets.len() >= Self::MAX_OUTSTANDING
            || now.saturating_sub(last_prune) >= PRUNE_INTERVAL_SECS
        {
            self.prune_expired();
        }
        if self.tickets.len() >= Self::MAX_OUTSTANDING {
            tracing::warn!(
                target: "openproxy::security",
                outstanding = self.tickets.len(),
                "ws ticket store saturated; refusing to issue"
            );
            return None;
        }
        let ticket = openproxy_core::api_keys::generate_opaque_token(TICKET_LEN);
        self.tickets.insert(
            ticket.clone(),
            Ticket {
                key_id,
                expires_at: Instant::now() + Self::TTL,
            },
        );
        Some(ticket)
    }

    /// Atomically remove and return the key bound to `ticket`, or `None`
    /// if the ticket is unknown, already used, or expired.
    pub fn consume(&self, ticket: &str) -> Option<ApiKeyId> {
        let (_, t) = self.tickets.remove(ticket)?;
        (Instant::now() < t.expires_at).then_some(t.key_id)
    }

    /// Number of live (unexpired, unused) tickets. Test/diagnostics helper.
    pub fn len(&self) -> usize {
        self.prune_expired();
        self.tickets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn prune_expired(&self) {
        self.last_prune_secs
            .store(current_time_secs(), Ordering::Relaxed);
        let now = Instant::now();
        self.tickets.retain(|_, t| t.expires_at > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_is_single_use() {
        let store = WsTicketStore::new();
        let key = ApiKeyId(7);
        let t = store.issue(key).expect("issue");
        assert_eq!(t.len(), TICKET_LEN);
        assert_eq!(store.consume(&t), Some(key));
        assert_eq!(store.consume(&t), None, "second use must fail");
    }

    #[test]
    fn unknown_ticket_is_rejected() {
        let store = WsTicketStore::new();
        assert_eq!(store.consume("nope"), None);
        assert_eq!(store.consume(""), None);
    }

    #[test]
    fn tickets_are_unique_and_bound_to_their_key() {
        let store = WsTicketStore::new();
        let a = store.issue(ApiKeyId(1)).unwrap();
        let b = store.issue(ApiKeyId(2)).unwrap();
        assert_ne!(a, b);
        assert_eq!(store.len(), 2);
        assert_eq!(store.consume(&b), Some(ApiKeyId(2)));
        assert_eq!(store.consume(&a), Some(ApiKeyId(1)));
        assert!(store.is_empty());
    }

    #[test]
    fn saturation_fails_closed() {
        let store = WsTicketStore::new();
        for _ in 0..WsTicketStore::MAX_OUTSTANDING {
            assert!(store.issue(ApiKeyId(1)).is_some());
        }
        assert!(store.issue(ApiKeyId(1)).is_none());
    }

    #[test]
    fn test_issue_pruning_throttled() {
        let store = WsTicketStore::new();
        let key = ApiKeyId(42);
        // Insert an expired ticket
        store.tickets.insert(
            "expired_ticket".to_string(),
            Ticket {
                key_id: key,
                expires_at: Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .expect("valid sub"),
            },
        );
        // Store just initialized, last_prune_ms is now. issue() without reaching MAX_OUTSTANDING
        // should NOT prune immediately because 15s have not elapsed.
        assert!(store.issue(key).is_some());
        assert!(store.tickets.contains_key("expired_ticket"));

        // Reset last_prune_secs to 0 to simulate 15s elapsed
        store.last_prune_secs.store(0, Ordering::Relaxed);
        assert!(store.issue(key).is_some());
        // Now expired ticket should have been pruned
        assert!(!store.tickets.contains_key("expired_ticket"));
    }
}
