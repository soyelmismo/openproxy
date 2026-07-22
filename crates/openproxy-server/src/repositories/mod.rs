//! Repository traits and SQLite implementation for data persistence layer.

use openproxy_core::{api_keys as core_api_keys, models::Model};
use openproxy_db as db;
use openproxy_types::{AccountId, ApiKeyId, ComboId, CoreError, ProviderId, combos::Combo};
use std::sync::Arc;
use std::time::Duration;

/// Base repository trait providing database connection guards.
pub trait Repository: Send + Sync {
    fn reader(&self) -> db::ReaderGuard<'_>;
    fn writer(&self) -> db::WriterGuard<'_>;
    fn try_writer_for(&self, timeout: Duration) -> Option<db::WriterGuard<'_>>;
}

/// Concrete SQLite implementation of Repository.
#[derive(Clone)]
pub struct SqliteRepository {
    pool: Arc<db::DbPool>,
}

impl SqliteRepository {
    pub fn new(pool: Arc<db::DbPool>) -> Self {
        Self { pool }
    }
}

impl Repository for SqliteRepository {
    fn reader(&self) -> db::ReaderGuard<'_> {
        self.pool.reader()
    }

    fn writer(&self) -> db::WriterGuard<'_> {
        self.pool.writer()
    }

    fn try_writer_for(&self, timeout: Duration) -> Option<db::WriterGuard<'_>> {
        self.pool.try_writer_for(timeout)
    }
}

/// ApiKey repository trait.
pub trait ApiKeyRepository: Send + Sync {
    fn list_api_keys(&self) -> Result<Vec<core_api_keys::ApiKey>, CoreError>;
    fn create_api_key(
        &self,
        input: core_api_keys::CreateApiKeyInput,
        creator: &str,
    ) -> Result<(core_api_keys::ApiKey, String), CoreError>;
    fn get_api_key_by_id(&self, id: ApiKeyId) -> Result<Option<core_api_keys::ApiKey>, CoreError>;
    fn update_api_key(
        &self,
        id: ApiKeyId,
        params: core_api_keys::UpdateParams,
    ) -> Result<(), CoreError>;
    fn count_active_api_keys(&self) -> Result<u64, CoreError>;
}

/// Account repository trait.
pub trait AccountRepository: Send + Sync {
    fn list_accounts(
        &self,
        provider: Option<&ProviderId>,
        master_key: &db::MasterKey,
    ) -> Result<Vec<openproxy_core::accounts::Account>, CoreError>;
    fn create_account(
        &self,
        master_key: &db::MasterKey,
        input: openproxy_core::admin::CreateAccountInput,
    ) -> Result<AccountId, CoreError>;
    fn delete_account(&self, id: AccountId) -> Result<(), CoreError>;
    fn set_account_health(
        &self,
        id: AccountId,
        health: openproxy_core::accounts::HealthStatus,
    ) -> Result<(), CoreError>;
    fn update_account_api_key(
        &self,
        master_key: &db::MasterKey,
        id: AccountId,
        input: openproxy_core::admin::UpdateAccountApiKeyInput,
    ) -> Result<(), CoreError>;
    fn get_account_api_key(
        &self,
        master_key: &db::MasterKey,
        id: AccountId,
    ) -> Result<String, CoreError>;
    fn update_account_label(
        &self,
        id: AccountId,
        input: openproxy_core::admin::UpdateAccountLabelInput,
    ) -> Result<(), CoreError>;
}

/// Combo repository trait.
pub trait ComboRepository: Send + Sync {
    fn list_combos(&self) -> Result<Vec<Combo>, CoreError>;
    fn compute_effective_context_window(&self, combo_id: ComboId)
    -> Result<Option<i64>, CoreError>;
}

/// Model repository trait.
pub trait ModelRepository: Send + Sync {
    fn list_active_all_models(&self, timeout: Duration) -> Result<Vec<Model>, CoreError>;
}

impl ApiKeyRepository for SqliteRepository {
    fn list_api_keys(&self) -> Result<Vec<core_api_keys::ApiKey>, CoreError> {
        let r = self.reader();
        core_api_keys::list(&r)
    }

    fn create_api_key(
        &self,
        input: core_api_keys::CreateApiKeyInput,
        creator: &str,
    ) -> Result<(core_api_keys::ApiKey, String), CoreError> {
        let w = self.writer();
        core_api_keys::create(&w, input, creator)
    }

    fn get_api_key_by_id(&self, id: ApiKeyId) -> Result<Option<core_api_keys::ApiKey>, CoreError> {
        let r = self.reader();
        core_api_keys::get_by_id(&r, id)
    }

    fn update_api_key(
        &self,
        id: ApiKeyId,
        params: core_api_keys::UpdateParams,
    ) -> Result<(), CoreError> {
        let w = self.writer();
        core_api_keys::update(&w, id, params)
    }

    fn count_active_api_keys(&self) -> Result<u64, CoreError> {
        let r = self.reader();
        core_api_keys::count_active(&r)
    }
}

impl AccountRepository for SqliteRepository {
    fn list_accounts(
        &self,
        provider: Option<&ProviderId>,
        master_key: &db::MasterKey,
    ) -> Result<Vec<openproxy_core::accounts::Account>, CoreError> {
        let r = self.reader();
        openproxy_core::admin::list_accounts(&r, provider, master_key)
    }

    fn create_account(
        &self,
        master_key: &db::MasterKey,
        input: openproxy_core::admin::CreateAccountInput,
    ) -> Result<AccountId, CoreError> {
        let w = self.writer();
        openproxy_core::admin::create_account(&w, master_key, input)
    }

    fn delete_account(&self, id: AccountId) -> Result<(), CoreError> {
        let w = self.writer();
        openproxy_core::admin::delete_account(&w, id)
    }

    fn set_account_health(
        &self,
        id: AccountId,
        health: openproxy_core::accounts::HealthStatus,
    ) -> Result<(), CoreError> {
        let w = self.writer();
        openproxy_core::accounts::set_health(&w, id, health)
    }

    fn update_account_api_key(
        &self,
        master_key: &db::MasterKey,
        id: AccountId,
        input: openproxy_core::admin::UpdateAccountApiKeyInput,
    ) -> Result<(), CoreError> {
        let w = self.writer();
        openproxy_core::admin::update_account_api_key(&w, master_key, id, input)
    }

    fn get_account_api_key(
        &self,
        master_key: &db::MasterKey,
        id: AccountId,
    ) -> Result<String, CoreError> {
        let r = self.reader();
        openproxy_core::admin::get_account_api_key(&r, master_key, id)
    }

    fn update_account_label(
        &self,
        id: AccountId,
        input: openproxy_core::admin::UpdateAccountLabelInput,
    ) -> Result<(), CoreError> {
        let w = self.writer();
        openproxy_core::admin::update_account_label(&w, id, input)
    }
}

impl ComboRepository for SqliteRepository {
    fn list_combos(&self) -> Result<Vec<Combo>, CoreError> {
        let w = self.writer();
        db::combos::list_combos(&w)
    }

    fn compute_effective_context_window(
        &self,
        combo_id: ComboId,
    ) -> Result<Option<i64>, CoreError> {
        let w = self.writer();
        db::combos::compute_effective_context_window(&w, combo_id)
    }
}

impl ModelRepository for SqliteRepository {
    fn list_active_all_models(&self, timeout: Duration) -> Result<Vec<Model>, CoreError> {
        let w = self.try_writer_for(timeout).ok_or_else(|| {
            CoreError::ServiceUnavailable("database busy; retry in a few seconds".into())
        })?;
        openproxy_core::models::list_active_all(&w)
    }
}
