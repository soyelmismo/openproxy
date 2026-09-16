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
    /// Create an empty registry.
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

    /// Register a new OAuth provider by `Arc`, keyed on the
    /// provider's own `name()`. If a provider with the same name
    /// already exists, it is replaced. This allows custom providers
    /// to override built-in ones at runtime.
    pub fn register_arc(&self, provider: OAuthProviderEnum) {
        let name = provider.name().to_string();
        let mut guard = self.inner.lock();
        guard.insert(name, provider);
    }

    /// Register an OAuth provider `Arc` under an explicit key
    /// (useful for aliases like `antigravity-cli` → same impl as
    /// `antigravity`). If a provider with the same key already
    /// exists, it is replaced.
    pub fn register_arc_with_name(&self, name: &str, provider: OAuthProviderEnum) {
        let mut guard = self.inner.lock();
        guard.insert(name.to_string(), provider);
    }

    /// Register a new OAuth provider by `Box`. Convenience wrapper
    /// around `register_arc`.
    pub fn register(&self, provider: OAuthProviderEnum) {
        self.register_arc(provider);
    }

    /// Look up an OAuth provider by name. Returns `None` if no provider
    /// is registered with that name.
    pub fn get(&self, name: &str) -> Option<OAuthProviderEnum> {
        let guard = self.inner.lock();
        guard.get(name).cloned()
    }
}

impl openproxy_pipeline::oauth::PipelineOAuthRegistry for OAuthProviderRegistry {
    fn refresh_and_store<'a>(
        &'a self,
        provider_id: &'a str,
        refresh_token: &'a str,
        upstream_client: &'a Arc<openproxy_adapters::upstream::UpstreamClient>,
        account_id: AccountId,
        conn: &'a parking_lot::Mutex<rusqlite::Connection>,
        master_key: &'a MasterKey,
    ) -> futures_util::future::BoxFuture<
        'a,
        std::result::Result<openproxy_pipeline::oauth::TokenResponse, CoreError>,
    > {
        use futures_util::FutureExt;
        async move {
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
                    db: DbRef::Connection(conn),
                    master_key,
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
        .boxed()
    }
}
