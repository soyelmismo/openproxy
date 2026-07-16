#[cfg(test)]
mod tests {
    use crate::usage::analytics::RecentUsageRow;
    use crate::usage::recording::redact_for_broadcast;
    use crate::ids::{ProviderId, UsageId};
    use crate::endpoint::EndpointKind;

    #[test]
    fn test_redact_for_broadcast() {
        let row = RecentUsageRow {
            id: UsageId(1),
            request_id: "req".to_string(),
            trace_id: "trace".to_string(),
            provider_id: ProviderId::new("test"),
            upstream_model_id: "test".to_string(),
            status_code: 200,
            total_ms: 100,
            prompt_tokens: None,
            completion_tokens: None,
            cost_usd: None,
            connect_ms: None,
            ttft_ms: None,
            request_body_json: Some(serde_json::Value::String("foo".to_string())),
            response_body_json: Some(serde_json::Value::String("bar".to_string())),
            request_headers: Some(std::collections::BTreeMap::new()),
            response_headers: Some(std::collections::BTreeMap::new()),
            error_message: None,
            race_total: None,
            race_attempts: None,
            is_streaming: false,
            stream_complete: false,
            race_lost: false,
            stop_reason: None,
            compression_savings_pct: None,
            compression_techniques: None,
            proxy_url: None,
            client_response: false,
            prompt_tokens_estimated: false,
            completion_tokens_estimated: false,
            proxy_status: None,
            is_proxy_rotated: false,
            endpoint_kind: EndpointKind::Chat,
            created_at: chrono::Utc::now().to_string(),
        };

        let redacted = redact_for_broadcast(row);

        assert_eq!(redacted.request_body_json, None);
        assert_eq!(redacted.response_body_json, None);
        assert_eq!(redacted.request_headers, None);
        assert_eq!(redacted.response_headers, None);
    }
}
