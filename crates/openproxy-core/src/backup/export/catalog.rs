//! Provider and model catalog backup export helpers.

use super::parse_backup_enum;
use openproxy_db::error::map_db_error;
use openproxy_types::backup::{BackupModel, BackupProvider};
use openproxy_types::providers::AuthType;
use openproxy_types::{ModelId, ProviderFormat, ProviderId, RateLimitScope, Result, TargetFormat};
use rusqlite::Connection;

pub(super) fn export_providers(conn: &Connection) -> Result<Vec<BackupProvider>> {
    let mut extra_by_id = crate::backup::introspection::export_extra_columns_by_pk(
        conn,
        "providers",
        "id",
        &[
            "id", "name", "base_url", "auth_type", "format", "extra_headers_json",
            "auto_activate_keyword", "active", "use_proxies", "current_proxy_id",
            "proxy_rotation_errors", "rate_limit_scope", "notif_keyword_only",
            "proxy_rotation_mode", "direct_first", "stream_mode", "prune_models",
            "prompt_cache_ttl", "created_at", "favicon_base64",
        ],
    )?;

    let mut stmt = conn
        .prepare(
            "SELECT id, name, base_url, auth_type, format, extra_headers_json, \
             auto_activate_keyword, active, use_proxies, current_proxy_id, \
             proxy_rotation_errors, rate_limit_scope, notif_keyword_only, \
             proxy_rotation_mode, direct_first, stream_mode, prune_models, prompt_cache_ttl \
             FROM providers ORDER BY id",
        )
        .map_err(map_db_error)?;

    let providers = stmt
        .query_map([], |row| {
            let auth_str: String = row.get(3)?;
            let fmt_str: String = row.get(4)?;
            let scope_str: String = row.get(11)?;

            let auth_type = parse_backup_enum(&auth_str, 3, AuthType::parse)?;
            let format = parse_backup_enum(&fmt_str, 4, ProviderFormat::parse)?;
            let rate_limit_scope = parse_backup_enum(&scope_str, 11, RateLimitScope::parse)?;

            let id_str: String = row.get(0)?;
            let extra_fields = extra_by_id.remove(&id_str).unwrap_or_default();

            Ok(BackupProvider {
                id: ProviderId::new(id_str),
                name: row.get(1)?,
                base_url: row.get(2)?,
                auth_type,
                format,
                extra_headers_json: row.get(5)?,
                auto_activate_keyword: row.get(6)?,
                active: row.get::<_, i64>(7)? != 0,
                use_proxies: row.get::<_, i64>(8)? != 0,
                current_proxy_id: row.get(9)?,
                proxy_rotation_errors: row.get(10)?,
                rate_limit_scope,
                notif_keyword_only: row.get::<_, i64>(12)? != 0,
                proxy_rotation_mode: row.get(13)?,
                direct_first: row.get::<_, i64>(14)? != 0,
                stream_mode: row.get(15)?,
                prune_models: row.get::<_, i64>(16)? != 0,
                prompt_cache_ttl: row.get(17)?,
                extra_fields,
            })
        })
        .map_err(map_db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_db_error)?;

    Ok(providers)
}

pub(super) fn export_models(conn: &Connection) -> Result<Vec<BackupModel>> {
    let mut extra_by_id = crate::backup::introspection::export_extra_columns_by_pk(
        conn,
        "models",
        "id",
        &[
            "id", "provider_id", "model_id", "display_name", "target_format",
            "timeout_overrides_json", "active", "custom", "context_length",
            "max_output_tokens", "capabilities_json", "family", "model_type",
            "input_modalities_json", "output_modalities_json", "manually_disabled_at",
            "discovered_at", "expires_at", "last_test_status", "last_test_at", "model_id_normalized",
        ],
    )?;

    let mut stmt = conn
        .prepare(
            "SELECT id, provider_id, model_id, display_name, target_format, \
             timeout_overrides_json, active, custom, context_length, \
             max_output_tokens, capabilities_json, family, model_type, \
             input_modalities_json, output_modalities_json, manually_disabled_at \
             FROM models ORDER BY id",
        )
        .map_err(map_db_error)?;

    let models = stmt
        .query_map([], |row| {
            let fmt_str: String = row.get(4)?;
            let target_format = parse_backup_enum(&fmt_str, 4, TargetFormat::parse)?;

            let id_val: i64 = row.get(0)?;
            let extra_fields = extra_by_id.remove(&id_val.to_string()).unwrap_or_default();

            Ok(BackupModel {
                id: id_val,
                provider_id: ProviderId::new(row.get::<_, String>(1)?),
                model_id: ModelId::new(row.get::<_, String>(2)?),
                display_name: row.get(3)?,
                target_format,
                timeout_overrides_json: row.get(5)?,
                active: row.get::<_, i64>(6)? != 0,
                custom: row.get::<_, i64>(7)? != 0,
                context_length: row.get(8)?,
                max_output_tokens: row.get(9)?,
                capabilities_json: row.get(10)?,
                family: row.get(11)?,
                model_type: row.get(12)?,
                input_modalities_json: row.get(13)?,
                output_modalities_json: row.get(14)?,
                manually_disabled_at: row.get(15)?,
                extra_fields,
            })
        })
        .map_err(map_db_error)?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(map_db_error)?;

    Ok(models)
}
