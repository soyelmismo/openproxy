//! Account storage, OAuth tokens, credentials and quota persistence.

use std::sync::atomic::{AtomicU64, Ordering};

/// Monotonic epoch bumped by every `accounts`-table mutation in this
/// process. Read-side caches (e.g. the pipeline's `get_account` cache) key
/// themselves on this value: a bump makes every cached entry stale at once
/// without tracking which function wrote.
static ACCOUNTS_EPOCH: AtomicU64 = AtomicU64::new(0);

/// Current epoch value.
pub fn epoch() -> u64 {
    ACCOUNTS_EPOCH.load(Ordering::Acquire)
}

/// Bump the epoch. Called at the top of every write path — cheap enough
/// (one atomic fetch-add) that bumping on no-op writes is fine.
pub(crate) fn bump_epoch() {
    ACCOUNTS_EPOCH.fetch_add(1, Ordering::AcqRel);
}

crate::def_table_select!(
    account_select,
    "accounts",
    "id, provider_id, label, priority, extra_config_json, \
     health_status, rate_limited_until, \
     quota_session_used, quota_session_limit, quota_session_reset_at, \
     quota_weekly_used, quota_weekly_limit, quota_weekly_reset_at, \
     quota_plan_name, quota_last_fetched_at, quota_fetch_error, \
     quota_model_details, \
     auth_type, email, oauth_scope, oauth_provider_specific, expires_at, \
     created_at, current_proxy_id, \
     quota_pools"
);

pub mod crud;
pub mod oauth;
pub mod quota;

#[cfg(test)]
mod tests;

pub use crud::*;
pub use oauth::*;
pub use quota::*;
