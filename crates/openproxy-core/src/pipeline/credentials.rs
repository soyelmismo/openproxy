use crate::combos::ComboTarget;
use crate::error::CoreError;
use crate::models::Model;
use crate::pipeline::context::{CustomProviderMeta, ResolvedTarget};
use crate::pipeline::repository::account::{KiroMeta, RawAccount};
use crate::secrets::MasterKey;
use std::collections::HashMap;
use std::sync::Arc;

pub struct CredentialManager;

impl CredentialManager {
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

                    let blob = match &raw_account.api_key_encrypted {
                        Some(b) => b,
                        None => {
                            tracing::error!(
                                "account {} has no API key (OAuth account?)",
                                account_id.0
                            );
                            continue;
                        }
                    };

                    let key = match master_key.decrypt(blob) {
                        Ok(k) => k,
                        Err(e) => {
                            tracing::error!(error=%e, "failed to decrypt api key");
                            continue;
                        }
                    };
                    let label = raw_account.label.clone();

                    let custom_meta = if t.provider_id.as_str() == "kiro"
                        || t.provider_id.as_str() == "antigravity"
                    {
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

                        let (kiro_region, kiro_profile_arn, antigravity_project) =
                            match t.provider_id.as_str() {
                                "kiro" => {
                                    let meta = kiro_map.get(&account_id.0);
                                    (
                                        meta.and_then(|m| m.region.clone()),
                                        meta.and_then(|m| m.profile_arn.clone()),
                                        None,
                                    )
                                }
                                "antigravity" => {
                                    let proj = antigravity_map.get(&account_id.0).cloned();
                                    if proj.is_none() {
                                        tracing::error!("failed to read antigravity project");
                                        continue;
                                    }
                                    (None, None, proj)
                                }
                                _ => (None, None, None),
                            };

                        Some(CustomProviderMeta {
                            access_token,
                            maybe_refresh,
                            kiro_region,
                            kiro_profile_arn,
                            antigravity_project,
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
