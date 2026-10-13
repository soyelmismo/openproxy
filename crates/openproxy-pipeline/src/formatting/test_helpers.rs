use super::*;
use openproxy_types::models::Model;
use openproxy_types::{ModelId, ModelRowId, OpenAIRequest, ProviderId, TargetFormat};
use std::sync::Arc;

pub(crate) fn test_req(openai_req: OpenAIRequest) -> PipelineRequest {
    PipelineRequest {
        request_id: openproxy_types::RequestId::new(),
        trace_id: openproxy_types::TraceId::new(),
        combo_id: openproxy_types::ComboId(1),
        openai_request: Arc::new(openai_req),
        client_disconnected: tokio::sync::watch::channel(None).1,
        stream_sink: None,
        api_key_id: None,
        combo_override: None,
        targets_override: None,
        request_headers: Default::default(),
        request_body_json: None,
        race_cancelled: false,
        race_cancel: None,
        endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
        compressed_messages: Arc::new(std::sync::OnceLock::new()),
        pii_session: Arc::new(parking_lot::Mutex::new(None)),
        compression_stats: Arc::new(parking_lot::Mutex::new(None)),
        proxy_override: None,
    }
}

pub(crate) fn test_model() -> Model {
    Model {
        row_id: ModelRowId(1),
        provider_id: ProviderId::new("nvidia-nim"),
        model_id: ModelId::new("deepseek-ai/deepseek-v4-flash-0731"),
        display_name: Some("test".into()),
        target_format: TargetFormat::Openai,
        discovered_at: "2026-01-01T00:00:00Z".into(),
        active: true,
        model_type: "chat".into(),
        ..Default::default()
    }
}
