//! openproxy-proxy-pool: Free proxy scrapers, health tester, rotation and pool persistence.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub(crate) use openproxy_types::error;
pub(crate) use openproxy_types::ids;

pub mod accounts {
    pub use openproxy_db::accounts::*;
    pub use openproxy_types::accounts::*;
}

pub mod providers {
    pub use openproxy_db::providers::*;
    pub use openproxy_types::providers::*;
}

pub use openproxy_notifications as notifications;

pub mod models {
    pub use openproxy_db::models::apply_auto_activation_with_retry;
}

pub mod free_proxies;
pub use free_proxies::*;
