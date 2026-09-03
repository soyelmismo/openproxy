//! Shared test helpers for the antigravity OAuth module.
//!
//! Both `retry::tests` and `retry::adversarial_retry_tests` previously
//! duplicated the same five fixtures (`dummy_token`, `invalid_grant_err`,
//! `network_err`, `clear_counter`, `get_counter_val`). They live here
//! so a fix to a helper propagates to both modules.
//!
//! `cfg(test)` only — never compiled into release builds.

use std::sync::atomic::Ordering;

use super::counters::INVALID_GRANT_COUNTERS;
use crate::error::CoreError;
use crate::ids::AccountId;
use crate::oauth::TokenResponse;

/// Build a token with a distinguishable label so tests can assert the
/// exact token a retry loop returned.
pub(super) fn dummy_token(label: &str) -> TokenResponse {
    TokenResponse {
        access_token: format!("access-{label}"),
        token_type: "Bearer".into(),
        expires_in: Some(3600),
        refresh_token: Some(format!("refresh-{label}")),
        scope: None,
        id_token: None,
    }
}

/// Synthesize an `invalid_grant` error. The retry loop matches by
/// substring (`"invalid_grant"`), so the literal string must be part
/// of `Display`.
pub(super) fn invalid_grant_err() -> CoreError {
    CoreError::Auth("server returned invalid_grant".into())
}

/// A non-`invalid_grant` error used to verify the retry loop
/// short-circuits and never reaches the counter.
pub(super) fn network_err() -> CoreError {
    CoreError::UpstreamConnection("connection refused".into())
}

/// Drop the entry from the global counter map so a previous test
/// cannot leak state into the next.
pub(super) fn clear_counter(account_id: AccountId) {
    INVALID_GRANT_COUNTERS.remove(&account_id.0);
}

/// Read the current counter value (0 if absent). Tests assert exact
/// values without unwrapping the `DashMap` directly.
pub(super) fn get_counter_val(account_id: AccountId) -> u32 {
    INVALID_GRANT_COUNTERS
        .get(&account_id.0)
        .map_or(0, |e| e.value().load(Ordering::Relaxed))
}