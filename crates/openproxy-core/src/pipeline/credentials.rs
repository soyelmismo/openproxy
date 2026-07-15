use crate::combos::ComboTarget;
use crate::error::CoreError;
use crate::models::Model;
use crate::pipeline::context::{CustomProviderMeta, ResolvedTarget};
use crate::pipeline::repository::account::{KiroMeta, RawAccount};
use crate::secrets::MasterKey;
use crate::adapters::ProviderAdapter;
use std::collections::HashMap;
use std::sync::Arc;

pub struct CredentialManager;

impl CredentialManager {
    fn antigravity_project_from_account(raw_account: &RawAccount) -> Option<String> {
        raw_account
            .oauth_provider_specific
            .as_deref()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|meta| {
                meta.get("projectId")
                    .or_else(|| meta.get("project_id"))
                    .and_then(|v| v.as_str())
                    .filter(|v| !v.is_empty())
                    .map(ToString::to_string)
            })
    }

    pub fn resolve_credentials(
        eligible: Vec<ComboTarget>,
        models_map: HashMap<i64, Model>,
        accounts_map: HashMap<i64, RawAccount>,
        kiro_map: HashMap<i64, KiroMeta>,
        antigravity_map: HashMap<i64, String>,
        providers_map: HashMap<String, String>,
        master_key: Arc<MasterKey>,
        oauth_registry: Option<Arc<crate::oauth::OAuthProviderRegistry>>,
    ) -> Vec<ResolvedTarget> {
        let mut resolved = Vec::with_capacity(eligible.len());
        for t in eligible {
            let model_row_id = match t.model_row_id {
                Some(m) => m,
                None => {
                    let err = CoreError::Internal(format!(
                        "execute_single called on a sub-combo target (id={})",
                        t.id.0
                    ));
                    tracing::error!(error=%err);
                    continue;
                }
            };

            let model = match models_map.get(&model_row_id.0) {
                Some(m) => m.clone(),
                None => {
                    let err = CoreError::ModelNotFound {
                        provider: "<unknown>".into(),
                        model: format!("row_id={}", model_row_id.0),
                    };
                    tracing::error!(error=%err);
                    continue;
                }
            };

            let (api_key, api_key_label, custom_meta) = match t.account_id {
                Some(account_id) => {
                    let raw_account = match accounts_map.get(&account_id.0) {
                        Some(r) => r,
                        None => {
                            tracing::error!(
                                "account {} not found during decryption phase",
                                account_id.0
                            );
                            continue;
                        }
                    };

                    let (key, has_api_key) = match &raw_account.api_key_encrypted {
                        Some(b) => match master_key.decrypt(b) {
                            Ok(k) => (k, true),
                            Err(e) => {
                                tracing::error!(error=%e, "failed to decrypt api key");
                                continue;
                            }
                        },
                        None => (String::new(), false),
                    };

                    let adapters = crate::adapters::builtin_adapters();
                    let requires_oauth = adapters
                        .iter()
                        .find(|a| a.id().as_str() == t.provider_id.as_str())
                        .map(|a| a.metadata().requires_oauth)
                        .unwrap_or(false);

                    if !has_api_key && !requires_oauth {
                        tracing::error!("account {} has no API key (OAuth account?)", account_id.0);
                        continue;
                    }
                    let label = raw_account.label.clone();

                    let custom_meta =
                        if requires_oauth {
                            let access_token = match &raw_account.access_token_encrypted {
                                Some(b) => match master_key.decrypt(b) {
                                    Ok(k) => k,
                                    Err(e) => {
                                        tracing::error!(error=%e, "failed to decrypt access token");
                                        continue;
                                    }
                                },
                                None => {
                                    tracing::error!(
                                        "no access token found for account {}",
                                        account_id.0
                                    );
                                    continue;
                                }
                            };

                            let maybe_refresh: Option<String> = if oauth_registry.is_some() {
                                if crate::oauth::pipeline_token_needs_refresh(
                                    raw_account.expires_at.as_deref(),
                                    t.provider_id.as_str(),
                                ) {
                                    if let Some(rt_enc) = &raw_account.refresh_token_encrypted {
                                        master_key.decrypt(rt_enc).ok()
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                            let (
                                kiro_region,
                                kiro_profile_arn,
                                antigravity_project,
                                codex_workspace_id,
                            ) = match t.provider_id.as_str() {
                                "kiro" => {
                                    let meta = kiro_map.get(&account_id.0);
                                    (
                                        meta.and_then(|m| m.region.clone()),
                                        meta.and_then(|m| m.profile_arn.clone()),
                                        None,
                                        None,
                                    )
                                }
                                "antigravity" => {
                                    let proj =
                                        antigravity_map.get(&account_id.0).cloned().or_else(|| {
                                            Self::antigravity_project_from_account(raw_account)
                                        });
                                    if proj.is_none() {
                                        tracing::error!("failed to read antigravity project");
                                        continue;
                                    }
                                    (None, None, proj, None)
                                }
                                "codex" => {
                                    let workspace_id = raw_account
                                        .oauth_provider_specific
                                        .as_deref()
                                        .and_then(|raw| {
                                            serde_json::from_str::<serde_json::Value>(raw).ok()
                                        })
                                        .and_then(|meta| {
                                            meta.get("workspaceId")
                                                .or_else(|| meta.get("workspace_id"))
                                                .and_then(|v| v.as_str())
                                                .filter(|v| !v.is_empty())
                                                .map(ToString::to_string)
                                        });
                                    (None, None, None, workspace_id)
                                }
                                _ => (None, None, None, None),
                            };

                            Some(CustomProviderMeta {
                                access_token,
                                maybe_refresh,
                                kiro_region,
                                kiro_profile_arn,
                                antigravity_project,
                                codex_workspace_id,
                            })
                        } else {
                            None
                        };

                    (key, label, custom_meta)
                }
                None => {
                    let auth_type = providers_map.get(&t.provider_id.0).map(|s| s.as_str());
                    if auth_type == Some("none") || t.provider_id.0 == "opencode-zen" {
                        (String::new(), None, None)
                    } else {
                        tracing::error!(
                            "combo_target {} has no account_id after expansion",
                            t.id.0
                        );
                        continue;
                    }
                }
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
            oauth_provider_specific: raw.map(ToString::to_string),
            quota_model_details: None,
            quota_session_reset_at: None,
        }
    }

    #[test]
    fn antigravity_project_reads_camel_case_account_meta() {
        let account = raw_with_meta(Some(r#"{"projectId":"proj-abc"}"#));

        assert_eq!(
            CredentialManager::antigravity_project_from_account(&account).as_deref(),
            Some("proj-abc")
        );
    }

    #[test]
    fn antigravity_project_reads_snake_case_account_meta() {
        let account = raw_with_meta(Some(r#"{"project_id":"proj-snake"}"#));

        assert_eq!(
            CredentialManager::antigravity_project_from_account(&account).as_deref(),
            Some("proj-snake")
        );
    }
}
