//! Exporting OpenProxy state to a self-contained backup bundle.

mod accounts;
mod catalog;
mod routing;
mod settings;

#[cfg(test)]
mod enum_contract_tests;
#[cfg(test)]
mod tests;

use accounts::export_accounts;
use catalog::{export_models, export_providers};
use routing::{export_combo_targets, export_combos};
use settings::{export_api_keys, export_app_config, export_proxy_sources};

use super::crypto::encrypt_bundle_payload;
use openproxy_db::MasterKey;
use openproxy_types::Result;
use openproxy_types::backup::{BACKUP_FORMAT_VERSION, BackupBundle, BackupPayload};
use rusqlite::Connection;

fn parse_backup_enum<T, E>(
    value: &str,
    column: usize,
    parse: impl FnOnce(&str) -> std::result::Result<T, E>,
) -> rusqlite::Result<T>
where
    E: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    parse(value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            column,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        )
    })
}

pub fn export_backup(
    conn: &Connection,
    master_key: &MasterKey,
    passphrase: Option<&str>,
) -> Result<BackupBundle> {
    let providers = export_providers(conn)?;
    let accounts = export_accounts(conn, master_key)?;
    let models = export_models(conn)?;
    let combos = export_combos(conn)?;
    let combo_targets = export_combo_targets(conn)?;
    let proxy_sources = export_proxy_sources(conn)?;
    let api_keys = export_api_keys(conn)?;
    let app_config = export_app_config(conn)?;

    let payload = BackupPayload {
        providers,
        accounts,
        models,
        combos,
        combo_targets,
        proxy_sources,
        api_keys,
        app_config,
    };

    if let Some(passphrase) = passphrase
        && !passphrase.trim().is_empty()
    {
        encrypt_bundle_payload(&payload, passphrase)
    } else {
        Ok(BackupBundle {
            version: BACKUP_FORMAT_VERSION,
            exported_at: chrono::Utc::now().to_rfc3339(),
            openproxy_version: env!("CARGO_PKG_VERSION").to_string(),
            encrypted: false,
            kdf: None,
            kdf_salt: None,
            kdf_iterations: None,
            nonce: None,
            ciphertext: None,
            payload: Some(payload),
        })
    }
}
