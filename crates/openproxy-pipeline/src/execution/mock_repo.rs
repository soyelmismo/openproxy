use super::*;
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::error::{CoreError, Result};
use openproxy_types::ids::*;
use std::sync::Arc;

pub(crate) struct MockFailingComboRepo {
    pub(crate) inner: Arc<dyn crate::repository::PipelineRepository>,
    pub(crate) record_cooldown_called: Arc<std::sync::atomic::AtomicBool>,
}

impl crate::repository::PipelineRepository for MockFailingComboRepo {
    fn load_combo(&self, _combo_id: ComboId) -> Result<Option<Combo>> {
        Err(CoreError::Internal(
            "simulated database failure loading combo".into(),
        ))
    }

    fn record_cooldown(
        &self,
        target_id: ComboTargetId,
        reason: &str,
        mode: openproxy_types::config::CooldownMode,
        base_secs: u64,
        max_secs: u64,
        factor: u32,
    ) -> Result<()> {
        self.record_cooldown_called
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner
            .record_cooldown(target_id, reason, mode, base_secs, max_secs, factor)
    }

    fn list_targets(&self, combo_id: ComboId) -> Result<Vec<ComboTarget>> {
        self.inner.list_targets(combo_id)
    }

    fn auto_populate_empty_combo(&self, combo_id: ComboId) -> Result<usize> {
        self.inner.auto_populate_empty_combo(combo_id)
    }

    fn get_account(
        &self,
        account_id: AccountId,
        master_key: &openproxy_db::secrets::MasterKey,
    ) -> Result<Option<openproxy_types::accounts::Account>> {
        self.inner.get_account(account_id, master_key)
    }

    fn decrypt_account_key(
        &self,
        account_id: AccountId,
        master_key: &openproxy_db::secrets::MasterKey,
    ) -> Result<String> {
        self.inner.decrypt_account_key(account_id, master_key)
    }

    fn decrypt_access_token(
        &self,
        account_id: AccountId,
        master_key: &openproxy_db::secrets::MasterKey,
    ) -> Result<String> {
        self.inner.decrypt_access_token(account_id, master_key)
    }

    fn store_oauth_tokens(
        &self,
        account_id: AccountId,
        master_key: &openproxy_db::secrets::MasterKey,
        params: openproxy_types::accounts::StoreOAuthTokensParams<'_>,
    ) -> Result<()> {
        self.inner
            .store_oauth_tokens(account_id, master_key, params)
    }

    fn insert_and_broadcast_notification(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        dedup_key: Option<&str>,
        provider_id: Option<&str>,
    ) -> Result<()> {
        self.inner
            .insert_and_broadcast_notification(kind, payload, dedup_key, provider_id)
    }

    fn load_model(&self, row_id: ModelRowId) -> Result<openproxy_types::models::Model> {
        self.inner.load_model(row_id)
    }

    fn get_account_label(
        &self,
        account_id: AccountId,
        master_key: &openproxy_db::secrets::MasterKey,
    ) -> Result<Option<String>> {
        self.inner.get_account_label(account_id, master_key)
    }

    fn record_usage_row(
        &self,
        input: &openproxy_types::usage::UsageInput,
    ) -> Result<Option<UsageId>> {
        self.inner.record_usage_row(input)
    }

    fn mark_client_response(&self, row_id: UsageId) -> Result<()> {
        self.inner.mark_client_response(row_id)
    }

    fn mark_winner_usage_row(
        &self,
        request_id: &str,
        attempt: u8,
        target_id: ComboTargetId,
    ) -> Result<()> {
        self.inner
            .mark_winner_usage_row(request_id, attempt, target_id)
    }

    fn record_no_healthy_targets_row(
        &self,
        request_id: &str,
        trace_id: &str,
        combo: &Combo,
        elapsed: u64,
        created_str: &str,
        error_msg: &str,
    ) -> Result<()> {
        self.inner.record_no_healthy_targets_row(
            request_id,
            trace_id,
            combo,
            elapsed,
            created_str,
            error_msg,
        )
    }

    fn clear_cooldown(&self, target_id: ComboTargetId) -> Result<()> {
        self.inner.clear_cooldown(target_id)
    }

    fn prune_expired_cooldowns(&self) -> Result<usize> {
        self.inner.prune_expired_cooldowns()
    }

    fn get_active_cooldown_targets(
        &self,
        combo_id: ComboId,
    ) -> Result<std::collections::HashSet<ComboTargetId>> {
        self.inner.get_active_cooldown_targets(combo_id)
    }

    fn update_proxy_status(
        &self,
        proxy_id: &str,
        status: &str,
        error_msg: Option<&str>,
    ) -> Result<()> {
        self.inner.update_proxy_status(proxy_id, status, error_msg)
    }

    fn get_or_assign_provider_proxy(
        &self,
        provider_id: &ProviderId,
        account_id: Option<AccountId>,
    ) -> Result<Option<String>> {
        self.inner
            .get_or_assign_provider_proxy(provider_id, account_id)
    }

    fn get_candidate_proxies(
        &self,
        provider_id: &ProviderId,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        self.inner.get_candidate_proxies(provider_id, limit)
    }

    fn get_proxy_status_by_url(&self, url: &str) -> Option<String> {
        self.inner.get_proxy_status_by_url(url)
    }

    fn get_models_by_row_ids(
        &self,
        model_row_ids: &[ModelRowId],
    ) -> Result<std::collections::HashMap<i64, openproxy_types::models::Model>> {
        self.inner.get_models_by_row_ids(model_row_ids)
    }

    fn get_accounts_meta(
        &self,
        account_ids: &[AccountId],
    ) -> Result<crate::repository::AccountsMetaMaps> {
        self.inner.get_accounts_meta(account_ids)
    }

    fn get_antigravity_projects(
        &self,
        account_ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, Box<str>>> {
        self.inner.get_antigravity_projects(account_ids)
    }

    fn get_providers_auth_type(
        &self,
        provider_ids: &[ProviderId],
    ) -> Result<std::collections::HashMap<String, String>> {
        self.inner.get_providers_auth_type(provider_ids)
    }

    fn update_antigravity_project_id(&self, account_id: i64, new_project_id: &str) -> Result<()> {
        self.inner
            .update_antigravity_project_id(account_id, new_project_id)
    }

    fn resolve_combo_to_targets(
        &self,
        combo_id: ComboId,
        visited: &mut Vec<ComboId>,
        depth: u32,
    ) -> Result<Vec<ComboTarget>> {
        self.inner
            .resolve_combo_to_targets(combo_id, visited, depth)
    }

    fn expand_account_rotation(&self, targets: Vec<ComboTarget>) -> Result<Vec<ComboTarget>> {
        self.inner.expand_account_rotation(targets)
    }

    fn resolve_target_order_with_mode(
        &self,
        combo: &Combo,
        rr_counters: &std::sync::Arc<
            dashmap::DashMap<openproxy_types::ids::ComboId, std::sync::atomic::AtomicU64>,
        >,
        selection_registry: &openproxy_types::SelectionRegistry,
    ) -> Result<Vec<ComboTarget>> {
        self.inner
            .resolve_target_order_with_mode(combo, rr_counters, selection_registry)
    }

    fn decrypt_api_key_and_label(
        &self,
        id: AccountId,
        master_key: &openproxy_db::secrets::MasterKey,
    ) -> Result<(String, Option<String>)> {
        self.inner.decrypt_api_key_and_label(id, master_key)
    }

    fn get_provider(
        &self,
        provider_id: &ProviderId,
    ) -> Result<Option<openproxy_types::providers::Provider>> {
        self.inner.get_provider(provider_id)
    }
}
