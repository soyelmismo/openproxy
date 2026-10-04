//! Account storage, OAuth tokens, credentials and quota persistence.

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
     created_at, current_proxy_id"
);

pub mod crud;
pub mod oauth;
pub mod quota;

#[cfg(test)]
mod tests;

pub use crud::*;
pub use oauth::*;
pub use quota::*;
