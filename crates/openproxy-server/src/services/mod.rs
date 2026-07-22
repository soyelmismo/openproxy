//! Application services centralizing business logic and decoupling HTTP handlers.

use std::sync::Arc;
use crate::repositories::{
    AccountRepository, ApiKeyRepository, ComboRepository, ModelRepository,
    Repository, SqliteRepository,
};
use openproxy_core::{api_keys as core_api_keys, models::Model};
use openproxy_db as db;
use openproxy_types::{
    combos::Combo,
    AccountId, ApiKeyId, ComboId, CoreError, ProviderId,
};

/// Service for managing API Keys business logic.
pub struct ApiKeyService {
    repo: Arc<dyn ApiKeyRepository>,
}

impl ApiKeyService {
    pub fn new(repo: Arc<dyn ApiKeyRepository>) -> Self {
        Self { repo }
    }

    pub fn list(&self) -> Result<Vec<core_api_keys::ApiKey>, CoreError> {
        self.repo.list_api_keys()
    }

    pub fn create(
        &self,
        input: core_api_keys::CreateApiKeyInput,
        creator: &str,
    ) -> Result<(core_api_keys::ApiKey, String), CoreError> {
        self.repo.create_api_key(input, creator)
    }

    pub fn get_by_id(&self, id: ApiKeyId) -> Result<Option<core_api_keys::ApiKey>, CoreError> {
        self.repo.get_api_key_by_id(id)
    }

    pub fn update(
        &self,
        id: ApiKeyId,
        params: core_api_keys::UpdateParams,
    ) -> Result<(), CoreError> {
        self.repo.update_api_key(id, params)
    }

    pub fn count_active(&self) -> Result<u64, CoreError> {
        self.repo.count_active_api_keys()
    }
}

/// Service for managing Account business logic.
pub struct AccountService {
    repo: Arc<dyn AccountRepository>,
}

impl AccountService {
    pub fn new(repo: Arc<dyn AccountRepository>) -> Self {
        Self { repo }
    }

    pub fn list(
        &self,
        provider: Option<&ProviderId>,
        master_key: &db::MasterKey,
    ) -> Result<Vec<openproxy_core::accounts::Account>, CoreError> {
        self.repo.list_accounts(provider, master_key)
    }

    pub fn create(
        &self,
        master_key: &db::MasterKey,
        input: openproxy_core::admin::CreateAccountInput,
    ) -> Result<AccountId, CoreError> {
        self.repo.create_account(master_key, input)
    }

    pub fn delete(&self, id: AccountId) -> Result<(), CoreError> {
        self.repo.delete_account(id)
    }

    pub fn set_health(
        &self,
        id: AccountId,
        health: openproxy_core::accounts::HealthStatus,
    ) -> Result<(), CoreError> {
        self.repo.set_account_health(id, health)
    }

    pub fn update_api_key(
        &self,
        master_key: &db::MasterKey,
        id: AccountId,
        input: openproxy_core::admin::UpdateAccountApiKeyInput,
    ) -> Result<(), CoreError> {
        self.repo.update_account_api_key(master_key, id, input)
    }

    pub fn get_api_key(
        &self,
        master_key: &db::MasterKey,
        id: AccountId,
    ) -> Result<String, CoreError> {
        self.repo.get_account_api_key(master_key, id)
    }

    pub fn update_label(
        &self,
        id: AccountId,
        input: openproxy_core::admin::UpdateAccountLabelInput,
    ) -> Result<(), CoreError> {
        self.repo.update_account_label(id, input)
    }
}

/// Service for managing Combo business logic.
pub struct ComboService {
    repo: Arc<dyn ComboRepository>,
}

impl ComboService {
    pub fn new(repo: Arc<dyn ComboRepository>) -> Self {
        Self { repo }
    }

    pub fn list_combos(&self) -> Result<Vec<Combo>, CoreError> {
        self.repo.list_combos()
    }

    pub fn compute_effective_context_window(&self, combo_id: ComboId) -> Result<Option<i64>, CoreError> {
        self.repo.compute_effective_context_window(combo_id)
    }
}

/// Service for managing Model business logic.
pub struct ModelService {
    repo: Arc<dyn ModelRepository>,
}

impl ModelService {
    pub fn new(repo: Arc<dyn ModelRepository>) -> Self {
        Self { repo }
    }

    pub fn list_active_all(&self, timeout: std::time::Duration) -> Result<Vec<Model>, CoreError> {
        self.repo.list_active_all_models(timeout)
    }
}

/// Container for all application services.
pub struct Services {
    pub repository: Arc<dyn Repository>,
    pub api_keys: Arc<ApiKeyService>,
    pub accounts: Arc<AccountService>,
    pub combos: Arc<ComboService>,
    pub models: Arc<ModelService>,
}

impl Services {
    pub fn new(db_pool: Arc<db::DbPool>) -> Self {
        let repo = Arc::new(SqliteRepository::new(db_pool));
        Self::from_repositories(
            repo.clone(),
            repo.clone(),
            repo.clone(),
            repo.clone(),
            repo,
        )
    }

    pub fn from_repositories(
        repository: Arc<dyn Repository>,
        api_keys_repo: Arc<dyn ApiKeyRepository>,
        accounts_repo: Arc<dyn AccountRepository>,
        combos_repo: Arc<dyn ComboRepository>,
        models_repo: Arc<dyn ModelRepository>,
    ) -> Self {
        let api_keys = Arc::new(ApiKeyService::new(api_keys_repo));
        let accounts = Arc::new(AccountService::new(accounts_repo));
        let combos = Arc::new(ComboService::new(combos_repo));
        let models = Arc::new(ModelService::new(models_repo));

        Self {
            repository,
            api_keys,
            accounts,
            combos,
            models,
        }
    }
}
