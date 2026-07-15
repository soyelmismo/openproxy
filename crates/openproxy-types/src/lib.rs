pub mod config;
pub mod error;
pub mod ids;
pub mod message;
pub mod providers;
pub mod token_estimate;

pub mod quota;

pub mod capabilities;
pub mod accounts;
pub mod models;

pub use accounts::{Account, HealthStatus};
pub use models::Model;
pub mod notifications;
pub use notifications::{NotificationEvent, publish_notification};
pub mod usage;
pub use usage::{UsageInput, RecentUsageRow, publish_usage_row};
pub mod endpoint;
pub use endpoint::EndpointKind;

pub use ids::{
    AccountId, ApiKeyId, ComboId, ComboTargetId, ModelId, ModelRowId, ProviderId, RequestId,
    TraceId, UsageId,
};
pub use error::{CoreError, ErrorContext, Result, map_db_error, map_db_error_ctx};
pub use message::{OpenAIMessage, TargetFormat, OpenAIRequest, OpenAIRequestView};
pub use providers::{
    ProviderMetadata, builtin_provider_ids, is_builtin, DiscoveredModel, ProviderFormat,
    AuthType, RateLimitScope, Provider,
};
pub mod combos;
pub use combos::{Strategy, PriorityMode, Combo, ComboTarget, ComboTargetWithModel};
pub use config::{
    CircuitBreakerConfig, CooldownConfig, EncryptionKeySource, MaintenanceConfig,
    QuotaProtectionConfig, RacingConfig, RetriesConfig, ServerConfig, SmartWarmupConfig,
    StorageConfig, CompressionMode, IDLE_CHUNK_RETRYABLE_DEFAULT, CooldownMode, TimeoutsConfig,
};
pub use quota::{AccountQuota, ModelQuotaDetail, now_unix_secs_str};
pub use capabilities::{
    ModelCapabilities, infer_capabilities, infer_context_length, infer_family,
    infer_input_modalities, infer_input_modalities_json, infer_max_output_tokens,
    infer_model_type, infer_output_modalities, infer_output_modalities_json,
};
pub mod model_normalize;
pub use model_normalize::normalize_model_id;

pub mod selection_registry;
pub use selection_registry::SelectionRegistry;
