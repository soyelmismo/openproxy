//! openproxy-analytics: Analytics queries, latency percentiles, race stats and usage tracking.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub(crate) use openproxy_types::error;
#[cfg(test)]
pub(crate) use openproxy_types::ids;

pub mod analytics;
pub mod usage;
