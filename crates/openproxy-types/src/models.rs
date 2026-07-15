use serde::{Deserialize, Serialize};
use crate::ids::{ModelRowId, ModelId, ProviderId};
use crate::message::TargetFormat;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub row_id: ModelRowId,
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub display_name: Option<String>,
    pub target_format: TargetFormat,
    pub discovered_at: String,
    pub expires_at: Option<String>,
    pub timeout_overrides_json: Option<String>,
    pub active: bool,
    pub last_test_status: Option<i32>,
    pub last_test_at: Option<String>,
    pub custom: bool,
    pub context_length: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities_json: Option<String>,
    pub family: Option<String>,
    pub model_type: String,
    pub input_modalities_json: Option<String>,
    pub output_modalities_json: Option<String>,
}
