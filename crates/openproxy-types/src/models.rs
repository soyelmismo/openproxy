use crate::ids::{ModelId, ModelRowId, ProviderId};
use crate::message::TargetFormat;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Model {
    pub row_id: ModelRowId,
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub display_name: Option<Box<str>>,
    pub discovered_at: Box<str>,
    pub expires_at: Option<Box<str>>,
    pub timeout_overrides_json: Option<Box<str>>,
    pub last_test_at: Option<Box<str>>,
    pub context_length: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub capabilities_json: Option<Box<str>>,
    pub family: Option<Box<str>>,
    pub model_type: Box<str>,
    pub input_modalities_json: Option<Box<str>>,
    pub output_modalities_json: Option<Box<str>>,
    pub last_test_status: Option<i32>,
    pub target_format: TargetFormat,
    pub active: bool,
    pub custom: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertResult {
    pub touched: usize,
    pub new_model_ids: Box<[ModelId]>,
}
