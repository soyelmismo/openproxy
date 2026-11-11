pub use super::{CODEBUDDY_MODELS, CodeBuddyModelDef};

pub type CodeBuddyModelCreditCost = CodeBuddyModelDef;

pub const CODEBUDDY_MODEL_CREDIT_COSTS: &[CodeBuddyModelDef] = CODEBUDDY_MODELS;

mod auth;
mod fetch;
mod parse;
mod request;
mod urls;

pub use auth::{is_codebuddy_auth_error, parse_codebuddy_provider_specific};
pub use fetch::fetch_codebuddy_quota_unified;
pub use parse::{
    build_codebuddy_quota_model_details, calculate_next_midnight_cst_unix_secs,
    parse_codebuddy_accounts_quota, parse_codebuddy_resource_quota,
    parse_codebuddy_resource_quota_at, parse_cst_datetime_to_unix_secs,
};
pub use request::{
    build_codebuddy_accounts_request, build_codebuddy_accounts_request_with_url,
    build_codebuddy_resource_request,
};
pub use urls::{
    CN_CODEBUDDY_BASE_URL, CODEBUDDY_ACCOUNTS_URL, CODEBUDDY_GET_USER_RESOURCE_SUMMARY_URL,
    CODEBUDDY_GET_USER_RESOURCE_URL, DEFAULT_CODEBUDDY_BASE_URL, LEGACY_CODEBUDDY_BASE_URL,
    MIRROR_CODEBUDDY_BASE_URL, codebuddy_base_url, codebuddy_candidate_origins,
    codebuddy_origin_from_base_url,
};

// Re-exported so the submodules keep the exact `super::` surface the original
// single-file module had against the parent `codebuddy` module.
pub(crate) use super::apply_codebuddy_spoofing_headers;
pub(crate) use super::{is_codebuddy_model_free, parse_codebuddy_credits_multiplier};

#[cfg(test)]
#[path = "quota_tests.rs"]
mod tests;
