use crate::endpoint::EndpointKind;
use crate::ids::{AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId};
use serde::{Deserialize, Serialize};

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
    pub cached_tokens: Option<u32>,
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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StageEvent {
    pub request_id: String,
    pub trace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_model_id: Option<String>,
    pub stage: String,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_kind: Option<EndpointKind>,
}

#[doc(hidden)]
pub trait IntoOptionString {
    fn into_option_string(self) -> Option<String>;
}

impl IntoOptionString for String {
    #[inline]
    fn into_option_string(self) -> Option<String> {
        Some(self)
    }
}
impl IntoOptionString for &str {
    #[inline]
    fn into_option_string(self) -> Option<String> {
        Some(self.to_string())
    }
}
impl IntoOptionString for Option<String> {
    #[inline]
    fn into_option_string(self) -> Option<String> {
        self
    }
}
impl IntoOptionString for Option<&str> {
    #[inline]
    fn into_option_string(self) -> Option<String> {
        self.map(ToString::to_string)
    }
}

impl IntoOptionString for crate::ids::ProviderId {
    #[inline]
    fn into_option_string(self) -> Option<String> {
        Some(self.0)
    }
}
impl IntoOptionString for &crate::ids::ProviderId {
    #[inline]
    fn into_option_string(self) -> Option<String> {
        Some(self.0.clone())
    }
}
impl IntoOptionString for crate::ids::ModelId {
    #[inline]
    fn into_option_string(self) -> Option<String> {
        Some(self.0)
    }
}
impl IntoOptionString for &crate::ids::ModelId {
    #[inline]
    fn into_option_string(self) -> Option<String> {
        Some(self.0.clone())
    }
}

#[doc(hidden)]
pub trait IntoOptionU64 {
    fn into_option_u64(self) -> Option<u64>;
}

impl IntoOptionU64 for u64 {
    #[inline]
    fn into_option_u64(self) -> Option<u64> {
        Some(self)
    }
}
impl IntoOptionU64 for Option<u64> {
    #[inline]
    fn into_option_u64(self) -> Option<u64> {
        self
    }
}

#[doc(hidden)]
pub trait IntoOptionU16 {
    fn into_option_u16(self) -> Option<u16>;
}

impl IntoOptionU16 for u16 {
    #[inline]
    fn into_option_u16(self) -> Option<u16> {
        Some(self)
    }
}
impl IntoOptionU16 for Option<u16> {
    #[inline]
    fn into_option_u16(self) -> Option<u16> {
        self
    }
}

#[doc(hidden)]
pub trait IntoOptionEndpointKind {
    fn into_option_endpoint_kind(self) -> Option<crate::endpoint::EndpointKind>;
}

impl IntoOptionEndpointKind for crate::endpoint::EndpointKind {
    #[inline]
    fn into_option_endpoint_kind(self) -> Option<crate::endpoint::EndpointKind> {
        Some(self)
    }
}
impl IntoOptionEndpointKind for Option<crate::endpoint::EndpointKind> {
    #[inline]
    fn into_option_endpoint_kind(self) -> Option<crate::endpoint::EndpointKind> {
        self
    }
}

#[macro_export]
#[doc(hidden)]
macro_rules! __set_stage_event_field {
    ($event:ident, provider_id, $val:expr) => {
        $event.provider_id = $crate::usage::IntoOptionString::into_option_string($val);
    };
    ($event:ident, upstream_model_id, $val:expr) => {
        $event.upstream_model_id = $crate::usage::IntoOptionString::into_option_string($val);
    };
    ($event:ident, connect_ms, $val:expr) => {
        $event.connect_ms = $crate::usage::IntoOptionU64::into_option_u64($val);
    };
    ($event:ident, ttft_ms, $val:expr) => {
        $event.ttft_ms = $crate::usage::IntoOptionU64::into_option_u64($val);
    };
    ($event:ident, status_code, $val:expr) => {
        $event.status_code = $crate::usage::IntoOptionU16::into_option_u16($val);
    };
    ($event:ident, error, $val:expr) => {
        $event.error = $crate::usage::IntoOptionString::into_option_string($val);
    };
    ($event:ident, stop_reason, $val:expr) => {
        $event.stop_reason = $crate::usage::IntoOptionString::into_option_string($val);
    };
    ($event:ident, timestamp, $val:expr) => {
        $event.timestamp = $crate::usage::IntoOptionString::into_option_string($val);
    };
    ($event:ident, endpoint_kind, $val:expr) => {
        $event.endpoint_kind = $crate::usage::IntoOptionEndpointKind::into_option_endpoint_kind($val);
    };
}

