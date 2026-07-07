//! Repository abstraction for model persistence.
//!
//! [`ModelRepository`] defines the contract that the rest of the
//! codebase programs against. [`SqliteModelRepository`] is the
//! production implementation backed by the `DbPool` connection pool.
//!
//! The trait follows the same pattern as
//! [`crate::pipeline::repository::PipelineRepository`]: each method
//! opens its own connection internally, so callers don't manage
//! connection lifetimes.

use super::{DiscoveredModel, Model, TargetFormat, UpsertResult};
use crate::db::DbPool;
use crate::error::Result;
use crate::ids::{ModelId, ModelRowId, ProviderId};
use std::sync::Arc;
use std::time::Duration;

/// Persistence contract for model CRUD.
///
/// Every method is `&self` and infallible with respect to connection
/// management (the implementation opens/closes a connection per call).
/// This keeps the trait mockable for unit tests and decouples callers
/// from the concrete storage backend.
pub trait ModelRepository: Send + Sync {
    // ── Queries ──────────────────────────────────────────────────

    /// List active models for a single provider.
    fn list_active(&self, provider: &ProviderId) -> Result<Vec<Model>>;

    /// List active models across all providers.
    fn list_active_all(&self) -> Result<Vec<Model>>;

    /// List every model row (active + inactive).
    fn list_all(&self) -> Result<Vec<Model>>;

    /// Fetch a model by primary key.
    fn get_by_row_id(&self, row_id: ModelRowId) -> Result<Option<Model>>;

    /// Find an active model by exact `model_id`.
    fn find_active_by_name(&self, model_id: &str) -> Result<Option<Model>>;

    /// Find an active model scoped to a specific provider.
    fn find_active_by_provider_and_name(
        &self,
        provider: &ProviderId,
        model_id: &str,
    ) -> Result<Option<Model>>;

    // ── Mutations ────────────────────────────────────────────────

    /// Toggle the soft-disable flag.
    fn set_active(&self, id: ModelRowId, active: bool) -> Result<()>;

    /// Bulk toggle for all non-custom models of a provider.
    fn set_active_bulk(&self, provider: &ProviderId, active: bool) -> Result<u64>;

    /// Stamp test result on a model row.
    fn set_test_status(&self, id: ModelRowId, status: i32) -> Result<()>;

    /// Hard-delete a model (cascades to `combo_targets`).
    fn delete(&self, id: ModelRowId) -> Result<u64>;

    /// Insert or update a hand-crafted custom model.
    fn create_custom(
        &self,
        provider_id: &ProviderId,
        model_id: &ModelId,
        display_name: Option<&str>,
        target_format: TargetFormat,
        ttl_seconds: i64,
    ) -> Result<ModelRowId>;

    /// Delete orphan rows older than 7 days.
    fn mark_expired(&self) -> Result<usize>;

    // ── Sync / Discovery ─────────────────────────────────────────

    /// Upsert a batch of discovered models and remove vanished ones.
    fn upsert_many(
        &self,
        provider: &ProviderId,
        discovered: &[DiscoveredModel],
        ttl: Duration,
    ) -> Result<UpsertResult>;

    /// Re-apply auto-activation rules after a discovery refresh.
    fn apply_auto_activation(&self, provider: &ProviderId, keyword: Option<&str>) -> Result<u64>;
}

// ─── SQLite implementation ──────────────────────────────────────────

/// Production [`ModelRepository`] backed by a `DbPool`.
///
/// Each method opens a connection from the pool, delegates to the
/// free functions in [`super::crud`], and returns the result.
pub struct SqliteModelRepository {
    pool: Arc<DbPool>,
}

impl SqliteModelRepository {
    pub fn new(pool: Arc<DbPool>) -> Self {
        Self { pool }
    }
}

impl ModelRepository for SqliteModelRepository {
    fn list_active(&self, provider: &ProviderId) -> Result<Vec<Model>> {
        let conn = self.pool.open_connection()?;
        super::crud::list_active(&conn, provider)
    }

    fn list_active_all(&self) -> Result<Vec<Model>> {
        let conn = self.pool.open_connection()?;
        super::crud::list_active_all(&conn)
    }

    fn list_all(&self) -> Result<Vec<Model>> {
        let conn = self.pool.open_connection()?;
        super::crud::list_all(&conn)
    }

    fn get_by_row_id(&self, row_id: ModelRowId) -> Result<Option<Model>> {
        let conn = self.pool.open_connection()?;
        super::crud::get_by_row_id(&conn, row_id)
    }

    fn find_active_by_name(&self, model_id: &str) -> Result<Option<Model>> {
        let conn = self.pool.open_connection()?;
        super::crud::find_active_by_name(&conn, model_id)
    }

    fn find_active_by_provider_and_name(
        &self,
        provider: &ProviderId,
        model_id: &str,
    ) -> Result<Option<Model>> {
        let conn = self.pool.open_connection()?;
        super::crud::find_active_by_provider_and_name(&conn, provider, model_id)
    }

    fn set_active(&self, id: ModelRowId, active: bool) -> Result<()> {
        let conn = self.pool.open_connection()?;
        super::crud::set_active(&conn, id, active)
    }

    fn set_active_bulk(&self, provider: &ProviderId, active: bool) -> Result<u64> {
        let conn = self.pool.open_connection()?;
        super::crud::set_active_bulk(&conn, provider, active)
    }

    fn set_test_status(&self, id: ModelRowId, status: i32) -> Result<()> {
        let conn = self.pool.open_connection()?;
        super::crud::set_test_status(&conn, id, status)
    }

    fn delete(&self, id: ModelRowId) -> Result<u64> {
        let conn = self.pool.open_connection()?;
        super::crud::delete(&conn, id)
    }

    fn create_custom(
        &self,
        provider_id: &ProviderId,
        model_id: &ModelId,
        display_name: Option<&str>,
        target_format: TargetFormat,
        ttl_seconds: i64,
    ) -> Result<ModelRowId> {
        let conn = self.pool.open_connection()?;
        super::crud::create_custom(
            &conn,
            provider_id,
            model_id,
            display_name,
            target_format,
            ttl_seconds,
        )
    }

    fn mark_expired(&self) -> Result<usize> {
        let conn = self.pool.open_connection()?;
        super::crud::mark_expired(&conn)
    }

    fn upsert_many(
        &self,
        provider: &ProviderId,
        discovered: &[DiscoveredModel],
        ttl: Duration,
    ) -> Result<UpsertResult> {
        let conn = self.pool.open_connection()?;
        super::crud::upsert_many(&conn, provider, discovered, ttl)
    }

    fn apply_auto_activation(&self, provider: &ProviderId, keyword: Option<&str>) -> Result<u64> {
        let conn = self.pool.open_connection()?;
        super::crud::apply_auto_activation(&conn, provider, keyword)
    }
}
