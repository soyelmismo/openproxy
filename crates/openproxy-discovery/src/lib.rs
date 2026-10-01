//! openproxy-discovery: Models discovery, models.dev sync, model normalization,
//! and discovery background scheduler.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub(crate) use openproxy_types::error;
pub(crate) use openproxy_types::ids;

pub mod accounts {
    pub use openproxy_db::accounts::*;
    pub use openproxy_types::accounts::*;
}

pub mod providers;

pub mod seed {
    pub use openproxy_types::providers::VIRTUAL_COMBO_PROVIDER_ID;

    pub fn builtin_provider_ids() -> Vec<String> {
        openproxy_adapters::adapters::builtin_adapters()
            .iter()
            .map(|a| a.config().id.0.clone())
            .collect()
    }

    pub fn is_builtin(id: &str) -> bool {
        builtin_provider_ids().iter().any(|s| s == id)
    }
}

pub mod admin {
    pub use crate::models::refresh_models;
}

pub use openproxy_notifications as notifications;
pub use openproxy_pricing as pricing;

pub mod discovery_scheduler;
pub mod model_normalize;
pub mod models;
pub mod models_dev_sync;

pub use discovery_scheduler::*;
pub use model_normalize::*;
pub use models::*;
pub use models_dev_sync::*;
