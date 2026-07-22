//! Discovery service: orchestrates fetch → upsert → auto-activate.
//!
//! Encapsulates the two-step refresh flow that was previously
//! duplicated across [`crate::admin::refresh_models`] (step 7) and
//! [`crate::discovery_scheduler`] (step 6). Both call sites can now
//! delegate to [`DiscoveryService::refresh_and_activate`] for the
//! full lifecycle.

use openproxy_db::models::ModelRepository;
use super::{DiscoveredModel, UpsertResult};
use crate::error::Result;
use crate::ids::ProviderId;
use std::time::Duration;

/// Orchestrates the model-discovery lifecycle.
///
/// Generic over `R: ModelRepository` so production code uses
/// `SqliteModelRepository` while tests can inject a mock.
pub struct DiscoveryService<R: ModelRepository> {
    repo: R,
}

impl<R: ModelRepository> DiscoveryService<R> {
    pub fn new(repo: R) -> Self {
        Self { repo }
    }

    /// Full refresh flow: upsert discovered models, then optionally
    /// re-apply the auto-activation keyword rule.
    ///
    /// This is the single entry point that replaces the scattered
    /// `upsert_many` + `apply_auto_activation` dance.
    ///
    /// # Arguments
    ///
    /// * `provider` — the provider whose catalog was just fetched.
    /// * `discovered` — the models reported by the upstream `/models`
    ///   endpoint.
    /// * `ttl` — cache lifetime for newly inserted rows.
    /// * `keyword` — if `Some`, re-applies auto-activation after the
    ///   upsert. If `None`, auto-activation is skipped.
    pub fn refresh_and_activate(
        &self,
        provider: &ProviderId,
        discovered: &[DiscoveredModel],
        ttl: Duration,
        keyword: Option<&str>,
    ) -> Result<UpsertResult> {
        let result = self.repo.upsert_many(provider, discovered, ttl)?;

        if let Some(kw) = keyword {
            // Auto-activation errors are non-fatal: log and continue.
            // The next discovery tick will retry.
            if let Err(e) = self.repo.apply_auto_activation(provider, Some(kw)) {
                tracing::warn!(
                    provider = %provider,
                    error = %e,
                    "DiscoveryService: auto-activation failed after upsert",
                );
            }
        }

        Ok(result)
    }

    /// Access the underlying repository (e.g. for ad-hoc queries
    /// outside the refresh flow).
    pub fn repository(&self) -> &R {
        &self.repo
    }
}
