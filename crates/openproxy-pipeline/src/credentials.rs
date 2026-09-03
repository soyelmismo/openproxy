use crate::context::{CustomProviderMeta, ResolvedTarget};
use crate::repository::{KiroMeta, RawAccount};
use openproxy_db::secrets::MasterKey;
use openproxy_types::combos::ComboTarget;
use openproxy_types::error::CoreError;
use openproxy_types::models::Model;
use std::collections::HashMap;

/// In-memory reader for the Antigravity `project_id` from an already-
/// loaded `oauth_provider_specific` JSON value.
///
/// Used in hot paths where the account is already in memory (the
/// pipeline's `Credentials` flow and the smart-warmup scheduler) and
/// issuing a DB query would be wasteful.
///
/// Reads the canonical snake_case `project_id` key (post-C.4 wire
/// format unification; the database migration
/// `000065_antigravity_project_id_wire_format.sql` normalizes all
/// pre-existing camelCase rows to snake_case).
///
/// Returns `None` when the value is not an object, when no
/// `project_id` key is present, or when the value is not a
/// non-empty string.
pub fn antigravity_project_from_value(value: &serde_json::Value) -> Option<String> {
    let pid = value.get("project_id")?.as_str()?;
    let trimmed = pid.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

pub struct CredentialManager;

pub struct ResolutionMaps<'a> {
    pub models_map: &'a HashMap<i64, Model>,
    pub accounts_map: &'a HashMap<i64, RawAccount>,
    pub kiro_map: &'a HashMap<i64, KiroMeta>,
    pub antigravity_map: &'a HashMap<i64, Box<str>>,
    pub providers_map: &'a HashMap<String, String>,
}

impl CredentialManager {
    pub fn resolve_credentials(
        eligible: Vec<ComboTarget>,
        maps: &ResolutionMaps<'_>,
        master_key: &MasterKey,
        oauth_registry: Option<&dyn crate::oauth::PipelineOAuthRegistry>,
    ) -> Vec<ResolvedTarget> {
        let mut resolved = Vec::with_capacity(eligible.len());
        for t in eligible {
            let Some(model) = resolve_target_model(&t, maps.models_map) else {
                continue;
            };

            let creds = match t.account_id {
                Some(account_id) => {
                    resolve_account_credentials(&t, account_id.0, maps, master_key, oauth_registry)
                }
                None => resolve_anonymous_credentials(&t, maps.providers_map),
            };

            let Some((api_key, api_key_label, custom_meta)) = creds else {
                continue;
            };

            resolved.push(ResolvedTarget {
                target: t,
                model,
                api_key,
                api_key_label,
                custom_meta,
            });
        }
        resolved
    }
}

fn resolve_target_model(t: &ComboTarget, models_map: &HashMap<i64, Model>) -> Option<Model> {
    let Some(model_row_id) = t.model_row_id else {
        let err = CoreError::Internal(format!(
            "execute_single called on a sub-combo target (id={})",
            t.id.0
        ));
        tracing::error!(error=%err);
        return None;
    };

    match models_map.get(&model_row_id.0) {
        Some(m) => Some(m.clone()),
        None => {
            let err = CoreError::ModelNotFound {
                provider: "<unknown>".into(),
                model: format!("row_id={}", model_row_id.0),
            };
            tracing::error!(error=%err);
            None
        }
    }
}

fn resolve_anonymous_credentials(
    t: &ComboTarget,
    providers_map: &HashMap<String, String>,
) -> Option<(String, Option<String>, Option<CustomProviderMeta>)> {
    let auth_type = providers_map
        .get(&t.provider_id.0)
        .map(std::string::String::as_str);
    if auth_type == Some("none")
        || openproxy_adapters::adapters::is_anonymous_fallback(&t.provider_id.0)
    {
        Some((String::new(), None, None))
    } else {
        tracing::error!("combo_target {} has no account_id after expansion", t.id.0);
        None
    }
}

