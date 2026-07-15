use serde::{Deserialize, Serialize};
use crate::ids::{AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId};
use crate::endpoint::EndpointKind;

#[derive(Debug, Clone)]
pub struct UsageInput {
    pub request_id: RequestId,
    pub trace_id: String,
    pub attempt: u8,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub combo_id: Option<ComboId>,
    pub combo_target_id: Option<ComboTargetId>,
    pub model_row_id: Option<ModelRowId>,
    pub upstream_model_id: String,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub connect_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
    pub status_code: u16,
    pub error_msg: Option<String>,
    pub race_total: u8,
    pub race_lost: bool,
    pub api_key_id: Option<ApiKeyId>,
    pub request_body_json: Option<bytes::Bytes>,
    pub response_body_json: Option<serde_json::Value>,
    pub request_headers: Option<std::collections::BTreeMap<String, String>>,
    pub response_headers: Option<std::collections::BTreeMap<String, String>>,
    pub error_message: Option<String>,
    pub race_attempts: u8,
    pub is_streaming: bool,
    pub stream_complete: bool,
    pub stop_reason: Option<String>,
    pub compression_savings_pct: Option<f64>,
    pub compression_techniques: Option<String>,
    pub client_response: bool,
    pub prompt_tokens_estimated: bool,
    pub completion_tokens_estimated: bool,
    pub endpoint_kind: EndpointKind,
    pub proxy_url: Option<String>,
    pub proxy_status: Option<String>,
    pub is_proxy_rotated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StageEvent {
    pub request_id: String,
    pub trace_id: String,
    pub provider_id: String,
    pub upstream_model_id: String,
    pub stage: String,
    pub elapsed_ms: u64,
    pub connect_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub status_code: u16,
    pub error: Option<String>,
    pub stop_reason: Option<String>,
    pub compression_savings_pct: Option<f64>,
    pub compression_techniques: Option<String>,
    pub timestamp: String,
    pub endpoint_kind: EndpointKind,
}

pub static STAGE_EVENT_PUBLISHER: once_cell::sync::OnceCell<fn(StageEvent)> = once_cell::sync::OnceCell::new();

pub fn publish_stage_event(event: StageEvent) {
    if let Some(publisher) = STAGE_EVENT_PUBLISHER.get() {
        publisher(event);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentUsageRow {
    pub id: crate::ids::UsageId,
    pub request_id: String,
    pub trace_id: String,
    pub provider_id: ProviderId,
    pub upstream_model_id: String,
    pub status_code: u16,
    pub total_ms: u64,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub cost_usd: Option<f64>,
    pub connect_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub request_body_json: Option<serde_json::Value>,
    pub response_body_json: Option<serde_json::Value>,
    pub request_headers: Option<std::collections::BTreeMap<String, String>>,
    pub response_headers: Option<std::collections::BTreeMap<String, String>>,
    pub error_message: Option<String>,
    pub race_total: Option<u8>,
    pub race_attempts: Option<u8>,
    pub is_streaming: bool,
    pub stream_complete: bool,
    pub race_lost: bool,
    pub stop_reason: Option<String>,
    pub compression_savings_pct: Option<f64>,
    pub compression_techniques: Option<String>,
    pub client_response: bool,
    pub prompt_tokens_estimated: bool,
    pub completion_tokens_estimated: bool,
    pub proxy_url: Option<String>,
    pub proxy_status: Option<String>,
    pub is_proxy_rotated: bool,
    pub endpoint_kind: EndpointKind,
    pub created_at: String,
}

pub static USAGE_ROW_PUBLISHER: once_cell::sync::OnceCell<fn(RecentUsageRow)> = once_cell::sync::OnceCell::new();

pub fn publish_usage_row(row: RecentUsageRow) {
    if let Some(publisher) = USAGE_ROW_PUBLISHER.get() {
        publisher(row);
    }
}

pub fn redact_for_broadcast(mut row: RecentUsageRow) -> RecentUsageRow {
    row.request_body_json = None;
    row.response_body_json = None;
    row.request_headers = None;
    row.response_headers = None;
    row
}
