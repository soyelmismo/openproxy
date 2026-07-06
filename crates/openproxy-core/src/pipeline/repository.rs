use crate::combos::{Combo, ComboTarget};
use crate::error::{CoreError, Result};
use crate::ids::{AccountId, ComboId, ModelRowId, UsageId};
use crate::models::Model;
use crate::secrets::MasterKey;
use crate::cost::UsageInput;
use rusqlite::{params, Connection};
use std::sync::Arc;

fn map_db_error<E: std::error::Error + Send + Sync + 'static>(e: E) -> CoreError {
    CoreError::Database {
        message: e.to_string(),
        source: Some(Box::new(e)),
    }
}

fn map_anyhow_error(e: anyhow::Error) -> CoreError {
    CoreError::Database {
        message: e.to_string(),
        source: None,
    }
}

pub trait PipelineRepository: Send + Sync {
    fn load_combo(&self, combo_id: ComboId) -> Result<Option<Combo>>;
    fn list_targets(&self, combo_id: ComboId) -> Result<Vec<ComboTarget>>;
    fn auto_populate_empty_combo(&self, combo_id: ComboId) -> Result<usize>;
    fn get_account(&self, account_id: AccountId) -> Result<Option<crate::accounts::Account>>;
    fn decrypt_account_key(&self, account_id: AccountId, master_key: &MasterKey) -> Result<String>;
    fn decrypt_access_token(&self, account_id: AccountId, master_key: &MasterKey) -> Result<String>;
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
    fn get_account_label(&self, account_id: AccountId) -> Result<Option<String>>;
    fn record_usage_row(&self, input: &UsageInput) -> Result<Option<UsageId>>;
    fn mark_client_response(&self, row_id: UsageId) -> Result<()>;
    fn record_no_healthy_targets_row(
        &self,
        request_id: &str,
        trace_id: &str,
        combo: &Combo,
        elapsed: u64,
        created_str: &str,
        error_msg: &str,
    ) -> Result<()>;
    fn clear_cooldown(&self, target_id: crate::ids::ComboTargetId) -> Result<()>;
    fn record_cooldown(
        &self,
        target_id: crate::ids::ComboTargetId,
        reason: &str,
        mode: crate::combos::CooldownMode,
        base_secs: u64,
        max_secs: u64,
        factor: u32,
    ) -> Result<()>;
}

#[derive(Clone)]
pub struct SqlitePipelineRepository {
    conn: Arc<parking_lot::Mutex<Connection>>,
}

impl SqlitePipelineRepository {
    pub fn new(conn: Arc<parking_lot::Mutex<Connection>>) -> Self {
        Self { conn }
    }
}

impl PipelineRepository for SqlitePipelineRepository {
    fn load_combo(&self, combo_id: ComboId) -> Result<Option<Combo>> {
        let conn = self.conn.lock();
        crate::combos::get_combo(&conn, combo_id)
    }

    fn list_targets(&self, combo_id: ComboId) -> Result<Vec<ComboTarget>> {
        let conn = self.conn.lock();
        crate::combos::list_targets(&conn, combo_id)
    }

    fn auto_populate_empty_combo(&self, combo_id: ComboId) -> Result<usize> {
        let conn = self.conn.lock();
        crate::combos::auto_populate_empty_combo(&conn, combo_id)
    }

    fn get_account(&self, account_id: AccountId) -> Result<Option<crate::accounts::Account>> {
        let conn = self.conn.lock();
        crate::accounts::get(&conn, account_id)
    }

    fn decrypt_account_key(&self, account_id: AccountId, master_key: &MasterKey) -> Result<String> {
        let conn = self.conn.lock();
        crate::accounts::decrypt_api_key(&conn, account_id, master_key)
    }

    fn decrypt_access_token(&self, account_id: AccountId, master_key: &MasterKey) -> Result<String> {
        let conn = self.conn.lock();
        crate::accounts::decrypt_access_token(&conn, account_id, master_key)
    }

    fn store_oauth_tokens(
        &self,
        account_id: AccountId,
        access_token: &str,
        refresh_token: Option<&str>,
        master_key: &MasterKey,
        token_type: &str,
        expires_at: Option<&str>,
        scope: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        crate::accounts::store_oauth_tokens(
            &conn,
            account_id,
            access_token,
            refresh_token,
            master_key,
            token_type,
            expires_at,
            scope,
            None,
            None,
        )?;
        Ok(())
    }

