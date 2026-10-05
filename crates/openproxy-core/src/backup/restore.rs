//! Restore state and configuration from a backup bundle into SQLite.

use super::crypto::decrypt_bundle_payload;
use openproxy_db::MasterKey;
use openproxy_db::error::{map_db_error, map_db_error_ctx};
use openproxy_types::backup::{BACKUP_FORMAT_VERSION, BackupBundle, RestoreOptions, RestoreReport};
use openproxy_types::{CoreError, Result};
use rusqlite::{Connection, params};
use std::path::Path;

pub fn restore_backup(
    conn: &mut Connection,
    master_key: &MasterKey,
    bundle: &BackupBundle,
    options: &RestoreOptions,
    db_path: Option<&Path>,
) -> Result<RestoreReport> {
    if bundle.version == 0 || bundle.version > BACKUP_FORMAT_VERSION + 1 {
        return Err(CoreError::Validation(format!(
            "Unsupported backup format version: {}. Expected <= {}",
            bundle.version, BACKUP_FORMAT_VERSION
        )));
    }

    let payload = decrypt_bundle_payload(bundle, options.passphrase.as_deref())?;

    // 1. Safety backup before making any changes
    let mut safety_backup_path = None;
    if let Some(path) = db_path
        && path.exists()
    {
        let _ = openproxy_db::maintenance::checkpoint_wal(conn);
        let timestamp = chrono::Utc::now().format("%Y%m%d_%H%M%S");
        let dest_str = format!("{}.safety-backup-{}.db", path.display(), timestamp);
        let dest = Path::new(&dest_str);
        if let Ok(()) = std::fs::copy(path, dest).map(|_| ()) {
            safety_backup_path = Some(dest_str);
        }
    }

    // 2. Ensure target database schema is at latest version
    openproxy_db::migrations::run(conn)?;

    // 3. Run restore in an immediate transaction with FK checks
    conn.execute_batch("PRAGMA foreign_keys = OFF;")
        .map_err(|e| map_db_error_ctx("disable foreign keys")(e))?;

    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| map_db_error_ctx("begin restore transaction")(e))?;

    // Clear dependent and existing tables
    tx.execute_batch(
        "DELETE FROM target_cooldowns; \
         DELETE FROM provider_proxy_cooldowns; \
         DELETE FROM provider_favicons; \
         DELETE FROM live_limited_models; \
         DELETE FROM oauth_device_tickets; \
         UPDATE usage SET api_key_id = NULL; \
         DELETE FROM combo_targets; \
         DELETE FROM combos; \
         DELETE FROM accounts; \
         DELETE FROM models; \
         DELETE FROM providers; \
         DELETE FROM proxy_sources; \
         DELETE FROM api_keys; \
         DELETE FROM app_config; \
         DELETE FROM sqlite_sequence WHERE name IN ('combo_targets', 'combos', 'accounts', 'models', 'api_keys');",
    )
    .map_err(|e| map_db_error_ctx("clean tables for restore")(e))?;

    // Restore providers
    let mut stmt = tx
        .prepare(
            "INSERT INTO providers ( \
                id, name, base_url, auth_type, format, extra_headers_json, \
                auto_activate_keyword, active, use_proxies, current_proxy_id, \
                proxy_rotation_errors, rate_limit_scope, notif_keyword_only, \
                proxy_rotation_mode, direct_first \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )
        .map_err(map_db_error)?;

    let mut providers_restored = 0;
    for p in &payload.providers {
        let safe_proxy_id = existing_proxy_id(&tx, p.current_proxy_id.as_deref());

        stmt.execute(params![
            p.id.as_str(),
            p.name,
            p.base_url,
            p.auth_type.as_str(),
            p.format.as_str(),
            p.extra_headers_json,
            p.auto_activate_keyword,
            p.active as i64,
            p.use_proxies as i64,
            safe_proxy_id,
            p.proxy_rotation_errors,
            p.rate_limit_scope.as_str(),
            p.notif_keyword_only as i64,
            p.proxy_rotation_mode,
            p.direct_first as i64,
        ])
        .map_err(|e| map_db_error_ctx(format!("restore provider {}", p.id))(e))?;
        providers_restored += 1;
    }
    drop(stmt);

    // Ensure virtual combo provider exists
    tx.execute(
        "INSERT OR IGNORE INTO providers (id, name, base_url, auth_type, format, rate_limit_scope) \
         VALUES ('combo', 'Virtual Combo Provider', 'http://virtual.combo', 'bearer', 'openai', 'account')",
        [],
    )
    .map_err(|e| map_db_error_ctx("seed virtual combo provider")(e))?;

    // Restore accounts with re-encryption using local MasterKey
    let mut stmt = tx
        .prepare(
            "INSERT INTO accounts ( \
                id, provider_id, api_key_encrypted, label, priority, \
                extra_config_json, health_status, rate_limited_until, auth_type, \
                email, oauth_scope, oauth_provider_specific, expires_at, \
                access_token_encrypted, refresh_token_encrypted, current_proxy_id \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )
        .map_err(map_db_error)?;

    let mut accounts_restored = 0;
    for a in &payload.accounts {
        let api_key_raw = a.api_key.as_deref().unwrap_or("");
        let api_key_enc = master_key.encrypt(api_key_raw)?;

        let access_token_enc = if let Some(token) = &a.access_token {
            Some(master_key.encrypt(token)?)
        } else {
            None
        };

        let refresh_token_enc = if let Some(token) = &a.refresh_token {
            Some(master_key.encrypt(token)?)
        } else {
            None
        };

        let oauth_specific_enc = if let Some(spec) = &a.oauth_provider_specific {
            Some(openproxy_db::accounts::encrypt_oauth_provider_specific(
                spec, master_key,
            )?)
        } else {
            None
        };

        let safe_acc_proxy_id = existing_proxy_id(&tx, a.current_proxy_id.as_deref());

        stmt.execute(params![
            a.id,
            a.provider_id.as_str(),
            api_key_enc,
            a.label,
            a.priority,
            a.extra_config_json,
            a.health_status,
            a.rate_limited_until,
            a.auth_type,
            a.email,
            a.oauth_scope,
            oauth_specific_enc,
            a.expires_at,
            access_token_enc,
            refresh_token_enc,
            safe_acc_proxy_id,
        ])
        .map_err(|e| map_db_error_ctx(format!("restore account {}", a.id))(e))?;
        accounts_restored += 1;
    }
    drop(stmt);

    // Restore models
    let mut stmt = tx
        .prepare(
            "INSERT INTO models ( \
                id, provider_id, model_id, display_name, target_format, \
                timeout_overrides_json, active, custom, context_length, \
                max_output_tokens, capabilities_json, family, model_type, \
                input_modalities_json, output_modalities_json, manually_disabled_at \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )
        .map_err(map_db_error)?;

    let mut models_restored = 0;
    for m in &payload.models {
        stmt.execute(params![
            m.id,
            m.provider_id.as_str(),
            m.model_id.as_str(),
            m.display_name,
            m.target_format.as_str(),
            m.timeout_overrides_json,
            m.active as i64,
            m.custom as i64,
            m.context_length,
            m.max_output_tokens,
            m.capabilities_json,
            m.family,
            m.model_type,
            m.input_modalities_json,
            m.output_modalities_json,
            m.manually_disabled_at,
        ])
        .map_err(|e| map_db_error_ctx(format!("restore model {}", m.model_id))(e))?;
        models_restored += 1;
    }
    drop(stmt);

    // Restore combos
    let mut stmt = tx
        .prepare(
            "INSERT INTO combos ( \
                id, name, strategy, race_size, preventive_rate_limit, \
                context_window, priority_mode, cooldown_mode, cooldown_base_secs, \
                cooldown_max_secs, cooldown_factor, lkgp_exploration_rate, \
                selection_window_secs, decision_model, decision_timeout_ms \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        )
        .map_err(map_db_error)?;

    let mut combos_restored = 0;
    for c in &payload.combos {
        stmt.execute(params![
            c.id,
            c.name,
            c.strategy.as_str(),
            c.race_size,
            c.preventive_rate_limit as i64,
            c.context_window,
            c.priority_mode.as_str(),
            c.cooldown_mode.as_str(),
            c.cooldown_base_secs.map(|v| v as i64),
            c.cooldown_max_secs.map(|v| v as i64),
            c.cooldown_factor,
            c.lkgp_exploration_rate,
            c.selection_window_secs.map(|v| v as i64),
            c.decision_model,
            c.decision_timeout_ms.map(|v| v as i64),
        ])
        .map_err(|e| map_db_error_ctx(format!("restore combo {}", c.name))(e))?;
        combos_restored += 1;
    }
    drop(stmt);

    // Restore combo targets
    let mut stmt = tx
        .prepare(
            "INSERT INTO combo_targets ( \
                id, combo_id, provider_id, account_id, model_row_id, \
                sub_combo_id, upstream_model_id, priority_order, weight, \
                active, cooldown_mode, cooldown_base_secs, \
                cooldown_max_secs, cooldown_factor, thinking_effort, description \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
        )
        .map_err(map_db_error)?;

    let valid_account_ids: std::collections::HashSet<i64> =
        payload.accounts.iter().map(|a| a.id).collect();
    let valid_model_ids: std::collections::HashSet<i64> =
        payload.models.iter().map(|m| m.id).collect();
    let valid_combo_ids: std::collections::HashSet<i64> =
        payload.combos.iter().map(|c| c.id).collect();

    let mut combo_targets_restored = 0;
    for t in &payload.combo_targets {
        let safe_account_id = t.account_id.filter(|id| valid_account_ids.contains(id));
        let safe_model_row_id = t.model_row_id.filter(|id| valid_model_ids.contains(id));
        let safe_sub_combo_id = t.sub_combo_id.filter(|id| valid_combo_ids.contains(id));

        stmt.execute(params![
            t.id,
            t.combo_id,
            t.provider_id.as_str(),
            safe_account_id,
            safe_model_row_id,
            safe_sub_combo_id,
            t.upstream_model_id,
            t.priority_order,
            t.weight,
            t.active as i64,
            t.cooldown_mode.as_ref().map(|m| m.as_str()),
            t.cooldown_base_secs.map(|v| v as i64),
            t.cooldown_max_secs.map(|v| v as i64),
            t.cooldown_factor,
            t.thinking_effort,
            t.description,
        ])
        .map_err(|e| map_db_error_ctx(format!("restore combo target {}", t.id))(e))?;
        combo_targets_restored += 1;
    }
    drop(stmt);

    // Restore proxy sources
    let mut stmt = tx
        .prepare(
            "INSERT INTO proxy_sources (id, name, url, priority, active, is_builtin) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .map_err(map_db_error)?;

    let mut proxy_sources_restored = 0;
    for ps in &payload.proxy_sources {
        stmt.execute(params![
            ps.id,
            ps.name,
            ps.url,
            ps.priority,
            ps.active as i64,
            ps.is_builtin as i64,
        ])
        .map_err(|e| map_db_error_ctx(format!("restore proxy source {}", ps.name))(e))?;
        proxy_sources_restored += 1;
    }
    drop(stmt);

    // Restore API keys
    let mut stmt = tx
        .prepare(
            "INSERT INTO api_keys ( \
                id, key_hash, key_prefix, label, scopes_json, \
                allowed_models_json, allowed_combos_json, is_active, revoked_at, \
                expires_at, created_by, blacklisted_providers_json, \
                blacklisted_models_json \
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        )
        .map_err(map_db_error)?;

    let mut api_keys_restored = 0;
    for k in &payload.api_keys {
        stmt.execute(params![
            k.id,
            k.key_hash,
            k.key_prefix,
            k.label,
            k.scopes_json,
            k.allowed_models_json,
            k.allowed_combos_json,
            k.is_active as i64,
            k.revoked_at,
            k.expires_at,
            k.created_by,
            k.blacklisted_providers_json,
            k.blacklisted_models_json,
        ])
        .map_err(|e| map_db_error_ctx(format!("restore api key {}", k.id))(e))?;
        api_keys_restored += 1;
    }
    drop(stmt);

    // Restore app_config
    let mut stmt = tx
        .prepare(
            "INSERT INTO app_config (key, value, updated_at) VALUES (?1, ?2, ?3) \
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .map_err(map_db_error)?;

    let mut app_config_restored = 0;
    for cfg in &payload.app_config {
        stmt.execute(params![cfg.key, cfg.value, cfg.updated_at])
            .map_err(|e| map_db_error_ctx(format!("restore app_config {}", cfg.key))(e))?;
        app_config_restored += 1;
    }
    drop(stmt);

    // Verify foreign keys
    let mut fk_stmt = tx
        .prepare("PRAGMA foreign_key_check;")
        .map_err(|e| map_db_error_ctx("prepare foreign_key_check")(e))?;

    let fk_violations = fk_stmt
        .query_map([], |row| {
            let table: String = row.get(0)?;
            let rowid: i64 = row.get(1)?;
            let target_table: String = row.get(2)?;
            let fkid: i64 = row.get(3)?;
            Ok(format!(
                "{table} (rowid {rowid}) references invalid {target_table} (fkid {fkid})"
            ))
        })
        .map_err(|e| map_db_error_ctx("query foreign_key_check")(e))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| map_db_error_ctx("collect foreign_key_check")(e))?;

    if !fk_violations.is_empty() {
        return Err(CoreError::Validation(format!(
            "Foreign key constraints failed during restore: {}",
            fk_violations.join(", ")
        )));
    }
    drop(fk_stmt);

    // Commit transaction
    tx.commit()
        .map_err(|e| map_db_error_ctx("commit restore transaction")(e))?;

    // Re-enable foreign keys
    conn.execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(|e| map_db_error_ctx("re-enable foreign keys")(e))?;

    Ok(RestoreReport {
        success: true,
        safety_backup_path,
        providers_restored,
        accounts_restored,
        models_restored,
        combos_restored,
        combo_targets_restored,
        proxy_sources_restored,
        api_keys_restored,
        app_config_restored,
        migrations_applied: 0,
        message: format!(
            "Restore successful: restored {providers_restored} providers, {accounts_restored} accounts, {models_restored} models, {combos_restored} combos."
        ),
    })
}

fn existing_proxy_id<'a>(conn: &Connection, proxy_id: Option<&'a str>) -> Option<&'a str> {
    proxy_id.filter(|pid| {
        conn.query_row("SELECT 1 FROM free_proxies WHERE id = ?1", [pid], |_| {
            Ok(true)
        })
        .unwrap_or(false)
    })
}

#[cfg(test)]
mod proxy_tests;
