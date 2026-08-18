#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

#[macro_use]
pub mod macros;
pub use macros::FromDb;
pub mod time;
pub use time::now_ms;

pub mod config;
pub mod error;
pub mod ids;
pub mod message;
pub mod providers;

pub mod quota;

pub mod accounts;
pub mod capabilities;
pub mod models;

pub use accounts::{Account, HealthStatus};
pub use models::{Model, UpsertResult};
pub mod notifications;
pub use notifications::{NotificationEvent, publish_notification};
pub mod usage;
pub use usage::{RecentUsageRow, StageEvent, UsageInput, publish_stage_event, publish_usage_row};
pub mod endpoint;
pub use endpoint::EndpointKind;
pub mod images;
pub use images::{ImageData, ImageEditRequest, ImageGenerationRequest, ImageGenerationResponse};
pub mod embeddings;
pub use embeddings::{
    EmbeddingInput, EmbeddingObject, EmbeddingRequest, EmbeddingResponse, EmbeddingUsage,
    EmbeddingVector,
};

pub use error::{CancelReason, CoreError, ErrorContext, Result};
pub use ids::{
    AccountId, ApiKeyId, ComboId, ComboTargetId, ModelId, ModelRowId, ProviderId, RequestId,
    TraceId, UsageId,
};
pub use message::{
    OpenAIChoice, OpenAIMessage, OpenAIRequest, OpenAIRequestView, OpenAIResponse, OpenAIUsage,
    TargetFormat, extract_content_part_text, extract_content_text,
};
pub use providers::{
    AuthType, DiscoveredModel, Provider, ProviderFormat, ProviderMetadata, RateLimitScope,
};
pub mod combos;
pub use capabilities::{
    ModelCapabilities, find_substring_match, infer_capabilities, infer_context_length,
    infer_default_output_modalities, infer_family, infer_input_modalities,
    infer_input_modalities_for_model, infer_input_modalities_json, infer_max_output_tokens,
    infer_model_type, infer_output_modalities, infer_output_modalities_json,
};
pub use combos::{
    Combo, ComboTarget, ComboTargetWithModel, MAX_SUB_COMBO_DEPTH, PriorityMode, Strategy,
};
pub use config::{
    CircuitBreakerConfig, CompressionMode, CooldownConfig, CooldownMode, EncryptionKeySource,
    IDLE_CHUNK_RETRYABLE_DEFAULT, MaintenanceConfig, QuotaProtectionConfig, RacingConfig,
    RetriesConfig, ServerConfig, SmartWarmupConfig, StorageConfig, TimeoutsConfig,
};
pub use quota::{AccountQuota, ModelQuotaDetail, now_unix_secs_str};
pub mod model_normalize;
pub use model_normalize::{candidate_normalized_forms, normalize_model_id};

pub mod selection_registry;
pub use selection_registry::SelectionRegistry;
pub mod context;
pub use context::{CustomProviderMeta, ResolvedTarget};
