//! Discovery service: fetch → upsert → auto-activate, shared by
//! [`crate::admin::refresh_models`] and [`crate::discovery_scheduler`] through
//! [`DiscoveryService::refresh_and_activate`].

use super::{DiscoveredModel, UpsertResult};
use crate::error::Result;
use crate::ids::ProviderId;
use openproxy_db::models::ModelRepository;
use std::time::Duration;

/// Model-discovery lifecycle, generic over `R: ModelRepository` so tests can
/// inject a mock.
pub struct DiscoveryService<R: ModelRepository> {
    repo: R,
}

impl<R: ModelRepository> DiscoveryService<R> {
    pub fn new(repo: R) -> Self {
        Self { repo }
    }

    /// Upsert `discovered` (the upstream catalog for `provider`) with `ttl` as the
    /// cache lifetime for new rows, then re-apply auto-activation when `keyword`
    /// is `Some`.
    pub fn refresh_and_activate(
        &self,
        provider: &ProviderId,
        discovered: &[DiscoveredModel],
        ttl: Duration,
        keyword: Option<&str>,
    ) -> Result<UpsertResult> {
        let result = self.repo.upsert_many(provider, discovered, ttl)?;

        if let Some(kw) = keyword {
            // non-fatal: the next tick retries
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

    /// The underlying repository, for queries outside the refresh flow.
    pub fn repository(&self) -> &R {
        &self.repo
    }
}
