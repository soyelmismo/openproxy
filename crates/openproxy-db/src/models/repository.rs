use openproxy_types::{
    DiscoveredModel, Model, ModelId, ModelRowId, ProviderId, Result, TargetFormat, UpsertResult,
};
use std::sync::Arc;
use std::time::Duration;

use super::activation::{apply_auto_activation, set_active, set_active_bulk};
use super::crud::{
    create_custom, delete, find_active_by_name, find_active_by_provider_and_name, get_by_row_id,
    list_active, list_active_all, list_all, mark_expired, set_test_status,
};
use super::upsert::upsert_many;
use crate::conn::DbPool;

/// Model repository trait.
pub trait ModelRepository: Send + Sync {
    fn list_active(&self, provider: &ProviderId) -> Result<Vec<Model>>;
    fn list_active_all(&self) -> Result<Vec<Model>>;
    fn list_all(&self) -> Result<Vec<Model>>;
    fn get_by_row_id(&self, row_id: ModelRowId) -> Result<Option<Model>>;
    fn find_active_by_name(&self, model_id: &str) -> Result<Option<Model>>;
    fn find_active_by_provider_and_name(
        &self,
        provider: &ProviderId,
        model_id: &str,
    ) -> Result<Option<Model>>;
    fn set_active(&self, id: ModelRowId, active: bool) -> Result<()>;
    fn set_active_bulk(&self, provider: &ProviderId, active: bool) -> Result<u64>;
    fn set_test_status(&self, id: ModelRowId, status: i32) -> Result<()>;
    fn delete(&self, id: ModelRowId) -> Result<u64>;
    fn create_custom(
        &self,
        provider_id: &ProviderId,
        model_id: &ModelId,
        display_name: Option<&str>,
        target_format: TargetFormat,
        ttl_seconds: i64,
        model_type: Option<&str>,
    ) -> Result<ModelRowId>;
    fn mark_expired(&self) -> Result<usize>;
    fn upsert_many(
        &self,
        provider: &ProviderId,
        discovered: &[DiscoveredModel],
        ttl: Duration,
    ) -> Result<UpsertResult>;
    fn apply_auto_activation(&self, provider: &ProviderId, keyword: Option<&str>) -> Result<u64>;
}

/// Concrete SQLite repository implementation.
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
        let conn = self.pool.reader();
        list_active(&conn, provider)
    }

    fn list_active_all(&self) -> Result<Vec<Model>> {
        let conn = self.pool.reader();
        list_active_all(&conn)
    }

    fn list_all(&self) -> Result<Vec<Model>> {
        let conn = self.pool.reader();
        list_all(&conn)
    }

    fn get_by_row_id(&self, row_id: ModelRowId) -> Result<Option<Model>> {
        let conn = self.pool.reader();
        get_by_row_id(&conn, row_id)
    }

    fn find_active_by_name(&self, model_id: &str) -> Result<Option<Model>> {
        let conn = self.pool.reader();
        find_active_by_name(&conn, model_id)
    }

    fn find_active_by_provider_and_name(
        &self,
        provider: &ProviderId,
        model_id: &str,
    ) -> Result<Option<Model>> {
        let conn = self.pool.reader();
        find_active_by_provider_and_name(&conn, provider, model_id)
    }

    fn set_active(&self, id: ModelRowId, active: bool) -> Result<()> {
        let conn = self.pool.writer();
        set_active(&conn, id, active)
    }

    fn set_active_bulk(&self, provider: &ProviderId, active: bool) -> Result<u64> {
        let conn = self.pool.writer();
        set_active_bulk(&conn, provider, active)
    }

    fn set_test_status(&self, id: ModelRowId, status: i32) -> Result<()> {
        let conn = self.pool.writer();
        set_test_status(&conn, id, status)
    }

    fn delete(&self, id: ModelRowId) -> Result<u64> {
        let conn = self.pool.writer();
        delete(&conn, id)
    }

    fn create_custom(
        &self,
        provider_id: &ProviderId,
        model_id: &ModelId,
        display_name: Option<&str>,
        target_format: TargetFormat,
        ttl_seconds: i64,
        model_type: Option<&str>,
    ) -> Result<ModelRowId> {
        let conn = self.pool.writer();
        create_custom(
            &conn,
            provider_id,
            model_id,
            display_name,
            target_format,
            ttl_seconds,
            model_type,
        )
    }

    fn mark_expired(&self) -> Result<usize> {
        let conn = self.pool.writer();
        mark_expired(&conn)
    }

    fn upsert_many(
        &self,
        provider: &ProviderId,
        discovered: &[DiscoveredModel],
        ttl: Duration,
    ) -> Result<UpsertResult> {
        let conn = self.pool.writer();
        upsert_many(&conn, provider, discovered, ttl)
    }

    fn apply_auto_activation(&self, provider: &ProviderId, keyword: Option<&str>) -> Result<u64> {
        let conn = self.pool.writer();
        apply_auto_activation(&conn, provider, keyword)
    }
}
