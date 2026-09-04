use openproxy_db::secrets::MasterKey;
use openproxy_types::SelectionRegistry;
use openproxy_types::{
    Account, AccountId, Combo, ComboId, ComboTarget, ComboTargetId, CooldownMode, Model,
    ModelRowId, ProviderId, Result, UsageId, UsageInput,
};
use std::collections::HashMap;

pub use openproxy_db::accounts::{AccountsMetaMaps, KiroMeta, RawAccount};

pub use PipelineRepository as Repository;

pub trait PipelineRepository: Send + Sync {
    fn load_combo(&self, combo_id: ComboId) -> Result<Option<Combo>>;
    fn list_targets(&self, combo_id: ComboId) -> Result<Vec<ComboTarget>>;
    fn auto_populate_empty_combo(&self, combo_id: ComboId) -> Result<usize>;
    fn get_account(&self, account_id: AccountId, master_key: &MasterKey)
    -> Result<Option<Account>>;
    fn decrypt_account_key(&self, account_id: AccountId, master_key: &MasterKey) -> Result<String>;
    fn decrypt_access_token(&self, account_id: AccountId, master_key: &MasterKey)
    -> Result<String>;
    fn store_oauth_tokens(
        &self,
        account_id: AccountId,
        master_key: &MasterKey,
        params: openproxy_types::accounts::StoreOAuthTokensParams<'_>,
    ) -> Result<()>;
    fn insert_and_broadcast_notification(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        dedup_key: Option<&str>,
        provider_id: Option<&str>,
    ) -> Result<()>;
    fn load_model(&self, row_id: ModelRowId) -> Result<Model>;
    fn get_account_label(
        &self,
        account_id: AccountId,
        master_key: &MasterKey,
    ) -> Result<Option<String>>;
    fn record_usage_row(&self, input: &UsageInput) -> Result<Option<UsageId>>;
    fn mark_client_response(&self, row_id: UsageId) -> Result<()>;
    fn mark_winner_usage_row(
        &self,
        request_id: &str,
        attempt: u8,
        target_id: ComboTargetId,
    ) -> Result<()>;
    fn record_no_healthy_targets_row(
        &self,
        request_id: &str,
        trace_id: &str,
        combo: &Combo,
        elapsed: u64,
        created_str: &str,
        error_msg: &str,
    ) -> Result<()>;
    fn clear_cooldown(&self, target_id: ComboTargetId) -> Result<()>;
    fn prune_expired_cooldowns(&self) -> Result<usize>;
    fn record_cooldown(
        &self,
        target_id: ComboTargetId,
        reason: &str,
        mode: CooldownMode,
        base_secs: u64,
        max_secs: u64,
        factor: u32,
    ) -> Result<()>;

    fn update_proxy_status(
        &self,
        proxy_id: &str,
        status: &str,
        error_msg: Option<&str>,
    ) -> Result<()>;
    fn get_or_assign_provider_proxy(
        &self,
        provider_id: &ProviderId,
        account_id: Option<AccountId>,
    ) -> Result<Option<String>>;
    fn get_candidate_proxies(
        &self,
        provider_id: &ProviderId,
        limit: usize,
    ) -> Result<Vec<(String, String)>>;
    fn get_proxy_status_by_url(&self, url: &str) -> Option<String>;

    // Batch Loading
    fn get_models_by_row_ids(&self, model_row_ids: &[ModelRowId]) -> Result<HashMap<i64, Model>>;
    fn get_accounts_meta(&self, account_ids: &[AccountId]) -> Result<AccountsMetaMaps>;
    fn get_antigravity_projects(&self, account_ids: &[i64]) -> Result<HashMap<i64, Box<str>>>;
    fn get_providers_auth_type(
        &self,
        provider_ids: &[ProviderId],
    ) -> Result<HashMap<String, String>>;
    fn update_antigravity_project_id(&self, account_id: i64, new_project_id: &str) -> Result<()>;

    // Routing Logic
    fn resolve_combo_to_targets(
        &self,
        combo_id: ComboId,
        visited: &mut Vec<ComboId>,
        depth: u32,
    ) -> Result<Vec<ComboTarget>>;
    fn expand_account_rotation(&self, targets: Vec<ComboTarget>) -> Result<Vec<ComboTarget>>;
    fn resolve_target_order_with_mode(
        &self,
        combo: &Combo,
        rr_counters: &std::sync::Arc<
            dashmap::DashMap<openproxy_types::ids::ComboId, std::sync::atomic::AtomicU64>,
        >,
        selection_registry: &SelectionRegistry,
    ) -> Result<Vec<ComboTarget>>;
    fn decrypt_api_key_and_label(
        &self,
        id: AccountId,
        master_key: &MasterKey,
    ) -> Result<(String, Option<String>)>;
    fn get_provider(
        &self,
        provider_id: &ProviderId,
    ) -> Result<Option<openproxy_types::providers::Provider>>;
}

#[derive(Clone)]
pub struct SqlitePipelineRepository {
    conn: std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>,
}

