//! Fixtures shared by `retry::tests` and `retry::adversarial`, so a helper fix
//! reaches both. `cfg(test)` only.

use std::sync::atomic::Ordering;

use super::counters::INVALID_GRANT_COUNTERS;
use crate::error::CoreError;
use crate::ids::AccountId;
use crate::oauth::TokenResponse;

/// Labelled token so tests can assert the exact one a retry loop returned.
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

/// `invalid_grant` error. The loop matches by substring, so the literal must be
/// part of `Display`.
pub(super) fn invalid_grant_err() -> CoreError {
    CoreError::Auth("server returned invalid_grant".into())
}

/// Non-`invalid_grant` error: the loop must short-circuit before the counter.
pub(super) fn network_err() -> CoreError {
    CoreError::UpstreamConnection("connection refused".into())
}

/// Drop the counter entry so a previous test cannot leak state into the next.
pub(super) fn clear_counter(account_id: AccountId) {
    INVALID_GRANT_COUNTERS.remove(&account_id.0);
}

/// Current counter value, 0 if absent.
pub(super) fn get_counter_val(account_id: AccountId) -> u32 {
    INVALID_GRANT_COUNTERS
        .get(&account_id.0)
        .map_or(0, |e| e.value().load(Ordering::Relaxed))
}
