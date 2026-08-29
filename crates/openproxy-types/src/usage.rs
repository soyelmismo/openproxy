use crate::endpoint::EndpointKind;
use crate::ids::{AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId};
use serde::{Deserialize, Serialize};

// Usage boolean flags packed into u8
pub const USAGE_FLAG_RACE_LOST: u8 = 1 << 0;
pub const USAGE_FLAG_IS_STREAMING: u8 = 1 << 1;
pub const USAGE_FLAG_STREAM_COMPLETE: u8 = 1 << 2;
pub const USAGE_FLAG_CLIENT_RESPONSE: u8 = 1 << 3;
pub const USAGE_FLAG_PROMPT_ESTIMATED: u8 = 1 << 4;
pub const USAGE_FLAG_COMPLETION_ESTIMATED: u8 = 1 << 5;
pub const USAGE_FLAG_PROXY_ROTATED: u8 = 1 << 6;

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
    pub api_key_id: Option<ApiKeyId>,
    pub request_body_json: Option<bytes::Bytes>,
    pub response_body_json: Option<serde_json::Value>,
    pub request_headers: Option<std::collections::BTreeMap<String, String>>,
    pub response_headers: Option<std::collections::BTreeMap<String, String>>,
    pub error_message: Option<String>,
    pub race_attempts: u8,
    pub stop_reason: Option<String>,
    pub compression_savings_pct: Option<f64>,
    pub compression_techniques: Option<String>,
    pub endpoint_kind: EndpointKind,
    pub proxy_url: Option<String>,
    pub proxy_status: Option<String>,
    pub flags: u8,
}

