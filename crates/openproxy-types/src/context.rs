use crate::combos::ComboTarget;
use crate::models::Model;

#[derive(Clone, Debug)]
pub struct CustomProviderMeta {
    pub access_token: String,
    pub maybe_refresh: Option<String>,
    pub kiro_region: Option<String>,
    pub kiro_profile_arn: Option<String>,
    pub antigravity_project: Option<String>,
    pub antigravity_metadata: Option<String>,
    pub codex_workspace_id: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ResolvedTarget {
    pub target: ComboTarget,
    pub model: Model,
    pub api_key: String,
    pub api_key_label: Option<String>,
    pub custom_meta: Option<CustomProviderMeta>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId};

    #[test]
    fn test_custom_provider_meta_clone_and_debug() {
        let meta = CustomProviderMeta {
            access_token: "token123".to_string(),
            maybe_refresh: Some("refresh123".to_string()),
            kiro_region: Some("us-east-1".to_string()),
            kiro_profile_arn: Some("arn:aws:123".to_string()),
            antigravity_project: Some("proj_1".to_string()),
            antigravity_metadata: Some("meta_data".to_string()),
            codex_workspace_id: Some("ws_123".to_string()),
        };

        let cloned = meta.clone();
        assert_eq!(cloned.access_token, "token123");
        assert_eq!(cloned.maybe_refresh.as_deref(), Some("refresh123"));
        assert_eq!(cloned.kiro_region.as_deref(), Some("us-east-1"));
        assert_eq!(cloned.kiro_profile_arn.as_deref(), Some("arn:aws:123"));
        assert_eq!(cloned.antigravity_project.as_deref(), Some("proj_1"));
        assert_eq!(cloned.antigravity_metadata.as_deref(), Some("meta_data"));
        assert_eq!(cloned.codex_workspace_id.as_deref(), Some("ws_123"));

        let debug_str = format!("{meta:?}");
        assert!(debug_str.contains("CustomProviderMeta"));
        assert!(debug_str.contains("token123"));
    }

    #[test]
    fn test_resolved_target_clone_and_debug() {
        let target = ComboTarget {
            id: ComboTargetId::new(1),
            combo_id: ComboId::new(10),
            provider_id: ProviderId::new("openai"),
            account_id: Some(AccountId::new(100)),
            weight: 1,
            ..Default::default()
        };

        let model = Model {
            row_id: ModelRowId::new(5),
            provider_id: ProviderId::new("openai"),
            model_id: crate::ids::ModelId::new("gpt-4o"),
            display_name: Some("GPT-4o".into()),
            active: true,
            ..Default::default()
        };

        let resolved = ResolvedTarget {
            target,
            model,
            api_key: "sk-testkey".to_string(),
            api_key_label: Some("test-key-label".to_string()),
            custom_meta: None,
        };

        let cloned = resolved.clone();
        assert_eq!(cloned.api_key, "sk-testkey");
        assert_eq!(cloned.api_key_label.as_deref(), Some("test-key-label"));
        assert!(cloned.custom_meta.is_none());

        let debug_str = format!("{resolved:?}");
        assert!(debug_str.contains("ResolvedTarget"));
        assert!(debug_str.contains("sk-testkey"));
    }
}