    fn insert_and_broadcast_notification(
        &self,
        kind: &str,
        payload: &serde_json::Value,
        dedup_key: Option<&str>,
        provider_id: Option<&str>,
    ) -> Result<()> {
        let conn = self.conn.lock();
        crate::notifications::insert_and_broadcast(
            &conn,
            kind,
            payload,
            dedup_key,
            provider_id,
        ).map_err(map_anyhow_error)?;
        Ok(())
    }

    fn load_model(&self, row_id: ModelRowId) -> Result<Model> {
        let conn = self.conn.lock();
        crate::models::get_by_row_id(&conn, row_id)?.ok_or(CoreError::ModelNotFound {
            provider: "<unknown>".into(),
            model: format!("row_id={}", row_id.0),
        })
    }

    fn get_account_label(&self, account_id: AccountId) -> Result<Option<String>> {
        let conn = self.conn.lock();
        Ok(crate::accounts::get(&conn, account_id)?.and_then(|a| a.label))
    }

    fn record_usage_row(&self, input: &UsageInput) -> Result<Option<UsageId>> {
        let conn = match self.conn.try_lock_for(crate::db::conn::HOT_PATH_LOCK_TIMEOUT) {
            Some(g) => g,
            None => {
                tracing::warn!("writer lock unavailable within 100ms; dropping usage row");
                return Ok(None);
            }
        };
        let usage_id = crate::cost::record(&conn, input)?;
        Ok(Some(usage_id))
    }

    fn mark_client_response(&self, row_id: UsageId) -> Result<()> {
        let conn = match self.conn.try_lock_for(crate::db::conn::HOT_PATH_LOCK_TIMEOUT) {
            Some(g) => g,
            None => {
                tracing::error!("failed to acquire lock to update usage row (lock timed out)");
                return Ok(());
            }
        };
        conn.execute("UPDATE usage SET was_winner = 1 WHERE id = ?", params![row_id.0]).map_err(map_db_error)?;
        Ok(())
    }

    fn record_no_healthy_targets_row(
        &self,
        request_id: &str,
        _trace_id: &str,
        combo: &Combo,
        elapsed: u64,
        created_str: &str,
        error_msg: &str,
    ) -> Result<()> {
        let conn = match self.conn.try_lock_for(crate::db::conn::HOT_PATH_LOCK_TIMEOUT) {
            Some(c) => c,
            None => {
                tracing::error!("failed to acquire lock to write usage row (lock timed out)");
                return Ok(());
            }
        };
        let sql = "INSERT INTO usage (
            request_id, combo_id, combo_name, attempt, race_size, strategy, priority_mode,
            provider_id, label, model_id, model, pricing_model,
            prompt_tokens, completion_tokens, total_tokens,
            prompt_cost, completion_cost, total_cost,
            status_code, elapsed_ms, connect_ms, ttft_ms,
            created_at, error, was_winner, stop_reason, partial
        ) VALUES (?, ?, ?, 1, 1, ?, ?, '', '', '', '', 'fixed', 0, 0, 0, 0, 0, 0, 502, ?, NULL, NULL, ?, ?, 1, NULL, 0)";
        let strategy_str = combo.strategy.as_str();
        let pm_str = combo.priority_mode.as_str();
        conn.execute(
            sql,
            params![
                request_id,
                combo.id.0,
                combo.name,
                strategy_str,
                pm_str,
                elapsed as i64,
                created_str,
                error_msg
            ],
        ).map_err(map_db_error)?;
        Ok(())
    }

    fn clear_cooldown(&self, target_id: crate::ids::ComboTargetId) -> Result<()> {
        let conn = self.conn.lock();
        crate::cooldown::clear(&conn, target_id)
    }

    fn record_cooldown(
        &self,
        target_id: crate::ids::ComboTargetId,
        reason: &str,
        mode: crate::combos::CooldownMode,
        base_secs: u64,
        max_secs: u64,
        factor: u32,
    ) -> Result<()> {
        let conn = self.conn.lock();
        crate::cooldown::record_failure_with_mode(
            &conn,
            target_id,
            reason,
            mode,
            base_secs,
            max_secs,
            factor,
        )?;
        Ok(())
    }
}
