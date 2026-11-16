//! Validation and inspection of backup bundles without mutating state.

use super::crypto::decrypt_bundle_payload;
use openproxy_types::backup::{BACKUP_FORMAT_VERSION, BackupBundle, BackupValidationSummary};
use openproxy_types::{CoreError, Result};
use std::collections::HashSet;

pub fn validate_backup(
    bundle: &BackupBundle,
    passphrase: Option<&str>,
) -> Result<BackupValidationSummary> {
    if bundle.version == 0 || bundle.version > BACKUP_FORMAT_VERSION {
        return Err(CoreError::Validation(format!(
            "Unsupported backup format version: {}. Expected version <= {}",
            bundle.version, BACKUP_FORMAT_VERSION
        )));
    }

    let payload = decrypt_bundle_payload(bundle, passphrase)?;

    let mut warnings = Vec::new();

    if bundle.version > BACKUP_FORMAT_VERSION {
        warnings.push(format!(
            "Backup was created by a newer version of OpenProxy (format v{}), some fields may not be supported.",
            bundle.version
        ));
    }

    // Check account API keys
    let accounts_without_key = payload
        .accounts
        .iter()
        .filter(|a| a.api_key.as_deref().is_none_or(str::is_empty) && a.access_token.is_none())
        .count();
    if accounts_without_key > 0 {
        warnings.push(format!(
            "{accounts_without_key} account(s) have no API key or token configured."
        ));
    }

    // Check combo target integrity
    let provider_ids: HashSet<&str> = payload.providers.iter().map(|p| p.id.as_str()).collect();
    let combo_ids: HashSet<i64> = payload.combos.iter().map(|c| c.id).collect();

    let missing_combo_refs = payload
        .combo_targets
        .iter()
        .filter(|t| !combo_ids.contains(&t.combo_id))
        .count();
    if missing_combo_refs > 0 {
        warnings.push(format!(
            "{missing_combo_refs} target(s) reference non-existent combos in the backup."
        ));
    }

    let missing_provider_refs = payload
        .combo_targets
        .iter()
        .filter(|t| t.sub_combo_id.is_none() && !provider_ids.contains(t.provider_id.as_str()))
        .count();
    if missing_provider_refs > 0 {
        warnings.push(format!(
            "{missing_provider_refs} target(s) reference providers not found in the backup."
        ));
    }

    let account_ids: HashSet<i64> = payload.accounts.iter().map(|a| a.id).collect();
    let missing_account_refs = payload
        .combo_targets
        .iter()
        .filter(|t| t.account_id.is_some_and(|aid| !account_ids.contains(&aid)))
        .count();
    if missing_account_refs > 0 {
        warnings.push(format!(
            "{missing_account_refs} target(s) reference accounts not found in the backup."
        ));
    }

    let model_ids: HashSet<i64> = payload.models.iter().map(|m| m.id).collect();
    let missing_model_refs = payload
        .combo_targets
        .iter()
        .filter(|t| t.model_row_id.is_some_and(|mid| !model_ids.contains(&mid)))
        .count();
    if missing_model_refs > 0 {
        warnings.push(format!(
            "{missing_model_refs} target(s) reference models not found in the backup."
        ));
    }

    Ok(BackupValidationSummary {
        version: bundle.version,
        encrypted: bundle.encrypted,
        exported_at: bundle.exported_at.clone(),
        openproxy_version: bundle.openproxy_version.clone(),
        providers_count: payload.providers.len(),
        accounts_count: payload.accounts.len(),
        models_count: payload.models.len(),
        combos_count: payload.combos.len(),
        combo_targets_count: payload.combo_targets.len(),
        proxy_sources_count: payload.proxy_sources.len(),
        api_keys_count: payload.api_keys.len(),
        app_config_count: payload.app_config.len(),
        warnings,
    })
}
