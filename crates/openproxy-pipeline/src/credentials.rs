use crate::context::{CustomProviderMeta, ResolvedTarget};
use crate::repository::{KiroMeta, RawAccount};
use openproxy_db::secrets::MasterKey;
use openproxy_types::combos::ComboTarget;
use openproxy_types::error::CoreError;
use openproxy_types::ids::{ComboId, ComboTargetId};
use openproxy_types::models::Model;
use std::collections::HashMap;

/// In-memory reader for the Antigravity `project_id` from an already-loaded
/// `oauth_provider_specific` JSON value (hot paths where a DB query would be
/// wasteful). Reads the canonical snake_case `project_id` key; migration
/// `000065_antigravity_project_id_wire_format.sql` normalizes legacy
/// camelCase rows. Returns `None` unless the value is an object with a
/// non-empty `project_id` string.
pub fn antigravity_project_from_value(value: &serde_json::Value) -> Option<String> {
    let pid = value.get("project_id")?.as_str()?;
    let trimmed = pid.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[derive(Debug, Clone)]
pub(crate) struct RejectedTarget {
    pub(crate) target: ComboTarget,
    pub(crate) model: Model,
    pub(crate) combo_name: String,
    pub(crate) reason: String,
}

pub(crate) fn format_missing_account_context(
    target_id: i64,
    combo_id: i64,
    combo_name: &str,
    provider: &str,
    model_id: &str,
    model_row_id: i64,
) -> String {
    format!(
        "combo_target {target_id} has no account_id after expansion (combo '{combo_name}' [{combo_id}], provider '{provider}', model '{model_id}' [row_id {model_row_id}])"
    )
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
        Self::resolve_credentials_with_rejects(
            eligible,
            maps,
            master_key,
            oauth_registry,
            None,
            None,
        )
        .0
    }

    pub(crate) fn resolve_credentials_with_rejects(
        eligible: Vec<ComboTarget>,
        maps: &ResolutionMaps<'_>,
        master_key: &MasterKey,
        oauth_registry: Option<&dyn crate::oauth::PipelineOAuthRegistry>,
        combo_names: Option<&HashMap<ComboId, String>>,
        active_cooldowns: Option<
            &HashMap<ComboId, std::collections::HashSet<ComboTargetId>>,
        >,
    ) -> (Vec<ResolvedTarget>, Vec<RejectedTarget>) {
        let mut resolved = Vec::with_capacity(eligible.len());
        let mut rejected = Vec::new();

        for t in eligible {
            let Some(model) = resolve_target_model(&t, maps.models_map) else {
                continue;
            };

            if let Some(account_id) = t.account_id {
                let creds = resolve_account_credentials(
                    &t,
                    account_id.0,
                    maps,
                    master_key,
                    oauth_registry,
                );
                if let Some((api_key, api_key_label, custom_meta)) = creds {
                    resolved.push(ResolvedTarget {
                        target: t,
                        model,
                        api_key,
                        api_key_label,
                        custom_meta,
                    });
                }
            } else if is_anonymous_target(&t, maps.providers_map) {
                resolved.push(ResolvedTarget {
                    target: t,
                    model,
                    api_key: String::new(),
                    api_key_label: None,
                    custom_meta: None,
                });
            } else {
                let is_already_cooled_down = active_cooldowns
                    .and_then(|m| m.get(&t.combo_id))
                    .is_some_and(|set| set.contains(&t.id));

                let combo_name = combo_names
                    .and_then(|m| m.get(&t.combo_id))
                    .cloned()
                    .unwrap_or_else(|| format!("combo_{}", t.combo_id.0));

                let context_msg = format_missing_account_context(
                    t.id.0,
                    t.combo_id.0,
                    &combo_name,
                    t.provider_id.as_str(),
                    model.model_id.as_str(),
                    model.row_id.0,
                );

                if is_already_cooled_down {
                    tracing::debug!(
                        combo_id = t.combo_id.0,
                        combo_name = %combo_name,
                        provider = %t.provider_id.as_str(),
                        model = %model.model_id.as_str(),
                        target_id = t.id.0,
                        "combo target has no account_id but is already in active cooldown; skipping reject reporting"
                    );
                } else {
                    tracing::error!(
                        combo_id = t.combo_id.0,
                        combo_name = %combo_name,
                        provider = %t.provider_id.as_str(),
                        model = %model.model_id.as_str(),
                        target_id = t.id.0,
                        "{context_msg}"
                    );
                    rejected.push(RejectedTarget {
                        target: t,
                        model,
                        combo_name,
                        reason: context_msg,
                    });
                }
            }
        }

        (resolved, rejected)
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
        Some(m) if m.active => Some(m.clone()),
        Some(_) => {
            tracing::debug!(
                model_row_id = model_row_id.0,
                target_id = t.id.0,
                "skipping combo target whose model is paused/inactive (active = false)"
            );
            None
        }
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

fn is_anonymous_target(t: &ComboTarget, providers_map: &HashMap<String, String>) -> bool {
    let auth_type = providers_map
        .get(&t.provider_id.0)
        .map(std::string::String::as_str);
    auth_type == Some("none")
        || openproxy_adapters::adapters::is_anonymous_fallback(&t.provider_id.0)
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

    let is_oauth = raw_account.access_token_encrypted.is_some() || requires_oauth;

    if !has_api_key && !is_oauth {
        tracing::error!("account {} has neither API key nor OAuth token", account_id);
        return None;
    }

    let custom_meta = if is_oauth {
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
        // A still-camelCase payload means the row escaped migration 000065, so
        // return `None` rather than silently shadowing the canonical key.
        let account = raw_with_meta(Some(r#"{"projectId":"proj-abc"}"#));
        assert_eq!(
            project_id_for(account.oauth_provider_specific.as_deref()).as_deref(),
            None
        );
    }

    #[test]
    fn antigravity_project_reads_snake_case_account_meta() {
        let account = raw_with_meta(Some(r#"{"project_id":"proj-snake"}"#));

        assert_eq!(
            project_id_for(account.oauth_provider_specific.as_deref()).as_deref(),
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
        // camelCase unsupported (migration 000065 normalizes legacy rows);
        // snake_case wins when both keys are present.
        assert_eq!(
            antigravity_project_from_value(&json!({"projectId":"camel"})),
            None
        );
        assert_eq!(
            antigravity_project_from_value(&json!({"project_id":"snake","projectId":"camel"})),
            Some("snake".to_string())
        );
        assert_eq!(antigravity_project_from_value(&json!({})), None);
        assert_eq!(
            antigravity_project_from_value(&json!("not-an-object")),
            None
        );
        assert_eq!(
            antigravity_project_from_value(&json!({"project_id":""})),
            None
        );
        assert_eq!(
            antigravity_project_from_value(&json!({"project_id":"   "})),
            None
        );
    }

    #[test]
    fn resolve_target_model_skips_inactive_models() {
        use openproxy_types::ids::{ComboId, ComboTargetId, ModelId, ModelRowId, ProviderId};
        use openproxy_types::models::Model;
        use std::collections::HashMap;

        let target = ComboTarget {
            id: ComboTargetId(1),
            combo_id: ComboId(1),
            provider_id: ProviderId("p1".into()),
            account_id: None,
            model_row_id: Some(ModelRowId(10)),
            sub_combo_id: None,
            priority_order: 1,
            weight: 1,
            active: true,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
            ..Default::default()
        };

        let mut models_map = HashMap::new();
        let mut model = Model {
            row_id: ModelRowId(10),
            model_id: ModelId("gpt-4".into()),
            active: false,
            ..Default::default()
        };
        models_map.insert(10, model.clone());

        assert!(resolve_target_model(&target, &models_map).is_none());

        model.active = true;
        models_map.insert(10, model);
        assert!(resolve_target_model(&target, &models_map).is_some());
    }

    #[test]
    fn test_format_missing_account_context() {
        let msg = format_missing_account_context(4384, 12, "my-combo", "openai", "gpt-4o", 99);
        assert!(msg.starts_with("combo_target 4384 has no account_id after expansion"));
        assert!(msg.contains("my-combo"));
        assert!(msg.contains("12"));
        assert!(msg.contains("openai"));
        assert!(msg.contains("gpt-4o"));
        assert!(msg.contains("99"));
        assert!(!msg.contains("sk-"));
    }

    #[test]
    fn test_resolve_credentials_with_rejects_behavior() {
        use openproxy_types::ids::{ComboId, ComboTargetId, ModelId, ModelRowId, ProviderId};
        use openproxy_types::models::Model;
        use std::collections::{HashMap, HashSet};

        let target = ComboTarget {
            id: ComboTargetId(4384),
            combo_id: ComboId(77),
            provider_id: ProviderId("openai".into()),
            account_id: None,
            model_row_id: Some(ModelRowId(10)),
            sub_combo_id: None,
            priority_order: 1,
            weight: 1,
            active: true,
            ..Default::default()
        };

        let mut models_map = HashMap::new();
        models_map.insert(
            10,
            Model {
                row_id: ModelRowId(10),
                model_id: ModelId("gpt-4o".into()),
                active: true,
                ..Default::default()
            },
        );

        let mut providers_map = HashMap::new();
        providers_map.insert("openai".into(), "bearer".into());

        let accounts_map = HashMap::new();
        let kiro_map = HashMap::new();
        let antigravity_map = HashMap::new();
        let maps = ResolutionMaps {
            models_map: &models_map,
            accounts_map: &accounts_map,
            kiro_map: &kiro_map,
            antigravity_map: &antigravity_map,
            providers_map: &providers_map,
        };

        let master_key = MasterKey::generate().unwrap();

        let mut combo_names = HashMap::new();
        combo_names.insert(ComboId(77), "test-combo".to_string());

        // Case 1: not in active cooldown -> rejected target returned
        let (resolved, rejected) = CredentialManager::resolve_credentials_with_rejects(
            vec![target.clone()],
            &maps,
            &master_key,
            None,
            Some(&combo_names),
            None,
        );
        assert!(resolved.is_empty());
        assert_eq!(rejected.len(), 1);
        assert_eq!(rejected[0].target.id, ComboTargetId(4384));
        assert_eq!(rejected[0].combo_name, "test-combo");
        assert_eq!(rejected[0].model.model_id.as_str(), "gpt-4o");

        // Case 2: already in active cooldown -> skipped from rejected list
        let mut active_cooldowns = HashMap::new();
        let mut set = HashSet::new();
        set.insert(ComboTargetId(4384));
        active_cooldowns.insert(ComboId(77), set);

        let (resolved2, rejected2) = CredentialManager::resolve_credentials_with_rejects(
            vec![target],
            &maps,
            &master_key,
            None,
            Some(&combo_names),
            Some(&active_cooldowns),
        );
        assert!(resolved2.is_empty());
        assert!(rejected2.is_empty());
    }
}
