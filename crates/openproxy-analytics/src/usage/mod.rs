//! Usage queries: list, summary, by-model, by-account, by-status, errors.
//!
//! See docs/mvp-spec.md §7 (Analytics Queries) and §8 (SQLite Schema).
//!
//! This module is the *read* side of the usage table; inserts live in
//! [`crate::cost::record`]. All queries share a common filter shape and are
//! race-aware: `winners` counts rows where `race_lost = 0`, `losers` counts
//! `race_lost = 1`, and `unique_requests` is the number of DISTINCT
//! `request_id`s with a successful row, so a race of N losers + 1 winner
//! counts as one logical request.
//!
//! ## Broadcast channel
//!
//! The module also owns the canonical in-process broadcast channel for newly
//! inserted usage rows. After [`crate::cost::record`] inserts a row, it calls
//! [`publish_usage_row`] to push the new row to all connected WebSocket
//! clients. The sender is stored in a `std::sync::OnceLock` so it is

pub mod analytics;

pub use analytics::prune_expired_recording_bodies;
pub use analytics::prune_expired_usage_rows;
pub use analytics::*;

pub mod broadcast;
pub use broadcast::*;

#[cfg(test)]
mod tests;