impl SqlitePipelineRepository {
    pub fn new(conn: std::sync::Arc<parking_lot::Mutex<rusqlite::Connection>>) -> Self {
        Self { conn }
    }
}

impl PipelineRepository for SqlitePipelineRepository {
    fn load_combo(&self, combo_id: ComboId) -> Result<Option<Combo>> {
        openproxy_db::combos::get_combo(&self.conn.lock(), combo_id)
    }

    fn list_targets(&self, combo_id: ComboId) -> Result<Vec<ComboTarget>> {
        openproxy_db::combos::list_targets(&self.conn.lock(), combo_id)
    }

    fn auto_populate_empty_combo(&self, combo_id: ComboId) -> Result<usize> {
        auto_populate_empty_combo(&self.conn.lock(), combo_id)
    }

    fn get_account(
        &self,
        account_id: AccountId,
        master_key: &MasterKey,
    ) -> Result<Option<Account>> {
        openproxy_db::accounts::get(&self.conn.lock(), account_id, master_key)
    }

    fn decrypt_account_key(&self, account_id: AccountId, master_key: &MasterKey) -> Result<String> {
        openproxy_db::accounts::decrypt_api_key(&self.conn.lock(), account_id, master_key)
    }

    fn decrypt_access_token(
        &self,
        account_id: AccountId,
        master_key: &MasterKey,
    ) -> Result<String> {
        openproxy_db::accounts::decrypt_access_token(&self.conn.lock(), account_id, master_key)
    }

    fn store_oauth_tokens(
        &self,
        account_id: AccountId,
        master_key: &MasterKey,
        params: openproxy_types::accounts::StoreOAuthTokensParams<'_>,
    ) -> Result<()> {
        openproxy_db::accounts::store_oauth_tokens(
            &self.conn.lock(),
            account_id,
            master_key,
            params,
        )
    }

    fn insert_and_broadcast_notification(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        dedup_key: Option<&str>,
        provider_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        if let Some(id) =
            openproxy_db::notifications::insert(&conn, kind, payload, dedup_key, provider_id)?
        {
            let created_at = openproxy_db::notifications::get_created_at(&conn, id)?
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
            drop(conn);
            openproxy_types::notifications::publish_notification(
                openproxy_types::notifications::NotificationEvent {
                    id,
                    kind: kind.to_string(),
                    payload: payload.to_owned(),
                    created_at,
                },
            );
        }
        Ok(())
    }

    fn load_model(&self, row_id: ModelRowId) -> Result<Model> {
        openproxy_db::models::get_by_row_id(&self.conn.lock(), row_id)?.ok_or_else(|| {
            openproxy_types::error::CoreError::Internal(format!("model {} not found", row_id.0))
        })
    }

    fn get_account_label(
        &self,
        account_id: AccountId,
        master_key: &MasterKey,
    ) -> Result<Option<String>> {
        openproxy_db::accounts::get(&self.conn.lock(), account_id, master_key)
            .map(|opt| opt.and_then(|a| a.label.map(|l| l.to_string())))
    }

    fn record_usage_row(&self, input: &UsageInput) -> Result<Option<UsageId>> {
        openproxy_db::cost::record(&self.conn.lock(), input).map(Some)
    }

    fn mark_client_response(&self, row_id: UsageId) -> Result<()> {
        openproxy_db::cost::mark_client_response(&self.conn.lock(), row_id)
    }

    fn mark_winner_usage_row(
        &self,
        request_id: &str,
        attempt: u8,
        target_id: ComboTargetId,
    ) -> Result<()> {
        openproxy_db::cost::mark_winner_usage_row(&self.conn.lock(), request_id, attempt, target_id)
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
        openproxy_db::cost::record_no_healthy_targets_row(
            &self.conn.lock(),
            request_id,
            trace_id,
            combo.id,
            elapsed,
            created_str,
            error_msg,
        )
    }

    fn clear_cooldown(&self, target_id: ComboTargetId) -> Result<()> {
        openproxy_db::cooldowns::clear_cooldown(&self.conn.lock(), target_id)
    }

    fn record_cooldown(
        &self,
        target_id: ComboTargetId,
        reason: &str,
        mode: CooldownMode,
        base_secs: u64,
        max_secs: u64,
        factor: u32,
    ) -> Result<()> {
        openproxy_db::cooldowns::record_cooldown(
            &self.conn.lock(),
            target_id,
            reason,
            mode,
            base_secs,
            max_secs,
            factor,
        )
    }

    fn get_models_by_row_ids(&self, model_row_ids: &[ModelRowId]) -> Result<HashMap<i64, Model>> {
        let models = openproxy_db::models::get_by_row_ids(&self.conn.lock(), model_row_ids)?;
        let mut map = HashMap::new();
        for m in models {
            map.insert(m.row_id.0, m);
        }
        Ok(map)
    }

    fn get_accounts_meta(&self, account_ids: &[AccountId]) -> Result<AccountsMetaMaps> {
        openproxy_db::accounts::get_accounts_meta(&self.conn.lock(), account_ids)
    }

