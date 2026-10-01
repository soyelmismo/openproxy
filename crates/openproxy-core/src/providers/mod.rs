//! Provider registry CRUD and domain utilities.

pub use openproxy_db::providers::{
    NewProvider, UpdateProviderParams, create, delete, delete_provider_favicon, get,
    get_auth_types, get_provider_favicon, list, list_active, set_active, set_provider_favicon,
    update, update_current_proxy,
};
pub use openproxy_types::providers::*;

pub use crate::seed::{builtin_provider_ids, is_builtin};

pub use openproxy_discovery::providers::{
    MAX_FAVICON_BYTES, extract_apex_domain, extract_domain, fetch_and_cache_favicon,
    fetch_favicon_data_uri, fetch_favicon_raw, is_loopback_or_private_host, validate_favicon_bytes,
};

#[cfg(test)]
use crate::ids::ProviderId;

#[cfg(test)]
mod tests;
