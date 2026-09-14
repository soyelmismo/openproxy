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
    /// Timestamp (UTC, ISO-ish) of the most recent operator-driven
    /// `set_active(id, false)`. `None` ⇒ the row is eligible for
    /// `apply_auto_activation` on the next refresh. See migration 000064.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub manually_disabled_at: Option<Box<str>>,
}

impl Default for Model {
    fn default() -> Self {
        Self {
            row_id: ModelRowId(0),
            provider_id: ProviderId(String::new()),
            model_id: ModelId(String::new()),
            display_name: None,
            discovered_at: Box::<str>::default(),
            expires_at: None,
            timeout_overrides_json: None,
            last_test_at: None,
            context_length: None,
            max_output_tokens: None,
            capabilities_json: None,
            family: None,
            model_type: Box::<str>::default(),
            input_modalities_json: None,
            output_modalities_json: None,
            last_test_status: None,
            target_format: TargetFormat::Openai,
            active: false,
            custom: false,
            manually_disabled_at: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertResult {
    pub touched: usize,
    pub new_model_ids: Box<[ModelId]>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelsRefreshedEvent {
    pub provider_id: ProviderId,
    pub models_refreshed: usize,
    pub new_model_ids: Vec<String>,
    pub models_activated: u64,
}

pub static MODELS_REFRESHED_PUBLISHER: std::sync::OnceLock<
    Box<dyn Fn(ModelsRefreshedEvent) + Send + Sync>,
> = std::sync::OnceLock::new();

pub fn publish_models_refreshed(event: ModelsRefreshedEvent) {
    if let Some(publisher) = MODELS_REFRESHED_PUBLISHER.get() {
        publisher(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn test_publish_models_refreshed() {
        let called = Arc::new(AtomicBool::new(false));
        let called_clone = Arc::clone(&called);

        let _ = MODELS_REFRESHED_PUBLISHER.set(Box::new(move |event: ModelsRefreshedEvent| {
            called_clone.store(true, Ordering::SeqCst);
            assert_eq!(event.provider_id.as_str(), "test-provider");
            assert_eq!(event.models_refreshed, 5);
            assert_eq!(event.new_model_ids, vec!["m1".to_string()]);
            assert_eq!(event.models_activated, 2);
        }));

        let event = ModelsRefreshedEvent {
            provider_id: ProviderId::new("test-provider"),
            models_refreshed: 5,
            new_model_ids: vec!["m1".to_string()],
            models_activated: 2,
        };

        publish_models_refreshed(event);
        assert!(called.load(Ordering::SeqCst));
    }
}
