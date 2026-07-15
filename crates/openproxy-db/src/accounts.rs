use openproxy_types::{AccountId, ProviderId, Result, CoreError};
use crate::secrets::MasterKey;
use rusqlite::{Connection, params};

pub fn create(
    conn: &Connection,
    provider_id: &ProviderId,
    api_key: Option<&str>,
    master_key: &MasterKey,
    label: Option<&str>,
    priority: i32,
    extra_config_json: Option<&str>,
) -> Result<AccountId> {
    let blob = if let Some(key) = api_key {
        Some(master_key.encrypt(key)?)
    } else {
        None
    };

    let result = conn.execute(
        "INSERT INTO accounts(provider_id, api_key_encrypted, label, priority, extra_config_json) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            provider_id.as_str(),
            blob,
            label,
            priority,
            extra_config_json,
        ],
    );

    match result {
        Ok(_) => {
            let rowid = conn.last_insert_rowid();
            Ok(AccountId(rowid))
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("FOREIGN KEY") {
                Err(CoreError::Validation("unknown provider".into()))
            } else {
                Err(openproxy_types::error::map_db_error_ctx(format!(
                    "insert account for provider {}",
                    provider_id
                ))(e))
            }
        }
    }
}
