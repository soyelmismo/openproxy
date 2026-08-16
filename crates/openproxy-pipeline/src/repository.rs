use openproxy_db::secrets::MasterKey;
use openproxy_types::SelectionRegistry;
use openproxy_types::{
    Account, AccountId, Combo, ComboId, ComboTarget, ComboTargetId, CooldownMode, Model,
    ModelRowId, ProviderId, Result, UsageId, UsageInput,
};
use std::collections::HashMap;

pub struct RawAccount {
    pub api_key_encrypted: Option<Vec<u8>>,
    pub label: Option<String>,
    pub access_token_encrypted: Option<Vec<u8>>,
    pub refresh_token_encrypted: Option<Vec<u8>>,
    pub expires_at: Option<String>,
    pub oauth_provider_specific: Option<String>,
    pub quota_session_reset_at: Option<String>,
    pub quota_model_details: Option<String>,
}

pub struct KiroMeta {
    pub region: Option<String>,
    pub profile_arn: Option<String>,
}

pub type AccountsMetaMaps = (
    HashMap<i64, RawAccount>,
    HashMap<i64, KiroMeta>,
    HashMap<i64, String>,
);

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
    fn get_proxy_status_by_url(&self, url: &str) -> Option<String>;

    // Batch Loading
    fn get_models_by_row_ids(&self, model_row_ids: &[ModelRowId]) -> Result<HashMap<i64, Model>>;
    fn get_accounts_meta(&self, account_ids: &[AccountId]) -> Result<AccountsMetaMaps>;
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
        rr_counters: &std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<ComboId, u64>>>,
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
        let conn = self.conn.lock();
        openproxy_db::combos::get_combo(&conn, combo_id)
    }

    fn list_targets(&self, combo_id: ComboId) -> Result<Vec<ComboTarget>> {
        let conn = self.conn.lock();
        openproxy_db::combos::list_targets(&conn, combo_id)
    }

    fn auto_populate_empty_combo(&self, combo_id: ComboId) -> Result<usize> {
        auto_populate_empty_combo(&self.conn.lock(), combo_id)
    }

    fn get_account(
        &self,
        account_id: AccountId,
        master_key: &MasterKey,
    ) -> Result<Option<Account>> {
        let conn = self.conn.lock();
        openproxy_db::accounts::get(&conn, account_id, master_key)
    }

    fn decrypt_account_key(&self, account_id: AccountId, master_key: &MasterKey) -> Result<String> {
        let conn = self.conn.lock();
        openproxy_db::accounts::decrypt_api_key(&conn, account_id, master_key)
    }

    fn decrypt_access_token(
        &self,
        account_id: AccountId,
        master_key: &MasterKey,
    ) -> Result<String> {
        let conn = self.conn.lock();
        openproxy_db::accounts::decrypt_access_token(&conn, account_id, master_key)
    }

    fn store_oauth_tokens(
        &self,
        account_id: AccountId,
        master_key: &MasterKey,
        params: openproxy_types::accounts::StoreOAuthTokensParams<'_>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        openproxy_db::accounts::store_oauth_tokens(&conn, account_id, master_key, params)
    }

    fn insert_and_broadcast_notification(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        dedup_key: Option<&str>,
        provider_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        let payload_str = serde_json::to_string(payload).map_err(|e| {
            openproxy_types::error::CoreError::Database {
                message: "serialize notification payload".into(),
                source: Some(Box::new(e)),
            }
        })?;
        conn.execute(
            "INSERT INTO notifications(kind, payload, dedup_key, provider_id) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![kind, payload_str, dedup_key, provider_id]
        ).map_err(|e| openproxy_types::error::CoreError::Database { message: "insert notification".into(), source: Some(Box::new(e)) })?;

        let id: i64 = conn.last_insert_rowid();
        let created_at: String = conn
            .query_row(
                "SELECT created_at FROM notifications WHERE id = ?1",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .unwrap_or_else(|_| chrono::Utc::now().to_rfc3339());

        openproxy_types::notifications::publish_notification(
            openproxy_types::notifications::NotificationEvent {
                id,
                kind: kind.to_string(),
                payload: payload.to_owned(),
                created_at,
            },
        );
        Ok(())
    }

    fn load_model(&self, row_id: ModelRowId) -> Result<Model> {
        let conn = self.conn.lock();
        openproxy_db::models::get_by_row_id(&conn, row_id)?
            .ok_or_else(|| openproxy_types::error::CoreError::Internal(format!("model {} not found", row_id.0)))
    }

    fn get_account_label(
        &self,
        account_id: AccountId,
        master_key: &MasterKey,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock();
        openproxy_db::accounts::get(&conn, account_id, master_key)
            .map(|opt| opt.and_then(|a| a.label))
    }

    fn record_usage_row(&self, input: &UsageInput) -> Result<Option<UsageId>> {
        let conn = self.conn.lock();
        let res = openproxy_db::cost::record(&conn, input);
        match res {
            Ok(id) => Ok(Some(id)),
            Err(e) => Err(openproxy_types::error::CoreError::Internal(e.to_string())),
        }
    }

    fn mark_client_response(&self, row_id: UsageId) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE usage SET client_responded = 1 WHERE id = ?1",
            rusqlite::params![row_id.0],
        )
        .map(|_| ())
        .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))
    }

    fn mark_winner_usage_row(
        &self,
        request_id: &str,
        attempt: u8,
        target_id: ComboTargetId,
    ) -> Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE usage SET was_winner = 1, client_response = 1 WHERE request_id = ?1 AND attempt = ?2 AND combo_target_id = ?3",
            rusqlite::params![request_id, attempt, target_id.0]
        ).map(|_| ()).map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))
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
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO usage(request_id, trace_id, combo_id, total_ms, created_at, status_code, error_msg, error_message, was_winner, client_response, prompt_tokens, completion_tokens, provider_id, upstream_model_id, attempt, race_total, race_lost) \
             VALUES (?1, ?2, ?3, ?4, ?5, 502, ?6, ?6, 1, 0, 0, 0, 'virtual', 'none', 1, 1, 0)",
            rusqlite::params![request_id, trace_id, combo.id.0, elapsed as i64, created_str, error_msg]
        ).map_err(|e| openproxy_types::error::CoreError::Database { message: "insert no_healthy_targets usage".into(), source: Some(Box::new(e)) })?;
        Ok(())
    }

    fn clear_cooldown(&self, target_id: ComboTargetId) -> Result<()> {
        let conn = self.conn.lock();
        openproxy_db::cooldowns::clear_cooldown(&conn, target_id)
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
        if mode == CooldownMode::None || base_secs == 0 {
            return Ok(());
        }

        let conn = self.conn.lock();
        let current_count: u32 = conn
            .query_row(
                "SELECT failure_count FROM target_cooldowns WHERE combo_target_id = ?1",
                rusqlite::params![target_id.0],
                |row| row.get(0),
            )
            .unwrap_or(0);

        let new_count = current_count + 1;

        let cooldown_secs = match mode {
            CooldownMode::None => return Ok(()),
            CooldownMode::Flat => base_secs,
            CooldownMode::Exponential => {
                let mut exp_secs =
                    base_secs.saturating_mul(u64::from(factor).saturating_pow(current_count));
                if exp_secs > max_secs {
                    exp_secs = max_secs;
                }
                exp_secs
            }
        };

        let cooldown_until = chrono::Utc::now() + chrono::Duration::seconds(cooldown_secs as i64);
        let cooldown_until_str = cooldown_until.to_rfc3339();
        conn.execute(
            "INSERT INTO target_cooldowns (combo_target_id, cooldown_until, reason, failure_count, updated_at) \
             VALUES (?1, ?2, ?3, ?4, datetime('now')) \
             ON CONFLICT(combo_target_id) DO UPDATE SET \
                 cooldown_until = excluded.cooldown_until, \
                 reason = excluded.reason, \
                 failure_count = excluded.failure_count, \
                 updated_at = excluded.updated_at",
            rusqlite::params![target_id.0, cooldown_until_str, reason, new_count]
        ).map(|_| ()).map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))
    }

    fn get_models_by_row_ids(&self, model_row_ids: &[ModelRowId]) -> Result<HashMap<i64, Model>> {
        let conn = self.conn.lock();
        let models = openproxy_db::models::get_by_row_ids(&conn, model_row_ids)?;
        let mut map = HashMap::new();
        for m in models {
            map.insert(m.row_id.0, m);
        }
        Ok(map)
    }

    fn get_accounts_meta(&self, account_ids: &[AccountId]) -> Result<AccountsMetaMaps> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock();
        let mut raw_map = HashMap::new();
        let mut kiro_map = HashMap::new();
        let mut ag_map = HashMap::new();
        for id in account_ids {
            let row = conn.query_row(
                "SELECT api_key_encrypted, label, access_token_encrypted, refresh_token_encrypted, expires_at, oauth_provider_specific, email, extra_config_json FROM accounts WHERE id = ?1",
                rusqlite::params![id.0],
                |r| {
                    Ok((
                        r.get::<_, Option<Vec<u8>>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                        r.get::<_, Option<Vec<u8>>>(2)?,
                        r.get::<_, Option<Vec<u8>>>(3)?,
                        r.get::<_, Option<String>>(4)?,
                        r.get::<_, Option<String>>(5)?,
                        r.get::<_, Option<String>>(6)?,
                        r.get::<_, Option<String>>(7)?,
                    ))
                }
            ).optional().map_err(|e| openproxy_types::error::CoreError::Database { message: "query accounts".into(), source: Some(Box::new(e)) })?;
            if let Some((
                api_key,
                label,
                access,
                refresh,
                expires,
                oauth_prov,
                _email,
                extra_json,
            )) = row
            {
                // Extract projectId from oauth_provider_specific JSON for antigravity accounts.
                // Do NOT use the email column — the API needs a real GCP project ID.
                if let Some(ref oauth_json) = oauth_prov
                    && let Ok(meta) = serde_json::from_str::<serde_json::Value>(oauth_json)
                    && let Some(pid) = meta
                        .get("projectId")
                        .or_else(|| meta.get("project_id"))
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|v| !v.is_empty())
                {
                    ag_map.insert(id.0, pid.to_string());
                }

                raw_map.insert(
                    id.0,
                    RawAccount {
                        api_key_encrypted: api_key,
                        label,
                        access_token_encrypted: access,
                        refresh_token_encrypted: refresh,
                        expires_at: expires,
                        oauth_provider_specific: oauth_prov,
                        quota_session_reset_at: None,
                        quota_model_details: None,
                    },
                );

                if let Some(cfg_str) = extra_json
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(&cfg_str)
                {
                    let region = val
                        .get("region")
                        .or(val.get("aws_region"))
                        .and_then(|v| v.as_str())
                        .map(std::string::ToString::to_string);
                    let profile_arn = val
                        .get("profile_arn")
                        .or(val.get("aws_role_arn"))
                        .and_then(|v| v.as_str())
                        .map(std::string::ToString::to_string);
                    if region.is_some() || profile_arn.is_some() {
                        kiro_map.insert(
                            id.0,
                            KiroMeta {
                                region,
                                profile_arn,
                            },
                        );
                    }
                }
            } else {
                return Err(openproxy_types::error::CoreError::Validation(format!(
                    "account {} not found",
                    id.0
                )));
            }
        }
        Ok((raw_map, kiro_map, ag_map))
    }

    fn update_antigravity_project_id(&self, account_id: i64, new_project_id: &str) -> Result<()> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock();

        let current_json_opt: Option<String> = conn
            .query_row(
                "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
                rusqlite::params![account_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| openproxy_types::error::CoreError::Database {
                message: "query account".into(),
                source: Some(Box::new(e)),
            })?
            .flatten();

        let mut meta = if let Some(json_str) = current_json_opt {
            serde_json::from_str::<serde_json::Value>(&json_str)
                .unwrap_or_else(|_| serde_json::json!({}))
        } else {
            serde_json::json!({})
        };

        if let Some(obj) = meta.as_object_mut() {
            obj.insert(
                "projectId".to_string(),
                serde_json::Value::String(new_project_id.to_string()),
            );
        }

        let new_json_str = serde_json::to_string(&meta).unwrap_or_else(|_| "{}".to_string());

        conn.execute(
            "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
            rusqlite::params![new_json_str, account_id],
        )
        .map_err(|e| openproxy_types::error::CoreError::Database {
            message: "update account".into(),
            source: Some(Box::new(e)),
        })?;

        Ok(())
    }

    fn get_providers_auth_type(
        &self,
        provider_ids: &[ProviderId],
    ) -> Result<HashMap<String, String>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock();
        let mut map = HashMap::new();
        for id in provider_ids {
            let auth: Option<String> = conn
                .query_row(
                    "SELECT auth_type FROM providers WHERE id = ?1",
                    rusqlite::params![id.as_str()],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| openproxy_types::error::CoreError::Database {
                    message: "query providers".into(),
                    source: Some(Box::new(e)),
                })?;
            if let Some(a) = auth {
                map.insert(id.as_str().to_string(), a);
            }
        }
        Ok(map)
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
        rr_counters: &std::sync::Arc<parking_lot::Mutex<std::collections::HashMap<ComboId, u64>>>,
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
        let conn = self.conn.lock();
        openproxy_db::providers::get(&conn, provider_id)
    }

    fn update_proxy_status(
        &self,
        proxy_id: &str,
        status: &str,
        _error_msg: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        openproxy_db::free_proxies::update_proxy_status(&conn, proxy_id, status, None)
    }

    fn get_or_assign_provider_proxy(
        &self,
        provider_id: &ProviderId,
        account_id: Option<AccountId>,
    ) -> Result<Option<String>> {
        let conn = self.conn.lock();
        openproxy_db::free_proxies::get_or_assign_provider_proxy(
            &conn,
            provider_id,
            account_id.as_ref(),
        )
    }

    fn get_proxy_status_by_url(&self, url: &str) -> Option<String> {
        let conn = self.conn.lock();
        openproxy_db::free_proxies::get_proxy_status_by_url(&conn, url)
    }

    fn prune_expired_cooldowns(&self) -> Result<usize> {
        let conn = self.conn.lock();
        openproxy_db::cooldowns::prune_expired(&conn)
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