    fn get_antigravity_projects(&self, account_ids: &[i64]) -> Result<HashMap<i64, Box<str>>> {
        let conn = self.conn.lock();
        let map: HashMap<i64, openproxy_db::accounts::AntigravityMeta> =
            openproxy_db::accounts::read_provider_meta_batch(&conn, None, account_ids)?;
        Ok(map
            .into_iter()
            .filter_map(|(id, meta)| meta.project_id.map(|s| (id, s.into_boxed_str())))
            .collect())
    }

    fn update_antigravity_project_id(&self, account_id: i64, new_project_id: &str) -> Result<()> {
        openproxy_db::accounts::update_antigravity_project_id(
            &self.conn.lock(),
            account_id,
            new_project_id,
        )
    }

    fn get_providers_auth_type(
        &self,
        provider_ids: &[ProviderId],
    ) -> Result<HashMap<String, String>> {
        openproxy_db::providers::get_auth_types(&self.conn.lock(), provider_ids)
    }

    fn resolve_combo_to_targets(
        &self,
        combo_id: ComboId,
        visited: &mut Vec<ComboId>,
        depth: u32,
    ) -> Result<Vec<ComboTarget>> {
        resolve_combo_to_targets(&self.conn.lock(), combo_id, visited, depth)
    }
    fn expand_account_rotation(&self, targets: Vec<ComboTarget>) -> Result<Vec<ComboTarget>> {
        expand_account_rotation(&self.conn.lock(), targets)
    }
    fn resolve_target_order_with_mode(
        &self,
        combo: &Combo,
        rr_counters: &std::sync::Arc<
            dashmap::DashMap<openproxy_types::ids::ComboId, std::sync::atomic::AtomicU64>,
        >,
        selection_registry: &SelectionRegistry,
    ) -> Result<Vec<ComboTarget>> {
        let targets = self.list_targets(combo.id)?;
        Ok(crate::load_balancing::execute_load_balancing(
            targets,
            combo,
            rr_counters,
            selection_registry,
        ))
    }

    fn decrypt_api_key_and_label(
        &self,
        id: AccountId,
        master_key: &MasterKey,
    ) -> Result<(String, Option<String>)> {
        let key = self.decrypt_account_key(id, master_key)?;
        let label = self.get_account_label(id, master_key)?;
        Ok((key, label))
    }

    fn get_provider(
        &self,
        provider_id: &ProviderId,
    ) -> Result<Option<openproxy_types::providers::Provider>> {
        openproxy_db::providers::get(&self.conn.lock(), provider_id)
    }

    fn update_proxy_status(
        &self,
        proxy_id: &str,
        status: &str,
        _error_msg: Option<&str>,
    ) -> Result<()> {
        openproxy_db::free_proxies::update_proxy_status(&self.conn.lock(), proxy_id, status, None)
    }

    fn get_or_assign_provider_proxy(
        &self,
        provider_id: &ProviderId,
        account_id: Option<AccountId>,
    ) -> Result<Option<String>> {
        openproxy_db::free_proxies::get_or_assign_provider_proxy(
            &self.conn.lock(),
            provider_id,
            account_id.as_ref(),
        )
    }

    fn get_candidate_proxies(
        &self,
        provider_id: &ProviderId,
        limit: usize,
    ) -> Result<Vec<(String, String)>> {
        openproxy_db::free_proxies::get_candidate_proxies_for_provider(
            &self.conn.lock(),
            provider_id,
            limit,
        )
    }

    fn get_proxy_status_by_url(&self, url: &str) -> Option<String> {
        openproxy_db::free_proxies::get_proxy_status_by_url(&self.conn.lock(), url)
    }

    fn prune_expired_cooldowns(&self) -> Result<usize> {
        openproxy_db::cooldowns::prune_expired(&self.conn.lock())
    }
}

pub fn list_targets(conn: &rusqlite::Connection, combo_id: ComboId) -> Result<Vec<ComboTarget>> {
    openproxy_db::combos::list_targets(conn, combo_id)
}

pub fn auto_populate_empty_combo(
    _conn: &rusqlite::Connection,
    _combo_id: ComboId,
) -> Result<usize> {
    Ok(0)
}

pub fn expand_account_rotation(
    conn: &rusqlite::Connection,
    targets: Vec<ComboTarget>,
) -> Result<Vec<ComboTarget>> {
    openproxy_db::combos::expand_account_rotation(conn, targets)
}

pub fn resolve_combo_to_targets(
    conn: &rusqlite::Connection,
    combo_id: ComboId,
    visited: &mut Vec<ComboId>,
    depth: u32,
) -> Result<Vec<ComboTarget>> {
    openproxy_db::combos::resolve_combo_to_targets(conn, combo_id, visited, depth)
}

pub fn prune_expired_cooldowns(conn: &rusqlite::Connection) -> Result<usize> {
    openproxy_db::cooldowns::prune_expired(conn)
}