#[derive(Default)]
struct ProviderCustomFields {
    kiro_region: Option<String>,
    kiro_profile_arn: Option<String>,
    antigravity_project: Option<String>,
    antigravity_metadata: Option<String>,
    codex_workspace_id: Option<String>,
}

fn extract_provider_custom_meta(
    raw_account: &RawAccount,
    provider_id: &str,
    account_id: i64,
    maps: &ResolutionMaps<'_>,
) -> ProviderCustomFields {
    match provider_id {
        "kiro" => {
            let meta = maps.kiro_map.get(&account_id);
            ProviderCustomFields {
                kiro_region: meta.and_then(|m| m.region.as_deref().map(ToString::to_string)),
                kiro_profile_arn: meta
                    .and_then(|m| m.profile_arn.as_deref().map(ToString::to_string)),
                ..Default::default()
            }
        }
        "antigravity" => {
            let proj = maps
                .antigravity_map
                .get(&account_id)
                .map(|s| s.to_string())
                .or_else(|| {
                    raw_account
                        .oauth_provider_specific
                        .as_deref()
                        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                        .and_then(|v| antigravity_project_from_value(&v))
                });
            let metadata = raw_account
                .oauth_provider_specific
                .as_deref()
                .map(ToString::to_string);
            ProviderCustomFields {
                antigravity_project: proj,
                antigravity_metadata: metadata,
                ..Default::default()
            }
        }
        "codex" => {
            let workspace_id = raw_account
                .oauth_provider_specific
                .as_deref()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
                .and_then(|meta| {
                    meta.get("workspaceId")
                        .or_else(|| meta.get("workspace_id"))
                        .and_then(|v| v.as_str())
                        .filter(|v| !v.is_empty())
                        .map(ToString::to_string)
                });
            ProviderCustomFields {
                codex_workspace_id: workspace_id,
                ..Default::default()
            }
        }
        _ => ProviderCustomFields::default(),
    }
}

fn resolve_oauth_refresh(
    raw_account: &RawAccount,
    provider_id: &str,
    master_key: &MasterKey,
    oauth_registry: Option<&dyn crate::oauth::PipelineOAuthRegistry>,
    adapters: &[openproxy_adapters::adapters::ProviderAdapterEnum],
) -> Option<String> {
    oauth_registry?;
    if !crate::oauth::pipeline_token_needs_refresh(
        raw_account.expires_at.as_deref(),
        provider_id,
        adapters,
    ) {
        return None;
    }
    raw_account
        .refresh_token_encrypted
        .as_ref()
        .and_then(|rt_enc| master_key.decrypt(rt_enc).ok())
}

fn resolve_account_oauth_meta(
    raw_account: &RawAccount,
    t: &ComboTarget,
    account_id: i64,
    maps: &ResolutionMaps<'_>,
    master_key: &MasterKey,
    oauth_registry: Option<&dyn crate::oauth::PipelineOAuthRegistry>,
    adapters: &[openproxy_adapters::adapters::ProviderAdapterEnum],
) -> Option<CustomProviderMeta> {
    let access_token = match &raw_account.access_token_encrypted {
        Some(b) => match master_key.decrypt(b) {
            Ok(k) => k,
            Err(e) => {
                tracing::error!(error=%e, "failed to decrypt access token");
                return None;
            }
        },
        None => {
            tracing::error!("no access token found for account {}", account_id);
            return None;
        }
    };

    let maybe_refresh = resolve_oauth_refresh(
        raw_account,
        t.provider_id.as_str(),
        master_key,
        oauth_registry,
        adapters,
    );
    let fields =
        extract_provider_custom_meta(raw_account, t.provider_id.as_str(), account_id, maps);

    Some(CustomProviderMeta {
        access_token,
        maybe_refresh,
        kiro_region: fields.kiro_region,
        kiro_profile_arn: fields.kiro_profile_arn,
        antigravity_project: fields.antigravity_project,
        antigravity_metadata: fields.antigravity_metadata,
        codex_workspace_id: fields.codex_workspace_id,
    })
}

