//! Application services centralizing business logic and decoupling HTTP handlers.
//!
//! Database-backed services are synchronous and must run inside blocking tasks.

pub mod pipeline_runner;
pub mod sse_coalesce;
pub use pipeline_runner::{PipelineRunner, PreparedPipelineRequest};

use crate::repositories::{
    AccountRepository, ApiKeyRepository, ComboRepository, ModelRepository, Repository,
    SqliteRepository,
};
use openproxy_core::{api_keys as core_api_keys, models::Model};
use openproxy_db as db;
use openproxy_types::{AccountId, ApiKeyId, ComboId, CoreError, ProviderId, combos::Combo};
use std::sync::Arc;

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
        let mut accounts = self.repo.list_accounts(provider, master_key)?;
        // Security (audit H-14): `oauth_provider_specific` is decrypted for
        // every reader, and for kiro it carries the dynamically registered
        // OIDC client credentials (`client_secret`). The account LIST feeds
        // the dashboard's routine refresh, where a plaintext secret would
        // sit in browser caches / proxy logs — a disclosure the individual
        // secret-read endpoints guard with `audit_secret_read` and an
        // explicit opt-in. Mask the secret in the listing; internal kiro
        // refresh flows read the decrypted record through the repository,
        // not through this service.
        for account in &mut accounts {
            if let Some(specific) = account.oauth_provider_specific.as_deref()
                && let Ok(mut value) = serde_json::from_str::<serde_json::Value>(specific)
                && value.get("client_secret").and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty())
            {
                if let Some(secret_value) = value.get_mut("client_secret") {
                    *secret_value = serde_json::Value::String("[REDACTED]".to_string());
                }
                account.oauth_provider_specific = Some(value.to_string().into_boxed_str());
            }
        }
        Ok(accounts)
    }

    pub fn create(
        &self,
        master_key: &db::MasterKey,
        input: openproxy_core::admin::CreateAccountInput,
    ) -> Result<AccountId, CoreError> {
        self.repo.create_account(master_key, input)
    }

    pub fn bulk_create(
        &self,
        master_key: &db::MasterKey,
        input: openproxy_core::admin::BulkCreateAccountsInput,
    ) -> Result<Vec<AccountId>, CoreError> {
        self.repo.bulk_create_accounts(master_key, input)
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

    pub fn set_active(&self, id: AccountId, active: bool) -> Result<(), CoreError> {
        self.repo.set_account_active(id, active)
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

    pub fn reorder(
        &self,
        input: openproxy_core::admin::ReorderAccountsInput,
    ) -> Result<(), CoreError> {
        self.repo.reorder_accounts(input)
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

    pub fn compute_effective_context_window(
        &self,
        combo_id: ComboId,
    ) -> Result<Option<i64>, CoreError> {
        self.repo.compute_effective_context_window(combo_id)
    }

    pub fn compute_effective_max_output_tokens(
        &self,
        combo_id: ComboId,
    ) -> Result<Option<i64>, CoreError> {
        self.repo.compute_effective_max_output_tokens(combo_id)
    }

    pub fn compute_effective_capabilities(
        &self,
        combo_id: ComboId,
    ) -> Result<Option<openproxy_types::capabilities::ModelCapabilities>, CoreError> {
        self.repo.compute_effective_capabilities(combo_id)
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
            Arc::clone(&repo) as Arc<dyn Repository>,
            Arc::clone(&repo) as Arc<dyn ApiKeyRepository>,
            Arc::clone(&repo) as Arc<dyn AccountRepository>,
            Arc::clone(&repo) as Arc<dyn ComboRepository>,
            repo as Arc<dyn ModelRepository>,
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

#[cfg(test)]
mod account_list_masking_tests {
    use super::*;
    use crate::repositories::AccountRepository;
    use openproxy_core::accounts::Account;

    struct OneKiroAccount;

    fn kiro_account() -> Account {
        let mut account: Account = serde_json::from_value(serde_json::json!({
            "id": 7,
            "provider_id": "kiro",
            "label": "kiro-main",
            "priority": 1,
            "health_status": "healthy",
            "auth_type": "oauth",
            "email": "ops@example.com",
            "created_at": "2026-01-01T00:00:00Z"
        }))
        .expect("account");
        account.oauth_provider_specific = Some(
            r#"{"client_id":"cid-123","client_secret":"super-secret-value","region":"us-east-1"}"#
                .to_string()
                .into_boxed_str(),
        );
        account
    }

    impl AccountRepository for OneKiroAccount {
        fn list_accounts(
            &self,
            _provider: Option<&ProviderId>,
            _master_key: &db::MasterKey,
        ) -> Result<Vec<Account>, CoreError> {
            Ok(vec![kiro_account()])
        }
        fn create_account(
            &self,
            _master_key: &db::MasterKey,
            _input: openproxy_core::admin::CreateAccountInput,
        ) -> Result<AccountId, CoreError> {
            unimplemented!("not needed for the listing test")
        }
        fn bulk_create_accounts(
            &self,
            _master_key: &db::MasterKey,
            _input: openproxy_core::admin::BulkCreateAccountsInput,
        ) -> Result<Vec<AccountId>, CoreError> {
            unimplemented!("not needed for the listing test")
        }
        fn delete_account(&self, _id: AccountId) -> Result<(), CoreError> {
            unimplemented!("not needed for the listing test")
        }
        fn set_account_health(
            &self,
            _id: AccountId,
            _health: openproxy_core::accounts::HealthStatus,
        ) -> Result<(), CoreError> {
            unimplemented!("not needed for the listing test")
        }
        fn set_account_active(&self, _id: AccountId, _active: bool) -> Result<(), CoreError> {
            unimplemented!("not needed for the listing test")
        }
        fn update_account_api_key(
            &self,
            _master_key: &db::MasterKey,
            _id: AccountId,
            _input: openproxy_core::admin::UpdateAccountApiKeyInput,
        ) -> Result<(), CoreError> {
            unimplemented!("not needed for the listing test")
        }
        fn get_account_api_key(
            &self,
            _master_key: &db::MasterKey,
            _id: AccountId,
        ) -> Result<String, CoreError> {
            unimplemented!("not needed for the listing test")
        }
        fn update_account_label(
            &self,
            _id: AccountId,
            _input: openproxy_core::admin::UpdateAccountLabelInput,
        ) -> Result<(), CoreError> {
            unimplemented!("not needed for the listing test")
        }
        fn reorder_accounts(
            &self,
            _input: openproxy_core::admin::ReorderAccountsInput,
        ) -> Result<(), CoreError> {
            unimplemented!("not needed for the listing test")
        }
    }

    /// Audit H-14: the account listing (dashboard refresh) must not disclose
    /// the kiro OIDC client_secret in plaintext — only the explicit
    /// secret-read endpoints may, behind audit_secret_read.
    #[test]
    fn kiro_client_secret_is_masked_in_the_listing() {
        let service = AccountService::new(std::sync::Arc::new(OneKiroAccount));
        let mk = db::MasterKey::generate().expect("mk");
        let listed = service.list(None, &mk).expect("list");
        assert_eq!(listed.len(), 1);
        let specific = listed[0].oauth_provider_specific.as_deref().expect("specific");
        assert!(
            !specific.contains("super-secret-value"),
            "client_secret must be masked: {specific}"
        );
        assert!(specific.contains("[REDACTED]"), "mask marker: {specific}");
        // Other fields survive.
        assert!(specific.contains("cid-123"), "client_id kept: {specific}");
    }
}