impl UsageInput {
    #[inline]
    pub fn has_flag(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    #[inline]
    pub fn set_flag(&mut self, flag: u8) {
        self.flags |= flag;
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StageEvent {
    pub request_id: String,
    pub trace_id: String,
    pub stage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_model_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    pub elapsed_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_code: Option<u16>,
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
        $event.endpoint_kind =
            $crate::usage::IntoOptionEndpointKind::into_option_endpoint_kind($val);
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_kind: Option<EndpointKind>,
}

pub static STAGE_EVENT_PUBLISHER: std::sync::OnceLock<fn(StageEvent)> = std::sync::OnceLock::new();

pub fn publish_stage_event(event: StageEvent) {
    if let Some(publisher) = STAGE_EVENT_PUBLISHER.get() {
        publisher(event);
    }
}

#[derive(Debug, Clone)]
pub struct RecentUsageRow {
    pub request_id: String,
    pub trace_id: String,
    pub provider_id: ProviderId,
    pub upstream_model_id: String,
    pub created_at: String,
    pub error_message: Option<String>,
    pub stop_reason: Option<String>,
    pub compression_techniques: Option<String>,
    pub proxy_url: Option<String>,
    pub proxy_status: Option<String>,
    pub request_body_json: Option<serde_json::Value>,
    pub response_body_json: Option<serde_json::Value>,
    pub request_headers: Option<std::collections::BTreeMap<String, String>>,
    pub response_headers: Option<std::collections::BTreeMap<String, String>>,
    pub id: crate::ids::UsageId,
    pub total_ms: u64,
    pub cost_usd: Option<f64>,
    pub connect_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub compression_savings_pct: Option<f64>,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub cached_tokens: Option<u32>,
    pub status_code: u16,
    pub race_total: Option<u8>,
    pub race_attempts: Option<u8>,
    pub flags: u8,
    pub endpoint_kind: EndpointKind,
}

impl RecentUsageRow {
    #[inline]
    pub fn has_flag(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    #[inline]
    pub fn set_flag(&mut self, flag: u8) {
        self.flags |= flag;
    }
}

#[derive(Serialize, Deserialize)]
struct RecentUsageRowSerde {
    pub request_id: String,
    pub trace_id: String,
    pub provider_id: ProviderId,
    pub upstream_model_id: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub compression_techniques: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub proxy_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub proxy_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub request_body_json: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub response_body_json: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub request_headers: Option<std::collections::BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub response_headers: Option<std::collections::BTreeMap<String, String>>,
    pub id: crate::ids::UsageId,
    pub total_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub connect_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub compression_savings_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cached_tokens: Option<u32>,
    pub status_code: u16,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub race_total: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub race_attempts: Option<u8>,
    pub is_streaming: bool,
    pub stream_complete: bool,
    pub race_lost: bool,
    pub client_response: bool,
    pub prompt_tokens_estimated: bool,
    pub completion_tokens_estimated: bool,
    pub is_proxy_rotated: bool,
    pub endpoint_kind: EndpointKind,
}

#[derive(Serialize)]
struct RecentUsageRowSerdeRef<'a> {
    pub request_id: &'a str,
    pub trace_id: &'a str,
    pub provider_id: &'a ProviderId,
    pub upstream_model_id: &'a str,
    pub created_at: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<&'a String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<&'a String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression_techniques: Option<&'a String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_url: Option<&'a String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_status: Option<&'a String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body_json: Option<&'a serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_body_json: Option<&'a serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_headers: Option<&'a std::collections::BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_headers: Option<&'a std::collections::BTreeMap<String, String>>,
    pub id: crate::ids::UsageId,
    pub total_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connect_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compression_savings_pct: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_tokens: Option<u32>,
    pub status_code: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub race_total: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub race_attempts: Option<u8>,
    pub is_streaming: bool,
    pub stream_complete: bool,
    pub race_lost: bool,
    pub client_response: bool,
    pub prompt_tokens_estimated: bool,
    pub completion_tokens_estimated: bool,
    pub is_proxy_rotated: bool,
    pub endpoint_kind: EndpointKind,
}

impl Serialize for RecentUsageRow {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let shadow = RecentUsageRowSerdeRef {
            request_id: &self.request_id,
            trace_id: &self.trace_id,
            provider_id: &self.provider_id,
            upstream_model_id: &self.upstream_model_id,
            created_at: &self.created_at,
            error_message: self.error_message.as_ref(),
            stop_reason: self.stop_reason.as_ref(),
            compression_techniques: self.compression_techniques.as_ref(),
            proxy_url: self.proxy_url.as_ref(),
            proxy_status: self.proxy_status.as_ref(),
            request_body_json: self.request_body_json.as_ref(),
            response_body_json: self.response_body_json.as_ref(),
            request_headers: self.request_headers.as_ref(),
            response_headers: self.response_headers.as_ref(),
            id: self.id,
            total_ms: self.total_ms,
            cost_usd: self.cost_usd,
            connect_ms: self.connect_ms,
            ttft_ms: self.ttft_ms,
            compression_savings_pct: self.compression_savings_pct,
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            cached_tokens: self.cached_tokens,
            status_code: self.status_code,
            race_total: self.race_total,
            race_attempts: self.race_attempts,
            is_streaming: self.has_flag(USAGE_FLAG_IS_STREAMING),
            stream_complete: self.has_flag(USAGE_FLAG_STREAM_COMPLETE),
            race_lost: self.has_flag(USAGE_FLAG_RACE_LOST),
            client_response: self.has_flag(USAGE_FLAG_CLIENT_RESPONSE),
            prompt_tokens_estimated: self.has_flag(USAGE_FLAG_PROMPT_ESTIMATED),
            completion_tokens_estimated: self.has_flag(USAGE_FLAG_COMPLETION_ESTIMATED),
            is_proxy_rotated: self.has_flag(USAGE_FLAG_PROXY_ROTATED),
            endpoint_kind: self.endpoint_kind,
        };
        shadow.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for RecentUsageRow {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let shadow = RecentUsageRowSerde::deserialize(deserializer)?;
        let mut flags = 0u8;
        if shadow.race_lost {
            flags |= USAGE_FLAG_RACE_LOST;
        }
        if shadow.is_streaming {
            flags |= USAGE_FLAG_IS_STREAMING;
        }
        if shadow.stream_complete {
            flags |= USAGE_FLAG_STREAM_COMPLETE;
        }
        if shadow.client_response {
            flags |= USAGE_FLAG_CLIENT_RESPONSE;
        }
        if shadow.prompt_tokens_estimated {
            flags |= USAGE_FLAG_PROMPT_ESTIMATED;
        }
        if shadow.completion_tokens_estimated {
            flags |= USAGE_FLAG_COMPLETION_ESTIMATED;
        }
        if shadow.is_proxy_rotated {
            flags |= USAGE_FLAG_PROXY_ROTATED;
        }
        Ok(RecentUsageRow {
            request_id: shadow.request_id,
            trace_id: shadow.trace_id,
            provider_id: shadow.provider_id,
            upstream_model_id: shadow.upstream_model_id,
            created_at: shadow.created_at,
            error_message: shadow.error_message,
            stop_reason: shadow.stop_reason,
            compression_techniques: shadow.compression_techniques,
            proxy_url: shadow.proxy_url,
            proxy_status: shadow.proxy_status,
            request_body_json: shadow.request_body_json,
            response_body_json: shadow.response_body_json,
            request_headers: shadow.request_headers,
            response_headers: shadow.response_headers,
            id: shadow.id,
            total_ms: shadow.total_ms,
            cost_usd: shadow.cost_usd,
            connect_ms: shadow.connect_ms,
            ttft_ms: shadow.ttft_ms,
            compression_savings_pct: shadow.compression_savings_pct,
            prompt_tokens: shadow.prompt_tokens,
            completion_tokens: shadow.completion_tokens,
            cached_tokens: shadow.cached_tokens,
            status_code: shadow.status_code,
            race_total: shadow.race_total,
            race_attempts: shadow.race_attempts,
            flags,
            endpoint_kind: shadow.endpoint_kind,
        })
    }
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

    #[test]
    fn test_usage_input_flags() {
        let mut input = UsageInput {
            request_id: RequestId::new(),
            trace_id: "trace-1".to_string(),
            attempt: 1,
            provider_id: ProviderId::new("openai"),
            account_id: None,
            combo_id: None,
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id: "gpt-4o".to_string(),
            prompt_tokens: None,
            completion_tokens: None,
            cached_tokens: None,
            connect_ms: None,
            ttft_ms: None,
            total_ms: 100,
            status_code: 200,
            error_msg: None,
            race_total: 1,
            api_key_id: None,
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            error_message: None,
            race_attempts: 1,
            stop_reason: None,
            compression_savings_pct: None,
            compression_techniques: None,
            endpoint_kind: EndpointKind::Chat,
            proxy_url: None,
            proxy_status: None,
            flags: 0,
        };

        assert!(!input.has_flag(USAGE_FLAG_IS_STREAMING));
        assert!(!input.has_flag(USAGE_FLAG_CLIENT_RESPONSE));
        input.set_flag(USAGE_FLAG_IS_STREAMING);
        input.set_flag(USAGE_FLAG_CLIENT_RESPONSE);
        assert!(input.has_flag(USAGE_FLAG_IS_STREAMING));
        assert!(input.has_flag(USAGE_FLAG_CLIENT_RESPONSE));
        assert!(!input.has_flag(USAGE_FLAG_RACE_LOST));
    }

    #[test]
    fn test_recent_usage_row_flags_and_serde() {
        let row = RecentUsageRow {
            request_id: "req-1".to_string(),
            trace_id: "trace-1".to_string(),
            provider_id: ProviderId::new("openai"),
            upstream_model_id: "gpt-4o".to_string(),
            created_at: "2026-08-28T00:00:00Z".to_string(),
            error_message: None,
            stop_reason: None,
            compression_techniques: None,
            proxy_url: None,
            proxy_status: None,
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            id: crate::ids::UsageId(42),
            total_ms: 250,
            cost_usd: Some(0.005),
            connect_ms: None,
            ttft_ms: None,
            compression_savings_pct: None,
            prompt_tokens: Some(100),
            completion_tokens: Some(50),
            cached_tokens: None,
            status_code: 200,
            race_total: Some(2),
            race_attempts: Some(2),
            flags: USAGE_FLAG_IS_STREAMING | USAGE_FLAG_CLIENT_RESPONSE | USAGE_FLAG_STREAM_COMPLETE,
            endpoint_kind: EndpointKind::Chat,
        };

        assert!(row.has_flag(USAGE_FLAG_IS_STREAMING));
        assert!(row.has_flag(USAGE_FLAG_CLIENT_RESPONSE));
        assert!(row.has_flag(USAGE_FLAG_STREAM_COMPLETE));
        assert!(!row.has_flag(USAGE_FLAG_RACE_LOST));

        let json = serde_json::to_string(&row).expect("serialize");
        assert!(json.contains("\"is_streaming\":true"));
        assert!(json.contains("\"client_response\":true"));
        assert!(json.contains("\"stream_complete\":true"));
        assert!(json.contains("\"race_lost\":false"));

        let deserialized: RecentUsageRow = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(deserialized.flags, row.flags);
        assert_eq!(deserialized.id, crate::ids::UsageId(42));
    }
}
