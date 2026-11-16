//! Cost calculation — canonical implementation re-exported from `openproxy_db::cost`.

pub use openproxy_db::cost::*;


#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ProviderId, RequestId, TraceId};

    fn make_input() -> UsageInput {
        UsageInput {
            proxy_url: None,
            proxy_status: None,
            request_id: RequestId::new(),
            trace_id: TraceId::new().to_string(),
            attempt: 1,
            provider_id: ProviderId::new("openrouter"),
            account_id: None,
            combo_id: None,
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id: "openai/gpt-4o".to_string(),
            prompt_tokens: Some(1000),
            completion_tokens: Some(500),
            cached_tokens: None,
            connect_ms: Some(100),
            ttft_ms: Some(200),
            total_ms: 1200,
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
            pii_redacted: None,
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
            flags: USAGE_FLAG_IS_STREAMING,
            audio_ms: None,
            session_hash: None,
            max_chunk_gap_ms: None,
            avg_chunk_gap_ms: None,
        }
    }

    #[test]
    fn compute_cost_zero_tokens() {
        let input = UsageInput {
            prompt_tokens: Some(0),
            completion_tokens: Some(0),
            total_ms: 500,
            ttft_ms: Some(100),
            ..make_input()
        };
        let price = pricing::lookup("openrouter", "openai/gpt-4o");
        let (cost, tps) = compute(price, &input);
        assert_eq!(cost, 0.0);
        // TPS guard: completion=0 => None
        assert!(tps.is_none());
    }

    #[test]
    fn compute_cost_known_pricing() {
        let input = UsageInput {
            prompt_tokens: Some(1_000_000),
            completion_tokens: Some(1_000_000),
            total_ms: 10_000,
            ttft_ms: Some(1_000),
            ..make_input()
        };
        let price = pricing::lookup("openrouter", "openai/gpt-4o").unwrap();
        // 2.5 * 1e6 / 1e6 + 10.0 * 1e6 / 1e6 = 12.5
        let (cost, _tps) = compute(Some(price), &input);
        assert!((cost - 12.5).abs() < 1e-9);
    }

    #[test]
    fn compute_tokens_per_sec_normal() {
        let input = UsageInput {
            prompt_tokens: Some(100),
            completion_tokens: Some(300),
            total_ms: 1500,
            ..make_input()
        };
        let price = pricing::lookup("openrouter", "openai/gpt-4o");
        let (_cost, tps) = compute(price, &input);
        // 300 * 1000 / 1500 = 200.0
        assert_eq!(tps, Some(200.0));
    }

    #[test]
    fn compute_tokens_per_sec_zero_completion_is_none() {
        let input = UsageInput {
            completion_tokens: Some(0),
            total_ms: 1500,
            ..make_input()
        };
        let (_cost, tps) = compute(None, &input);
        assert!(tps.is_none());
    }

    #[test]
    fn compute_tokens_per_sec_zero_total_is_none() {
        let input = UsageInput {
            completion_tokens: Some(300),
            total_ms: 0,
            ..make_input()
        };
        let (_cost, tps) = compute(None, &input);
        assert!(tps.is_none());
    }

    #[test]
    fn compute_tokens_per_sec_non_streaming_works() {
        let input = UsageInput {
            completion_tokens: Some(300),
            total_ms: 1500,
            flags: 0,
            ..make_input()
        };
        let (_cost, tps) = compute(None, &input);
        assert_eq!(tps, Some(200.0));
    }

    #[test]
    fn compute_tokens_per_sec_unknown_pricing_is_some_zero() {
        // price = None => cost = 0.0, but tps depends only on tokens/timings.
        let input = UsageInput {
            upstream_model_id: "no/such-model".to_string(),
            prompt_tokens: Some(100),
            completion_tokens: Some(200),
            total_ms: 1000,
            connect_ms: Some(0),
            ..make_input()
        };
        let (cost, tps) = compute(None, &input);
        assert_eq!(cost, 0.0);
        // effective = 1000 - 0 = 1000; 200 * 1000 / 1000 = 200.0
        assert_eq!(tps, Some(200.0));
    }

    #[test]
    fn redact_sk_key() {
        let raw = "error: sk-abcdefghij1234567890 is invalid";
        let (sanitized, redacted) = redact_error_msg(raw);
        assert!(sanitized.contains("sk-[REDACTED]"));
        assert!(!sanitized.contains("abcdefghij1234567890"));
        assert_eq!(sanitized, redacted);
    }

    #[test]
    fn redact_x_api_key_header() {
        let raw = "auth failed: X-API-Key: abc123secret";
        let (sanitized, _redacted) = redact_error_msg(raw);
        assert!(sanitized.contains("x-api-key: [REDACTED]"));
        assert!(!sanitized.contains("abc123secret"));
    }

    #[test]
    fn redact_bearer_token() {
        let raw = "upstream error: Authorization: Bearer tok_xyz123";
        let (sanitized, _redacted) = redact_error_msg(raw);
        assert!(sanitized.contains("Authorization: Bearer [REDACTED]"));
        assert!(!sanitized.contains("tok_xyz123"));
    }

    #[test]
    fn redact_caps_at_2kb() {
        let raw = "x".repeat(3000);
        let (sanitized, _redacted) = redact_error_msg(&raw);
        // 2048 'x' + "...[truncated]" = 2061
        assert!(sanitized.ends_with("...[truncated]"));
        assert!(sanitized.len() <= 2048 + "...[truncated]".len());
    }

    #[test]
    fn redact_caps_at_2kb_multibyte_utf8() {
        // Construct a string where character boundary at index 2048 falls inside a multi-byte UTF-8 character (e.g. 🦀 = 4 bytes)
        let prefix = "a".repeat(2047);
        let raw = format!("{prefix}🦀🦀🦀🦀");
        let (sanitized, _redacted) = redact_error_msg(&raw);
        assert!(sanitized.ends_with("...[truncated]"));
        assert!(sanitized.contains("...[truncated]"));
        // Ensure no panic happened and string is valid UTF-8
        assert!(std::str::from_utf8(sanitized.as_bytes()).is_ok());
    }

    // The live-logs WebSocket terminal event goes through redact_error_msg, so a
    // new secret format needs an assertion here to keep WS and DB rows in sync.

    #[test]
    fn redact_matches_db_row_for_all_common_secret_formats() {
        // DB row and WS payload both go through redact_error_msg, so they agree
        // iff the same input masks identically.
        let secret_input = "upstream error: sk-abcdefghij1234567890 \
                             x-api-key: topsecret \
                             Authorization: Bearer ya31.aaa.bbb";
        let (db_form, _redacted) = redact_error_msg(secret_input);
        let ws_form = redact_error_msg(secret_input).0;
        assert_eq!(
            db_form, ws_form,
            "DB row and WS publish must render the same redacted form"
        );
        assert!(db_form.contains("sk-[REDACTED]"));
        assert!(db_form.contains("x-api-key: [REDACTED]"));
        assert!(db_form.contains("Authorization: Bearer [REDACTED]"));
        assert!(!db_form.contains("abcdefghij1234567890"));
        assert!(!db_form.contains("topsecret"));
        assert!(!db_form.contains("ya31.aaa.bbb"));
    }

    #[test]
    fn compute_returns_zero_cost_and_none_tps_on_error_status_code() {
        let input = UsageInput {
            status_code: 500,
            prompt_tokens: Some(1_000_000),
            completion_tokens: Some(1_000_000),
            total_ms: 10_000,
            ttft_ms: Some(1_000),
            ..make_input()
        };
        let price = pricing::lookup("openrouter", "openai/gpt-4o");
        let (cost, tps) = compute(price, &input);
        assert_eq!(cost, 0.0);
        assert!(tps.is_none());
    }

    #[test]
    fn record_and_record_with_retry_inserts_row() {
        let conn = openproxy_db::testing::open_in_memory();
        let input = make_input();

        let id1 = record(&conn, &input).expect("record should succeed");
        assert!(id1.0 > 0);

        let id2 = record_with_retry(&conn, &input).expect("record_with_retry should succeed");
        assert!(id2.0 > id1.0);

        let count: i64 = conn
            .query_row("SELECT count(*) FROM usage", [], |r| r.get(0))
            .expect("query usage count");
        assert_eq!(count, 2);
    }
}
