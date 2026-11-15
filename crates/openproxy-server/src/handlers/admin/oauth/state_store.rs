/// Server-side OAuth state store (OP-19).
///
/// Security: `oauth_exchange` previously trusted the client-supplied
/// `redirect_uri` and `code_verifier` verbatim and never validated `state`,
/// so the CSRF/CSRF-mitigation trio of the authorization-code flow rested
/// entirely on the IdP. The authorize handler now persists the
/// `(provider, state)` pair it generated together with the PKCE verifier and
/// redirect URI it used; the exchange consumes that record (single-use,
/// 10-minute TTL) and uses the persisted values instead of whatever the
/// client posts.
pub struct OAuthStateStore {
    entries: dashmap::DashMap<(String, String), (String, String, std::time::Instant)>,
}

/// Lifetime of an authorize→exchange pair.
const OAUTH_STATE_TTL: std::time::Duration = std::time::Duration::from_secs(600);
/// Bound on distinct pending states.
const OAUTH_STATE_MAX_ENTRIES: usize = 4096;

impl Default for OAuthStateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthStateStore {
    pub fn new() -> Self {
        Self {
            entries: dashmap::DashMap::new(),
        }
    }

    /// Record a freshly generated authorize response.
    ///
    /// Saturation policy (audit H-5): the store used to `clear()` EVERYTHING
    /// when it hit the 4096-entry bound, so one client's authorize flood
    /// invalidated the pending states of every other operator mid-flow (their
    /// exchanges then failed as "unknown state"). Eviction is now the same
    /// oldest-first policy the `AdminAuthLimiter` uses: drop expired entries
    /// first, then the OLDEST records until there is room — a flood only ever
    /// displaces states that were issued before it started.
    pub fn insert(&self, provider: &str, state: &str, code_verifier: &str, redirect_uri: &str) {
        if self.entries.len() >= OAUTH_STATE_MAX_ENTRIES {
            self.evict_expired();
        }
        while self.entries.len() >= OAUTH_STATE_MAX_ENTRIES {
            self.evict_oldest();
        }
        self.entries.insert(
            (provider.to_string(), state.to_string()),
            (
                code_verifier.to_string(),
                redirect_uri.to_string(),
                std::time::Instant::now(),
            ),
        );
    }

    /// Consume the record for `(provider, state)` — single use. `None` when
    /// unknown, already used, or expired.
    pub fn consume(&self, provider: &str, state: &str) -> Option<(String, String)> {
        let (verifier, redirect_uri, created) = self
            .entries
            .remove(&(provider.to_string(), state.to_string()))?
            .1;
        if created.elapsed() > OAUTH_STATE_TTL {
            return None;
        }
        Some((verifier, redirect_uri))
    }

    fn evict_expired(&self) {
        self.entries
            .retain(|_, (_, _, created)| created.elapsed() <= OAUTH_STATE_TTL);
    }

    /// Drop the single oldest entry. Returns whether something was removed.
    fn evict_oldest(&self) -> bool {
        let mut oldest: Option<((String, String), std::time::Instant)> = None;
        for entry in &self.entries {
            let created = entry.value().2;
            if oldest
                .as_ref()
                .is_none_or(|(_, oldest_created)| created < *oldest_created)
            {
                oldest = Some((entry.key().clone(), created));
            }
        }
        match oldest {
            Some((key, _)) => self.entries.remove(&key).is_some(),
            None => false,
        }
    }

    /// Number of pending entries (test/observability).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the store is empty (test/observability).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
#[path = "state_store_tests.rs"]
mod state_store_tests;