fn resolve_account_credentials(
    t: &ComboTarget,
    account_id: i64,
    maps: &ResolutionMaps<'_>,
    master_key: &MasterKey,
    oauth_registry: Option<&dyn crate::oauth::PipelineOAuthRegistry>,
) -> Option<(String, Option<String>, Option<CustomProviderMeta>)> {
    let raw_account = maps.accounts_map.get(&account_id).or_else(|| {
        tracing::error!("account {} not found during decryption phase", account_id);
        None
    })?;

    let (key, has_api_key) = match &raw_account.api_key_encrypted {
        Some(b) => match master_key.decrypt(b) {
            Ok(k) => (k, true),
            Err(e) => {
                tracing::error!(error=%e, "failed to decrypt api key");
                return None;
            }
        },
        None => (String::new(), false),
    };

    let adapters = openproxy_adapters::adapters::builtin_adapters();
    let requires_oauth = adapters
        .iter()
        .find(|a| a.id().as_str() == t.provider_id.as_str())
        .is_some_and(|a| a.metadata().requires_oauth);

    if !has_api_key && !requires_oauth {
        tracing::error!("account {} has no API key (OAuth account?)", account_id);
        return None;
    }

    let custom_meta = if requires_oauth {
        Some(resolve_account_oauth_meta(
            raw_account,
            t,
            account_id,
            maps,
            master_key,
            oauth_registry,
            &adapters,
        )?)
    } else {
        None
    };

    Some((
        key,
        raw_account.label.as_deref().map(ToString::to_string),
        custom_meta,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw_with_meta(raw: Option<&str>) -> RawAccount {
        RawAccount {
            api_key_encrypted: None,
            label: None,
            access_token_encrypted: None,
            refresh_token_encrypted: None,
            expires_at: None,
            oauth_provider_specific: raw.map(Into::into),
            quota_model_details: None,
            quota_session_reset_at: None,
        }
    }

    fn project_id_for(raw: Option<&str>) -> Option<String> {
    raw.and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
        .and_then(|v| antigravity_project_from_value(&v))
}

    #[test]
    fn antigravity_project_skips_camel_case_post_migration() {
        // Post-migration (snake_case is canonical), the in-memory helper
        // only inspects `project_id`. Legacy camelCase `projectId` rows
        // are normalized by DB migration 000065 and the DB-backed reader
        // (AntigravityMeta's #[serde(alias = "projectId")]) before this
        // helper is ever called with decrypted JSON. So a still-camelCase
        // payload reaching the in-memory helper means a row was not
        // normalized, and we must return `None` rather than silently
        // shadowing the canonical key.
        let account = raw_with_meta(Some(r#"{"projectId":"proj-abc"}"#));
        assert_eq!(
            project_id_for(account.oauth_provider_specific.as_deref())
                .as_deref(),
            None
        );
    }

    #[test]
    fn antigravity_project_reads_snake_case_account_meta() {
        let account = raw_with_meta(Some(r#"{"project_id":"proj-snake"}"#));

        assert_eq!(
            project_id_for(account.oauth_provider_specific.as_deref())
                .as_deref(),
            Some("proj-snake")
        );
    }

    #[test]
    fn antigravity_project_from_value_reads_snake_case_only() {
        use serde_json::json;
        assert_eq!(
            antigravity_project_from_value(&json!({"project_id":"snake"})),
            Some("snake".to_string())
        );
        // camelCase is NOT supported by the in-memory helper post-C.4
        // (the migration normalizes legacy rows to snake_case).
        assert_eq!(
            antigravity_project_from_value(&json!({"projectId":"camel"})),
            None
        );
        // snake_case wins when both keys are present.
        assert_eq!(
            antigravity_project_from_value(
                &json!({"project_id":"snake","projectId":"camel"})
            ),
            Some("snake".to_string())
        );
        assert_eq!(antigravity_project_from_value(&json!({})), None);
        assert_eq!(antigravity_project_from_value(&json!("not-an-object")), None);
        assert_eq!(
            antigravity_project_from_value(&json!({"project_id":""})),
            None
        );
        assert_eq!(
            antigravity_project_from_value(&json!({"project_id":"   "})),
            None
        );
    }
}
