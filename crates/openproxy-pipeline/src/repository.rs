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

pub trait PipelineRepository: Send + Sync {
    fn load_combo(&self, combo_id: ComboId) -> Result<Option<Combo>>;
    fn list_targets(&self, combo_id: ComboId) -> Result<Vec<ComboTarget>>;
    fn auto_populate_empty_combo(&self, combo_id: ComboId) -> Result<usize>;
    fn get_account(&self, account_id: AccountId, master_key: &MasterKey)
    -> Result<Option<Account>>;
    fn decrypt_account_key(&self, account_id: AccountId, master_key: &MasterKey) -> Result<String>;
    fn decrypt_access_token(&self, account_id: AccountId, master_key: &MasterKey)
    -> Result<String>;
    #[allow(clippy::too_many_arguments)]
    fn store_oauth_tokens(
        &self,
        account_id: AccountId,
        access_token: &str,
        refresh_token: Option<&str>,
        master_key: &MasterKey,
        token_type: &str,
        expires_at: Option<&str>,
        scope: Option<&str>,
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
    fn get_or_assign_provider_proxy(&self, provider_id: &ProviderId) -> Result<Option<String>>;
    fn get_proxy_status_by_url(&self, url: &str) -> Option<String>;

    // Batch Loading
    fn get_models_by_row_ids(&self, model_row_ids: &[ModelRowId]) -> Result<HashMap<i64, Model>>;
    #[allow(clippy::type_complexity)]
    fn get_accounts_meta(
        &self,
        account_ids: &[AccountId],
    ) -> Result<(
        HashMap<i64, RawAccount>,
        HashMap<i64, KiroMeta>,
        HashMap<i64, String>,
    )>;
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
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock();
        let combo = conn
            .query_row(
                "SELECT id, name, strategy, race_size, created_at, context_window, \
                    priority_mode, cooldown_mode, cooldown_base_secs, cooldown_max_secs, \
                    cooldown_factor, lkgp_exploration_rate, selection_window_secs \
             FROM combos WHERE id = ?1",
                rusqlite::params![combo_id.0],
                |row| {
                    let strategy_str: String = row.get(2)?;
                    let strategy = openproxy_types::combos::Strategy::parse(&strategy_str)
                        .map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Text,
                                Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                            )
                        })?;
                    let priority_mode_str: Option<String> = row.get(6)?;
                    let priority_mode = priority_mode_str
                        .map(|s| {
                            openproxy_types::combos::PriorityMode::parse(&s)
                                .unwrap_or(openproxy_types::combos::PriorityMode::Strict)
                        })
                        .unwrap_or(openproxy_types::combos::PriorityMode::Strict);
                    let cooldown_mode_str: Option<String> = row.get(7)?;
                    let cooldown_mode = cooldown_mode_str
                        .map(|s| {
                            openproxy_types::config::CooldownMode::parse(&s)
                                .unwrap_or(openproxy_types::config::CooldownMode::Flat)
                        })
                        .unwrap_or(openproxy_types::config::CooldownMode::Flat);
                    Ok(Combo {
                        id: ComboId(row.get(0)?),
                        name: row.get(1)?,
                        strategy,
                        race_size: row.get::<_, i64>(3)? as u8,
                        created_at: row.get(4)?,
                        context_window: row.get(5)?,
                        priority_mode,
                        cooldown_mode,
                        cooldown_base_secs: row.get::<_, Option<i64>>(8)?.map(|v| v as u64),
                        cooldown_max_secs: row.get::<_, Option<i64>>(9)?.map(|v| v as u64),
                        cooldown_factor: row.get::<_, Option<i64>>(10)?.map(|v| v as u32),
                        lkgp_exploration_rate: row.get(11)?,
                        selection_window_secs: row.get::<_, Option<i64>>(12)?.map(|v| v as u64),
                    })
                },
            )
            .optional()
            .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?;
        Ok(combo)
    }

    fn list_targets(&self, combo_id: ComboId) -> Result<Vec<ComboTarget>> {
        list_targets(&self.conn.lock(), combo_id)
    }
    fn auto_populate_empty_combo(&self, combo_id: ComboId) -> Result<usize> {
        auto_populate_empty_combo(&self.conn.lock(), combo_id)
    }
    fn get_account(
        &self,
        account_id: AccountId,
        _master_key: &MasterKey,
    ) -> Result<Option<Account>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock();
        let row = conn
            .query_row(
                "SELECT a.id, a.provider_id, a.label, a.priority, a.extra_config_json, a.health_status, \
                    a.rate_limited_until, a.expires_at, a.created_at, \
                    a.quota_session_used, a.quota_session_limit, a.quota_session_reset_at, \
                    a.quota_weekly_used, a.quota_weekly_limit, a.quota_weekly_reset_at, \
                    a.quota_plan_name, a.quota_last_fetched_at, a.quota_fetch_error, a.quota_model_details, \
                    p.auth_type \
             FROM accounts a \
             JOIN providers p ON a.provider_id = p.id \
             WHERE a.id = ?1",
                rusqlite::params![account_id.0],
                |row| {
                    let health_str: String = row.get(5)?;
                    let health_status = openproxy_types::HealthStatus::parse(&health_str)
                        .unwrap_or(openproxy_types::HealthStatus::Healthy);

                    let quota_model_details_str: Option<String> = row.get(18)?;
                    let quota_model_details = quota_model_details_str.and_then(|s| {
                        serde_json::from_str(&s).ok()
                    });

                    Ok(Account {
                        id: AccountId(row.get(0)?),
                        provider_id: openproxy_types::ids::ProviderId::new(
                            row.get::<_, String>(1)?,
                        ),
                        label: row.get(2)?,
                        priority: row.get(3)?,
                        extra_config_json: row.get(4)?,
                        health_status,
                        rate_limited_until: row.get(6)?,
                        quota_session_used: row.get(9)?,
                        quota_session_limit: row.get(10)?,
                        quota_session_reset_at: row.get(11)?,
                        quota_weekly_used: row.get(12)?,
                        quota_weekly_limit: row.get(13)?,
                        quota_weekly_reset_at: row.get(14)?,
                        quota_plan_name: row.get(15)?,
                        quota_last_fetched_at: row.get(16)?,
                        quota_fetch_error: row.get(17)?,
                        quota_model_details,
                        auth_type: row.get(19)?,
                        email: None,
                        oauth_scope: None,
                        oauth_provider_specific: None,
                        expires_at: row.get(7)?,
                        created_at: row.get(8)?,
                    })
                },
            )
            .optional()
            .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?;
        Ok(row)
    }

    fn decrypt_account_key(&self, account_id: AccountId, master_key: &MasterKey) -> Result<String> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock();
        let val: Option<Vec<u8>> = conn
            .query_row(
                "SELECT api_key_encrypted FROM accounts WHERE id = ?1",
                rusqlite::params![account_id.0],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?
            .flatten();
        match val {
            Some(b) => master_key.decrypt(&b),
            None => Ok(String::new()),
        }
    }

    fn decrypt_access_token(
        &self,
        account_id: AccountId,
        master_key: &MasterKey,
    ) -> Result<String> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock();
        let val: Option<Vec<u8>> = conn
            .query_row(
                "SELECT access_token_encrypted FROM accounts WHERE id = ?1",
                rusqlite::params![account_id.0],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?
            .flatten();
        match val {
            Some(b) => master_key.decrypt(&b),
            None => Ok(String::new()),
        }
    }

    fn store_oauth_tokens(
        &self,
        account_id: AccountId,
        access_token: &str,
        refresh_token: Option<&str>,
        master_key: &MasterKey,
        _token_type: &str,
        expires_at: Option<&str>,
        _scope: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        let access_token_encrypted = master_key.encrypt(access_token)?;
        let refresh_token_encrypted = refresh_token.map(|rt| master_key.encrypt(rt)).transpose()?;
        conn.execute(
            "UPDATE accounts SET access_token_encrypted = ?1, refresh_token_encrypted = ?2, expires_at = ?3 WHERE id = ?4",
            rusqlite::params![access_token_encrypted, refresh_token_encrypted, expires_at, account_id.0]
        ).map(|_| ()).map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))
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
                payload: payload.clone(),
                created_at,
            },
        );
        Ok(())
    }

    fn load_model(&self, row_id: ModelRowId) -> Result<Model> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT id, provider_id, model_id, display_name, target_format, \
                    discovered_at, expires_at, timeout_overrides_json, active, \
                    last_test_status, last_test_at, custom, \
                    context_length, max_output_tokens, capabilities_json, \
                    family, model_type, input_modalities_json, \
                    output_modalities_json \
             FROM models WHERE id = ?1",
            rusqlite::params![row_id.0],
            |row| {
                let target_format_str: String = row.get(4)?;
                let target_format = match target_format_str.as_str() {
                    "openai" => openproxy_types::message::TargetFormat::Openai,
                    "anthropic" => openproxy_types::message::TargetFormat::Anthropic,
                    "gemini" => openproxy_types::message::TargetFormat::Gemini,
                    "responses" => openproxy_types::message::TargetFormat::Responses,
                    other => {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("invalid target_format in db: {}", other),
                            )),
                        ));
                    }
                };

                let active_bit: i64 = row.get(8)?;
                let active = match active_bit {
                    0 => false,
                    1 => true,
                    other => {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            8,
                            rusqlite::types::Type::Integer,
                            Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("invalid active bit in db: {}", other),
                            )),
                        ));
                    }
                };

                let custom_bit: i64 = row.get(11)?;
                let custom = match custom_bit {
                    0 => false,
                    1 => true,
                    other => {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            11,
                            rusqlite::types::Type::Integer,
                            Box::new(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                format!("invalid custom bit in db: {}", other),
                            )),
                        ));
                    }
                };

                Ok(Model {
                    row_id: openproxy_types::ids::ModelRowId(row.get(0)?),
                    provider_id: openproxy_types::ids::ProviderId::new(row.get::<_, String>(1)?),
                    model_id: openproxy_types::ids::ModelId::new(row.get::<_, String>(2)?),
                    display_name: row.get(3)?,
                    target_format,
                    discovered_at: row.get(5)?,
                    expires_at: row.get(6)?,
                    timeout_overrides_json: row.get(7)?,
                    active,
                    last_test_status: row.get(9)?,
                    last_test_at: row.get(10)?,
                    custom,
                    context_length: row.get(12)?,
                    max_output_tokens: row.get(13)?,
                    capabilities_json: row.get(14)?,
                    family: row.get(15)?,
                    model_type: row.get(16)?,
                    input_modalities_json: row.get(17)?,
                    output_modalities_json: row.get(18)?,
                })
            },
        )
        .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))
    }

    fn get_account_label(
        &self,
        account_id: AccountId,
        _master_key: &MasterKey,
    ) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock();
        let label = conn
            .query_row(
                "SELECT label FROM accounts WHERE id = ?1",
                rusqlite::params![account_id.0],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?;
        Ok(label)
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
            "UPDATE usage SET was_winner = 1 WHERE request_id = ?1 AND attempt = ?2 AND combo_target_id = ?3",
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
            CooldownMode::Flat => base_secs,
            CooldownMode::Exponential => {
                let mut exp_secs =
                    base_secs.saturating_mul((factor as u64).saturating_pow(current_count));
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
        let mut map = HashMap::new();
        for id in model_row_ids {
            let m = self.load_model(*id)?;
            map.insert(id.0, m);
        }
        Ok(map)
    }

    fn get_accounts_meta(
        &self,
        account_ids: &[AccountId],
    ) -> Result<(
        HashMap<i64, RawAccount>,
        HashMap<i64, KiroMeta>,
        HashMap<i64, String>,
    )> {
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
            if let Some((api_key, label, access, refresh, expires, oauth_prov, _email, extra_json)) =
                row
            {
                raw_map.insert(
                    id.0,
                    RawAccount {
                        api_key_encrypted: api_key,
                        label,
                        access_token_encrypted: access,
                        refresh_token_encrypted: refresh,
                        expires_at: expires,
                        oauth_provider_specific: oauth_prov.clone(),
                        quota_session_reset_at: None,
                        quota_model_details: None,
                    },
                );
                // Extract projectId from oauth_provider_specific JSON for antigravity accounts.
                // Do NOT use the email column — the API needs a real GCP project ID.
                if let Some(ref oauth_json) = oauth_prov {
                    if let Ok(meta) = serde_json::from_str::<serde_json::Value>(oauth_json) {
                        if let Some(pid) = meta
                            .get("projectId")
                            .or_else(|| meta.get("project_id"))
                            .and_then(|v| v.as_str())
                            .filter(|v| !v.is_empty())
                        {
                            ag_map.insert(id.0, pid.to_string());
                        }
                    }
                }
                if let Some(cfg_str) = extra_json
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(&cfg_str)
                {
                    let region = val
                        .get("region")
                        .or(val.get("aws_region"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let profile_arn = val
                        .get("profile_arn")
                        .or(val.get("aws_role_arn"))
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
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
        
        let current_json_opt: Option<String> = conn.query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
            rusqlite::params![account_id],
            |row| row.get(0),
        ).optional().map_err(|e| openproxy_types::error::CoreError::Database { message: "query account".into(), source: Some(Box::new(e)) })?.flatten();

        let mut meta = if let Some(json_str) = current_json_opt {
            serde_json::from_str::<serde_json::Value>(&json_str).unwrap_or_else(|_| serde_json::json!({}))
        } else {
            serde_json::json!({})
        };

        if let Some(obj) = meta.as_object_mut() {
            obj.insert("projectId".to_string(), serde_json::Value::String(new_project_id.to_string()));
        }

        let new_json_str = serde_json::to_string(&meta).unwrap_or_default();
        
        conn.execute(
            "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
            rusqlite::params![new_json_str, account_id],
        ).map_err(|e| openproxy_types::error::CoreError::Database { message: "update account".into(), source: Some(Box::new(e)) })?;
        
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
        let now = chrono::Utc::now().to_rfc3339();
        conn.execute(
            "UPDATE free_proxies SET status = ?1, latency_ms = ?2, last_validated = ?3, updated_at = ?4 WHERE id = ?5",
            rusqlite::params![status, None::<i64>, now, now, proxy_id],
        )
        .map(|_| ())
        .map_err(|e| openproxy_types::error::CoreError::Database {
            message: e.to_string(),
            source: Some(Box::new(e)),
        })
    }

    fn get_or_assign_provider_proxy(&self, provider_id: &ProviderId) -> Result<Option<String>> {
        use rusqlite::OptionalExtension;
        let conn = self.conn.lock();
        let provider = match openproxy_db::providers::get(&conn, provider_id)? {
            Some(p) => p,
            None => return Ok(None),
        };

        if !provider.use_proxies {
            return Ok(None);
        }

        if let Some(ref proxy_id) = provider.current_proxy_id {
            let exists_and_alive: Option<(String, i64, String)> = conn
                .query_row(
                    "SELECT host, port, type FROM free_proxies WHERE id = ?1 AND status = 'alive'",
                    rusqlite::params![proxy_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()
                .map_err(|e| openproxy_types::error::CoreError::Database {
                    message: format!("query current proxy: {}", e),
                    source: Some(Box::new(e)),
                })?;

            if let Some((host, port, proto)) = exists_and_alive {
                return Ok(Some(format!(
                    "{}://{}:{}",
                    proto.to_lowercase(),
                    host,
                    port
                )));
            }
        }

        let new_proxy: Option<(String, String, i64, String)> = conn
            .query_row(
                "SELECT id, host, port, type FROM free_proxies WHERE status = 'alive' ORDER BY latency_ms ASC, random() LIMIT 1",
                [],
                |row| Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                )),
            )
            .optional()
            .map_err(|e| openproxy_types::error::CoreError::Database {
                message: format!("query new proxy: {}", e),
                source: Some(Box::new(e)),
            })?;

        if let Some((id, host, port, proto)) = new_proxy {
            openproxy_db::providers::update_current_proxy(&conn, provider_id, Some(&id))?;
            return Ok(Some(format!(
                "{}://{}:{}",
                proto.to_lowercase(),
                host,
                port
            )));
        }

        Ok(None)
    }

    fn get_proxy_status_by_url(&self, url: &str) -> Option<String> {
        let conn = self.conn.lock();
        let parts: Vec<&str> = url.split("://").collect();
        if parts.len() != 2 {
            return None;
        }
        let host_port = parts[1];
        let host_port_parts: Vec<&str> = host_port.split(':').collect();
        if host_port_parts.len() != 2 {
            return None;
        }
        let host = host_port_parts[0];
        let port: i64 = host_port_parts[1].parse().ok()?;
        conn.query_row(
            "SELECT status FROM free_proxies WHERE host = ?1 AND port = ?2",
            rusqlite::params![host, port],
            |row| row.get::<_, String>(0),
        )
        .ok()
    }

    fn prune_expired_cooldowns(&self) -> Result<usize> {
        let conn = self.conn.lock();
        prune_expired_cooldowns(&conn)
    }
}

pub fn list_targets(conn: &rusqlite::Connection, combo_id: ComboId) -> Result<Vec<ComboTarget>> {
    let mut stmt = conn
        .prepare(
            "SELECT ct.id, ct.combo_id, ct.provider_id, ct.account_id, ct.model_row_id, \
                    ct.sub_combo_id, ct.priority_order, ct.weight, p.rate_limit_scope \
             FROM combo_targets ct \
             INNER JOIN providers p ON p.id = ct.provider_id \
             WHERE ct.combo_id = ?1 AND p.active = 1 \
                 \
             ORDER BY ct.priority_order ASC, ct.id ASC",
        )
        .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?;
    let rows = stmt
        .query_map(rusqlite::params![combo_id.0], |row| {
            let id: i64 = row.get(0)?;
            let combo_id: i64 = row.get(1)?;
            let provider_id: String = row.get(2)?;
            let account_id: Option<i64> = row.get(3)?;
            let model_row_id: Option<i64> = row.get(4)?;
            let sub_combo_id: Option<i64> = row.get(5)?;
            let priority_order: i32 = row.get(6)?;
            let weight: i32 = row.get::<_, Option<i64>>(7)?.unwrap_or(1) as i32;
            let rate_limit_scope: String = row.get(8)?;

            Ok(ComboTarget {
                id: openproxy_types::ids::ComboTargetId(id),
                combo_id: ComboId(combo_id),
                provider_id: openproxy_types::ids::ProviderId::new(provider_id),
                account_id: account_id.map(AccountId),
                model_row_id: model_row_id.map(ModelRowId),
                sub_combo_id: sub_combo_id.map(ComboId),
                priority_order,
                weight,
                rate_limit_scope: openproxy_types::providers::RateLimitScope::parse(
                    &rate_limit_scope,
                )
                .unwrap_or_default(),
            })
        })
        .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?;
    let mut res = Vec::new();
    for r in rows {
        res.push(r.map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?);
    }
    Ok(res)
}

pub fn auto_populate_empty_combo(conn: &rusqlite::Connection, combo_id: ComboId) -> Result<usize> {
    let provider_id: Option<String> = conn.query_row(
            "SELECT p.id FROM providers p \
             WHERE p.active = 1 AND p.id != 'virtual' \
             AND EXISTS (SELECT 1 FROM accounts a WHERE a.provider_id = p.id AND a.health_status = 'healthy') \
             AND EXISTS (SELECT 1 FROM models m WHERE m.provider_id = p.id AND m.active = 1) \
             ORDER BY p.id ASC LIMIT 1",
            [],
            |row| row.get(0)
        ).unwrap_or(None);

    if let Some(pid) = provider_id {
        let mut added = 0;
        let mut stmt = conn
            .prepare("SELECT id FROM models WHERE provider_id = ?1 AND active = 1")
            .map_err(|e| openproxy_types::error::CoreError::Database {
                message: "prepare models".into(),
                source: Some(Box::new(e)),
            })?;
        let mut rows = stmt.query(rusqlite::params![pid]).map_err(|e| {
            openproxy_types::error::CoreError::Database {
                message: "query models".into(),
                source: Some(Box::new(e)),
            }
        })?;
        while let Some(r) =
            rows.next()
                .map_err(|e| openproxy_types::error::CoreError::Database {
                    message: "next model".into(),
                    source: Some(Box::new(e)),
                })?
        {
            let mid =
                r.get::<_, i64>(0)
                    .map_err(|e| openproxy_types::error::CoreError::Database {
                        message: "get mid".into(),
                        source: Some(Box::new(e)),
                    })?;
            let res = conn.execute(
                    "INSERT OR IGNORE INTO combo_targets(combo_id, provider_id, model_row_id, priority_order, weight) \
                     VALUES (?1, ?2, ?3, ?4, 100)",
                    rusqlite::params![combo_id.0, pid, mid, mid]
                ).map_err(|e| openproxy_types::error::CoreError::Database { message: "insert combo_targets".into(), source: Some(Box::new(e)) })?;
            added += res;
        }
        Ok(added)
    } else {
        Ok(0)
    }
}

pub fn expand_account_rotation(
    conn: &rusqlite::Connection,
    targets: Vec<ComboTarget>,
) -> Result<Vec<ComboTarget>> {
    let mut out = Vec::new();
    for t in targets {
        if t.account_id.is_some() || t.sub_combo_id.is_some() {
            out.push(t);
            continue;
        }
        let mut stmt = conn.prepare("SELECT id FROM accounts WHERE provider_id = ?1 AND health_status = 'healthy' ORDER BY priority ASC, id ASC").map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?;
        let mut rows = stmt
            .query(rusqlite::params![t.provider_id.as_str()])
            .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?;
        let mut count = 0;
        while let Some(r) = rows
            .next()
            .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?
        {
            let mut ct = t.clone();
            ct.account_id =
                Some(AccountId(r.get::<_, i64>(0).map_err(|e| {
                    openproxy_types::error::CoreError::Internal(e.to_string())
                })?));
            out.push(ct);
            count += 1;
        }
        if count == 0 {
            out.push(t);
        }
    }
    Ok(out)
}

pub fn resolve_combo_to_targets(
    conn: &rusqlite::Connection,
    combo_id: ComboId,
    visited: &mut Vec<ComboId>,
    depth: u32,
) -> Result<Vec<ComboTarget>> {
    if depth > 5 {
        return Err(openproxy_types::error::CoreError::Validation(format!(
            "max sub-combo depth ({}) exceeded",
            5
        )));
    }
    if visited.contains(&combo_id) {
        return Err(openproxy_types::error::CoreError::Validation(format!(
            "cyclic combo detected at id {}",
            combo_id.0
        )));
    }
    visited.push(combo_id);

    let targets = list_targets(conn, combo_id)?;
    let mut flat = Vec::new();
    for t in targets {
        if let Some(sub_id) = t.sub_combo_id {
            let sub = resolve_combo_to_targets(conn, sub_id, visited, depth + 1)?;
            flat.extend(sub);
        } else {
            flat.push(t);
        }
    }
    visited.pop();
    Ok(flat)
}

pub fn prune_expired_cooldowns(conn: &rusqlite::Connection) -> Result<usize> {
    let now = chrono::Utc::now().to_rfc3339();
    conn.execute(
        "DELETE FROM target_cooldowns WHERE datetime(cooldown_until) <= datetime(?1)",
        rusqlite::params![now],
    )
    .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))
}