/// Constructs a `StageEvent` struct with default `None` for omitted optional fields.
#[macro_export]
macro_rules! stage_event {
    (
        request_id: $req_id:expr,
        trace_id: $trace_id:expr,
        stage: $stage:expr,
        elapsed_ms: $elapsed:expr
        $(, $field:ident : $val:expr)*
        $(,)?
    ) => {{
        #[allow(unused_mut)]
        let mut event = $crate::usage::StageEvent {
            request_id: $req_id.to_string(),
            trace_id: $trace_id.to_string(),
            stage: $stage.to_string(),
            elapsed_ms: $elapsed,
            ..Default::default()
        };
        $(
            $crate::__set_stage_event_field!(event, $field, $val);
        )*
        event
    }};
}

/// Constructs and publishes a `StageEvent` to the stage broadcast channel.
#[macro_export]
macro_rules! emit_stage_event {
    ($($tt:tt)*) => {
        $crate::usage::publish_stage_event($crate::stage_event!($($tt)*))
    };
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InflightAttempt {
    pub attempt_key: String,
    pub request_id: String,
    pub trace_id: String,
    pub provider_id: String,
    pub upstream_model_id: String,
    pub started_at_ms: u64,
    pub updated_at_ms: u64,
    pub stage: String,
    pub stage_seq: u32,
    pub stage_rank: u8,
    pub elapsed_ms_at_event: u64,
    pub connect_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub status_code: Option<u16>,
    pub terminal: bool,
    pub terminal_kind: Option<String>,
    pub error: Option<String>,
    pub row_id: Option<i64>,
    pub source: String,
}

pub static STAGE_EVENT_PUBLISHER: std::sync::OnceLock<fn(StageEvent)> = std::sync::OnceLock::new();

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body_json: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body_json: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub race_total: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub race_attempts: Option<u8>,
    pub is_streaming: bool,
    pub stream_complete: bool,
    pub race_lost: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression_savings_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression_techniques: Option<String>,
    pub client_response: bool,
    pub prompt_tokens_estimated: bool,
    pub completion_tokens_estimated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_status: Option<String>,
    pub is_proxy_rotated: bool,
    pub endpoint_kind: EndpointKind,
    pub created_at: String,
}

pub static USAGE_ROW_PUBLISHER: std::sync::OnceLock<fn(RecentUsageRow)> =
    std::sync::OnceLock::new();

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stage_event_minimal() {
        let ev = stage_event!(
            request_id: "req-1",
            trace_id: "trace-1",
            stage: "started",
            elapsed_ms: 10,
        );
        assert_eq!(ev.request_id, "req-1");
        assert_eq!(ev.trace_id, "trace-1");
        assert_eq!(ev.stage, "started");
        assert_eq!(ev.elapsed_ms, 10);
        assert!(ev.provider_id.is_none());
        assert!(ev.connect_ms.is_none());
        assert!(ev.ttft_ms.is_none());
        assert!(ev.status_code.is_none());
        assert!(ev.error.is_none());
    }

    #[test]
    fn test_stage_event_full_fields() {
        let provider = crate::ids::ProviderId::new("openai");
        let model = crate::ids::ModelId::new("gpt-4o");
        let ev = stage_event!(
            request_id: "req-2",
            trace_id: "trace-2",
            stage: "completed",
            elapsed_ms: 250,
            provider_id: &provider,
            upstream_model_id: &model,
            connect_ms: 45,
            ttft_ms: 120,
            status_code: 200,
            error: None::<String>,
            stop_reason: "stop",
            timestamp: "2026-08-18T00:00:00Z",
            endpoint_kind: EndpointKind::Chat,
        );
        assert_eq!(ev.request_id, "req-2");
        assert_eq!(ev.provider_id.as_deref(), Some("openai"));
        assert_eq!(ev.upstream_model_id.as_deref(), Some("gpt-4o"));
        assert_eq!(ev.connect_ms, Some(45));
        assert_eq!(ev.ttft_ms, Some(120));
        assert_eq!(ev.status_code, Some(200));
        assert_eq!(ev.stop_reason.as_deref(), Some("stop"));
        assert_eq!(ev.endpoint_kind, Some(EndpointKind::Chat));
    }
}
