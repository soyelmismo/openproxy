use crate::combos::ComboTarget;
use crate::models::Model;

#[derive(Clone, Debug)]
pub struct CustomProviderMeta {
    pub access_token: String,
    pub maybe_refresh: Option<String>,
    pub kiro_region: Option<String>,
    pub kiro_profile_arn: Option<String>,
    pub antigravity_project: Option<String>,
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
