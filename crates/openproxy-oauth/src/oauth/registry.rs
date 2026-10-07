use super::{DbRef, OAuthProvider, OAuthProviderEnum, OAuthRefreshParams, TokenRefreshCoordinator};
use crate::error::CoreError;
use crate::ids::AccountId;
use openproxy_db::secrets::MasterKey;
use std::sync::Arc;

pub struct OAuthProviderRegistry {
    inner: std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<String, OAuthProviderEnum>>>,
}

impl Default for OAuthProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthProviderRegistry {
    /// Registry with no providers.
    pub fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(parking_lot::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Create a registry pre-populated with the built-in OAuth providers.
    pub fn builtin() -> Self {
        let reg = Self::new();

        for provider in OAuthProviderEnum::builtin_providers() {
            for alias in provider.aliases() {
                reg.register_arc_with_name(alias, OAuthProviderEnum::clone(&provider));
            }

            reg.register_arc(provider);
        }

        reg
    }

    /// Register a provider by `Arc`, keyed on its own `name()`. An existing entry
    /// with the same name is replaced, so a custom provider can override a
    /// built-in at runtime.
    pub fn register_arc(&self, provider: OAuthProviderEnum) {
        let name = provider.name().to_string();
        let mut guard = self.inner.lock();
        guard.insert(name, provider);
    }

    /// Register a provider `Arc` under an explicit key, for aliases such as
    /// `antigravity-cli` pointing at the `antigravity` impl. An existing entry
    /// with the same key is replaced.
    pub fn register_arc_with_name(&self, name: &str, provider: OAuthProviderEnum) {
        let mut guard = self.inner.lock();
        guard.insert(name.to_string(), provider);
    }

    /// [`Self::register_arc`] for a boxed provider.
    pub fn register(&self, provider: OAuthProviderEnum) {
        self.register_arc(provider);
    }

    /// Provider registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<OAuthProviderEnum> {
        let guard = self.inner.lock();
        guard.get(name).cloned()
    }

    async fn refresh_and_store_internal<'a>(
        &'a self,
        provider_id: &'a str,
        refresh_token: &'a str,
        upstream_client: &'a Arc<openproxy_adapters::upstream::UpstreamClient>,
        account_id: AccountId,
        db: DbRef<'a>,
        master_key: &'a MasterKey,
    ) -> std::result::Result<openproxy_pipeline::oauth::TokenResponse, CoreError> {
        let provider = self
            .get(provider_id)
            .ok_or_else(|| CoreError::ProviderNotFound(provider_id.to_string()))?;
        let token = TokenRefreshCoordinator::global()
            .refresh_and_store(OAuthRefreshParams {
                provider_id,
                provider,
                refresh_token,
                upstream_client,
                account_id,
                db,
                master_key,
                force: false,
            })
            .await?;
        Ok(openproxy_pipeline::oauth::TokenResponse {
            access_token: token.access_token,
            token_type: token.token_type,
            expires_in: token.expires_in,
            refresh_token: token.refresh_token,
            scope: token.scope,
            id_token: token.id_token,
        })
    }

    /// Refresh and persist tokens for `account_id` using a shared connection handle.
    pub fn refresh_and_store_shared<'a>(
        &'a self,
        provider_id: &'a str,
        refresh_token: &'a str,
        upstream_client: &'a Arc<openproxy_adapters::upstream::UpstreamClient>,
        account_id: AccountId,
        conn: &'a Arc<parking_lot::Mutex<rusqlite::Connection>>,
        master_key: &'a MasterKey,
    ) -> futures_util::future::BoxFuture<
        'a,
        std::result::Result<openproxy_pipeline::oauth::TokenResponse, CoreError>,
    > {
        use futures_util::FutureExt;
        async move {
            self.refresh_and_store_internal(
                provider_id,
                refresh_token,
                upstream_client,
                account_id,
                DbRef::Shared(conn),
                master_key,
            )
            .await
        }
        .boxed()
    }
}

impl openproxy_pipeline::oauth::PipelineOAuthRegistry for OAuthProviderRegistry {
    fn refresh_and_store<'a>(
        &'a self,
        provider_id: &'a str,
        refresh_token: &'a str,
        upstream_client: &'a Arc<openproxy_adapters::upstream::UpstreamClient>,
        account_id: AccountId,
        db_pool: Option<&'a openproxy_db::DbPool>,
        master_key: &'a MasterKey,
    ) -> futures_util::future::BoxFuture<
        'a,
        std::result::Result<openproxy_pipeline::oauth::TokenResponse, CoreError>,
    > {
        use futures_util::FutureExt;
        async move {
            let Some(pool) = db_pool else {
                return Err(CoreError::Internal(
                    "oauth refresh requires a database pool".to_string(),
                ));
            };
            self.refresh_and_store_internal(
                provider_id,
                refresh_token,
                upstream_client,
                account_id,
                DbRef::Pool(pool),
                master_key,
            )
            .await
        }
        .boxed()
    }

    fn refresh_and_store_shared<'a>(
        &'a self,
        provider_id: &'a str,
        refresh_token: &'a str,
        upstream_client: &'a Arc<openproxy_adapters::upstream::UpstreamClient>,
        account_id: AccountId,
        conn: &'a Arc<parking_lot::Mutex<rusqlite::Connection>>,
        master_key: &'a MasterKey,
    ) -> futures_util::future::BoxFuture<
        'a,
        std::result::Result<openproxy_pipeline::oauth::TokenResponse, CoreError>,
    > {
        self.refresh_and_store_shared(
            provider_id,
            refresh_token,
            upstream_client,
            account_id,
            conn,
            master_key,
        )
    }
}
