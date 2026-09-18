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

impl Model {
    /// Returns the parsed compact typed model kind.
    pub fn kind(&self) -> ModelKind {
        ModelKind::parse_kind(&self.model_type)
    }

    /// Returns the compact bitflags for input modalities.
    pub fn input_modalities(&self) -> ModalityFlags {
        ModalityFlags::parse_from_json(self.input_modalities_json.as_deref())
    }

    /// Returns the compact bitflags for output modalities.
    pub fn output_modalities(&self) -> ModalityFlags {
        ModalityFlags::parse_from_json(self.output_modalities_json.as_deref())
    }

    /// Creates a lightweight summary projection for catalog listing.
    pub fn to_summary(&self) -> ModelSummary {
        ModelSummary {
            model_id: self.model_id.clone(),
            provider_id: self.provider_id.clone(),
            context_length: self.context_length,
            max_output_tokens: self.max_output_tokens,
            family: self.family.clone(),
            model_type: self.model_type.clone(),
            active: self.active,
        }
    }
}

/// Compact typed representation of model types (1 byte enum vs heap string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelKind {
    #[default]
    Chat,
    Image,
    Embedding,
    Audio,
    Rerank,
    Other,
}

impl ModelKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Image => "image",
            Self::Embedding => "embedding",
            Self::Audio => "audio",
            Self::Rerank => "rerank",
            Self::Other => "other",
        }
    }

    pub fn parse_kind(s: &str) -> Self {
        match s {
            "chat" => Self::Chat,
            "image" => Self::Image,
            "embedding" => Self::Embedding,
            "audio" => Self::Audio,
            "rerank" => Self::Rerank,
            _ => Self::Other,
        }
    }
}

impl std::str::FromStr for ModelKind {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(Self::parse_kind(s))
    }
}

/// Bitflag representation of input/output modalities (1 byte instead of JSON strings on heap).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct ModalityFlags(pub u8);

impl ModalityFlags {
    pub const NONE: u8 = 0;
    pub const TEXT: u8 = 1 << 0;
    pub const IMAGE: u8 = 1 << 1;
    pub const AUDIO: u8 = 1 << 2;
    pub const VIDEO: u8 = 1 << 3;

    pub fn new(flags: u8) -> Self {
        Self(flags)
    }

    pub fn has_text(&self) -> bool {
        self.0 & Self::TEXT != 0
    }

    pub fn has_image(&self) -> bool {
        self.0 & Self::IMAGE != 0
    }

    pub fn has_audio(&self) -> bool {
        self.0 & Self::AUDIO != 0
    }

    pub fn has_video(&self) -> bool {
        self.0 & Self::VIDEO != 0
    }

    pub fn parse_from_json(json_str: Option<&str>) -> Self {
        let Some(s) = json_str else {
            return Self(Self::TEXT);
        };
        let mut flags = 0u8;
        if s.contains("text") {
            flags |= Self::TEXT;
        }
        if s.contains("image") {
            flags |= Self::IMAGE;
        }
        if s.contains("audio") {
            flags |= Self::AUDIO;
        }
        if s.contains("video") {
            flags |= Self::VIDEO;
        }
        if flags == 0 {
            flags = Self::TEXT;
        }
        Self(flags)
    }

    pub fn to_json_string(&self) -> Box<str> {
        let mut parts = Vec::new();
        if self.has_text() {
            parts.push("\"text\"");
        }
        if self.has_image() {
            parts.push("\"image\"");
        }
        if self.has_audio() {
            parts.push("\"audio\"");
        }
        if self.has_video() {
            parts.push("\"video\"");
        }
        format!("[{}]", parts.join(",")).into_boxed_str()
    }
}

/// Compact in-memory projection of a Model for high-throughput catalog listing (/v1/models),
/// avoiding loading timestamps, override JSONs, and capability trees.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelSummary {
    pub model_id: ModelId,
    pub provider_id: ProviderId,
    pub context_length: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub family: Option<Box<str>>,
    pub model_type: Box<str>,
    pub active: bool,
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

    #[test]
    fn test_model_kind_and_modality_flags() {
        let mut m = Model {
            model_type: "chat".into(),
            ..Default::default()
        };
        assert_eq!(m.kind(), ModelKind::Chat);

        m.model_type = "embedding".into();
        assert_eq!(m.kind(), ModelKind::Embedding);

        m.input_modalities_json = Some("[\"text\", \"image\"]".into());
        let in_mod = m.input_modalities();
        assert!(in_mod.has_text());
        assert!(in_mod.has_image());
        assert!(!in_mod.has_audio());
        assert!(!in_mod.has_video());

        let json_str = in_mod.to_json_string();
        assert!(json_str.contains("\"text\""));
        assert!(json_str.contains("\"image\""));

        let summary = m.to_summary();
        assert_eq!(summary.model_type.as_ref(), "embedding");
    }

    #[test]
    fn test_model_serde_compatibility() {
        let m = Model {
            row_id: ModelRowId(42),
            provider_id: ProviderId::new("openai"),
            model_id: ModelId::new("gpt-4o"),
            display_name: Some("GPT-4o".into()),
            discovered_at: "2026-01-01T00:00:00Z".into(),
            expires_at: None,
            timeout_overrides_json: None,
            last_test_at: None,
            context_length: Some(128000),
            max_output_tokens: Some(4096),
            capabilities_json: Some("{\"reasoning\":true}".into()),
            family: Some("gpt".into()),
            model_type: "chat".into(),
            input_modalities_json: Some("[\"text\"]".into()),
            output_modalities_json: Some("[\"text\"]".into()),
            last_test_status: Some(200),
            target_format: TargetFormat::Openai,
            active: true,
            custom: false,
            manually_disabled_at: None,
        };

        let json = serde_json::to_string(&m).unwrap();
        let deserialized: Model = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.row_id.0, 42);
        assert_eq!(deserialized.provider_id.as_str(), "openai");
        assert_eq!(deserialized.model_id.as_str(), "gpt-4o");
        assert_eq!(deserialized.kind(), ModelKind::Chat);
    }
}
