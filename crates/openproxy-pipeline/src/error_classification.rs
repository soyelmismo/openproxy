//! Classify upstream error bodies into account fault vs. request fault, so the
//! circuit breaker does not trip on request-shaped errors.
//!
//! Rationale in `docs/specs/antigravity-gaps-p2.md` §2. The enum lives in
//! `openproxy-types` so `CoreError::UpstreamError` can embed it; this module
//! holds the classification policy.

pub use openproxy_types::UpstreamErrorClass;

use openproxy_types::CoreError;

#[must_use]
pub fn classify_upstream_error(status: u16, body: &str) -> UpstreamErrorClass {
    // CodeBuddy business error codes take priority over the text markers.
    if let Some(code) = openproxy_adapters::adapters::codebuddy::parse_codebuddy_error_code(body)
        && let Some(cb_err) =
            openproxy_adapters::adapters::codebuddy::CodeBuddyErrorCode::from_code(code)
    {
        let class = cb_err.to_upstream_error_class();
        if class != UpstreamErrorClass::Generic {
            return class;
        }
    }

    if body.contains("UsageLimitEnterpriseExhausted")
        || body.contains("UsageLimitUserExhausted")
        || body.contains("UsageLimitExceeded")
    {
        return UpstreamErrorClass::ResourceExhausted;
    }
    if body.contains("UsageLimitLicenseExpired")
        || body.contains("UsageLimitEnterpriseNotActivated")
        || body.contains("UsageLimitUserNotActivated")
    {
        return UpstreamErrorClass::PermissionDenied;
    }

    if status == 400 {
        if body.contains("2013") || body.contains("function name or parameters is empty") {
            return UpstreamErrorClass::MalformedToolCall;
        }
        let lower = body.to_ascii_lowercase();
        if lower.contains("model is unavailable")
            || lower.contains("upstream request failed")
            || lower.contains("is not supported")
        {
            return UpstreamErrorClass::ResourceExhausted;
        }
        if !body.is_empty()
            && (lower.contains("base64")
                || lower.contains("invalid value")
                || lower.contains("invalid_request_error")
                || lower.contains("invalid_argument")
                || lower.contains("malformed")
                || lower.contains("decoding failed")
                || lower.contains("input required")
                || lower.contains("extra data:")
                || lower.contains("unterminated string")
                || lower.contains("expecting ',' delimiter")
                || lower.contains("expecting value")
                || lower.contains("must be valid json")
                || lower.contains("must be a valid json")
                || lower.contains("validation:")
                || lower.contains("invalid json")
                || lower.contains("jsondecodeerror"))
        {
            return UpstreamErrorClass::InvalidPayload;
        }
    }
    if status == 403 {
        if body.contains("VALIDATION_REQUIRED") {
            return UpstreamErrorClass::ValidationRequired;
        }
        if body.contains("PERMISSION_DENIED") || body.contains("API_KEY_INVALID") {
            return UpstreamErrorClass::PermissionDenied;
        }
    }
    if status == 429 && body.contains("RESOURCE_EXHAUSTED") {
        return UpstreamErrorClass::ResourceExhausted;
    }
    UpstreamErrorClass::Generic
}

#[must_use]
pub fn is_hard_skip_error(err: &CoreError) -> bool {
    if let CoreError::UpstreamError {
        status,
        body,
        is_hard_skip,
        ..
    } = err
    {
        if *is_hard_skip {
            return true;
        }
        classify_upstream_error(*status, body).is_hard_skip()
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classification_403_validation_required() {
        assert_eq!(
            classify_upstream_error(403, r#"{"error":"VALIDATION_REQUIRED"}"#),
            UpstreamErrorClass::ValidationRequired
        );
    }

    #[test]
    fn classification_429_resource_exhausted() {
        assert_eq!(
            classify_upstream_error(429, r#"{"reason":"RESOURCE_EXHAUSTED"}"#),
            UpstreamErrorClass::ResourceExhausted
        );
    }

    #[test]
    fn classification_400_2013() {
        assert_eq!(
            classify_upstream_error(
                400,
                r#"{"error":{"code":2013,"message":"function name or parameters is empty"}}"#,
            ),
            UpstreamErrorClass::MalformedToolCall
        );
    }

    #[test]
    fn classification_400_text_marker_only() {
        assert_eq!(
            classify_upstream_error(400, "function name or parameters is empty"),
            UpstreamErrorClass::MalformedToolCall
        );
    }

    #[test]
    fn classification_403_permission_denied() {
        assert_eq!(
            classify_upstream_error(403, "PERMISSION_DENIED"),
            UpstreamErrorClass::PermissionDenied
        );
        assert_eq!(
            classify_upstream_error(403, "API_KEY_INVALID"),
            UpstreamErrorClass::PermissionDenied
        );
    }

    #[test]
    fn classification_500_generic() {
        assert_eq!(
            classify_upstream_error(500, "upstream down"),
            UpstreamErrorClass::Generic
        );
    }

    #[test]
    fn classification_empty_body_403_is_generic() {
        assert_eq!(
            classify_upstream_error(403, ""),
            UpstreamErrorClass::Generic
        );
    }

    #[test]
    fn classification_400_invalid_payload() {
        assert_eq!(
            classify_upstream_error(
                400,
                r#"{"error":{"code":400,"message":"Invalid value at 'contents[0].parts[1].inline_data.data' (TYPE_BYTES), Base64 decoding failed"}}"#,
            ),
            UpstreamErrorClass::InvalidPayload
        );
        assert_eq!(
            classify_upstream_error(
                400,
                r#"{"error":{"message":"Input required: specify \"prompt\" or \"messages\"","code":400},"user_id":"org_123"}"#,
            ),
            UpstreamErrorClass::InvalidPayload
        );
        assert_eq!(
            classify_upstream_error(400, "Extra data: line 1 column 7186 (char 7185)"),
            UpstreamErrorClass::InvalidPayload
        );
        assert_eq!(
            classify_upstream_error(
                400,
                "Unterminated string starting at: line 1 column 13 (char 12)"
            ),
            UpstreamErrorClass::InvalidPayload
        );
        assert_eq!(
            classify_upstream_error(
                400,
                r#"{"message":"Validation: `messages[32].tool_calls[0].function.arguments` must be a valid JSON object string: trailing characters at line 1 column 7186","type":"Bad Request","code":400}"#
            ),
            UpstreamErrorClass::InvalidPayload
        );
        assert_eq!(
            classify_upstream_error(
                400,
                r#"{"error":{"message":"Error from provider (Console): arguments must be valid JSON","type":"invalid_request_error"}}"#
            ),
            UpstreamErrorClass::InvalidPayload
        );
        assert_eq!(
            classify_upstream_error(
                400,
                "Expecting ',' delimiter: line 1 column 2386 (char 2385)"
            ),
            UpstreamErrorClass::InvalidPayload
        );
    }

    #[test]
    fn classification_400_masked_upstream_failure() {
        assert_eq!(
            classify_upstream_error(
                400,
                r#"{"type":"error","error":{"type":"api_error","message":"Error from provider (Console): Upstream request failed: Model is unavailable."}}"#,
            ),
            UpstreamErrorClass::ResourceExhausted
        );
        assert_eq!(
            classify_upstream_error(400, "Upstream request failed: Model is unavailable."),
            UpstreamErrorClass::ResourceExhausted
        );
        assert_eq!(
            classify_upstream_error(400, "The requested model is not supported."),
            UpstreamErrorClass::ResourceExhausted
        );
    }

    #[test]
    fn is_hard_skip_class_method() {
        assert!(UpstreamErrorClass::ValidationRequired.is_hard_skip());
        assert!(UpstreamErrorClass::PermissionDenied.is_hard_skip());
        assert!(UpstreamErrorClass::ResourceExhausted.is_hard_skip());
        assert!(UpstreamErrorClass::MalformedToolCall.is_hard_skip());
        assert!(UpstreamErrorClass::InvalidPayload.is_hard_skip());
        assert!(!UpstreamErrorClass::Generic.is_hard_skip());
    }

    #[test]
    fn is_hard_skip_error_pulls_body_out_of_core_error() {
        let err = CoreError::upstream_error_with_skip(
            403,
            "antigravity",
            "gemini-2.5",
            r#"{"error":"VALIDATION_REQUIRED"}"#,
            false,
            true,
        );
        assert!(is_hard_skip_error(&err));
    }

    #[test]
    fn is_hard_skip_error_false_for_500() {
        let err = CoreError::upstream_error(500, "antigravity", "gemini-2.5", "boom", false);
        assert!(!is_hard_skip_error(&err));
    }

    #[test]
    fn is_hard_skip_error_false_for_non_upstream() {
        assert!(!is_hard_skip_error(&CoreError::RateLimited {
            provider: "p".into(),
            retry_after_ms: 1000,
            is_proxy_rotated: false,
        }));
    }
}

#[cfg(test)]
mod adversarial_tests {
    use super::*;

    #[test]
    fn adv_large_body_with_marker_inside_json() {
        // Substring matching must find a marker buried in a ~1MB JSON body.
        let mut body = String::from(r#"{"data":""#);
        for _ in 0..100_000 {
            body.push_str("abcdefghij");
        }
        body.push_str(r#"","error":"VALIDATION_REQUIRED"}"#);
        assert_eq!(
            classify_upstream_error(403, &body),
            UpstreamErrorClass::ValidationRequired,
            "must find marker in large body"
        );
    }

    #[test]
    fn adv_validation_required_with_wrong_status_500() {
        // Classification is status-gated: a 500 never matches, marker or not.
        assert_eq!(
            classify_upstream_error(500, r#"{"error":"VALIDATION_REQUIRED"}"#),
            UpstreamErrorClass::Generic
        );
    }

    #[test]
    fn adv_permission_denied_with_wrong_status_500() {
        assert_eq!(
            classify_upstream_error(500, "PERMISSION_DENIED"),
            UpstreamErrorClass::Generic
        );
    }

    #[test]
    fn adv_resource_exhausted_with_wrong_status_500() {
        assert_eq!(
            classify_upstream_error(500, "RESOURCE_EXHAUSTED"),
            UpstreamErrorClass::Generic
        );
    }

    #[test]
    fn adv_malformed_tool_call_marker_with_status_403() {
        // The 2013 marker is gated to status 400.
        assert_eq!(
            classify_upstream_error(403, "code 2013 error"),
            UpstreamErrorClass::Generic
        );
    }

    #[test]
    fn adv_empty_body_all_statuses() {
        for status in [400, 403, 429, 500, 503] {
            assert_eq!(
                classify_upstream_error(status, ""),
                UpstreamErrorClass::Generic,
                "empty body must be Generic for status {status}"
            );
        }
    }

    #[test]
    fn adv_400_body_with_both_2013_and_permission_denied() {
        // The 400 branch is checked first.
        assert_eq!(
            classify_upstream_error(400, r#"{"error":"2013 PERMISSION_DENIED"}"#,),
            UpstreamErrorClass::MalformedToolCall
        );
    }

    #[test]
    fn adv_403_body_with_validation_and_permission_denied() {
        // The 403 branch is checked first.
        assert_eq!(
            classify_upstream_error(
                403,
                r#"{"error":"VALIDATION_REQUIRED","detail":"PERMISSION_DENIED"}"#,
            ),
            UpstreamErrorClass::ValidationRequired
        );
    }

    #[test]
    fn adv_body_with_null_bytes() {
        let body = "VALIDATION_REQUIRED\u{0000}\n";
        assert_eq!(
            classify_upstream_error(403, body),
            UpstreamErrorClass::ValidationRequired
        );
    }

    #[test]
    fn adv_body_with_control_characters() {
        let body = "PERMISSION_DENIED\u{0003}\u{0004}";
        assert_eq!(
            classify_upstream_error(403, body),
            UpstreamErrorClass::PermissionDenied
        );
    }

    #[test]
    fn adv_503_html_body_is_generic() {
        let body = "<html><body><h1>503 Service Unavailable</h1></body></html>";
        assert_eq!(
            classify_upstream_error(503, body),
            UpstreamErrorClass::Generic
        );
    }

    #[test]
    fn adv_403_body_with_json_array_of_error_strings() {
        let body = r#"["VALIDATION_REQUIRED", "PERMISSION_DENIED"]"#;
        assert_eq!(
            classify_upstream_error(403, body),
            UpstreamErrorClass::ValidationRequired
        );
    }

    #[test]
    fn adv_is_hard_skip_default_upstream_error_is_false() {
        // is_hard_skip_error re-runs classification on the body, so a matching
        // marker overrides the constructor default of false.
        let err = CoreError::upstream_error(403, "p", "m", "VALIDATION_REQUIRED", false);
        assert!(is_hard_skip_error(&err));
    }

    #[test]
    fn adv_is_hard_skip_explicitly_set_true() {
        let err = CoreError::upstream_error_with_skip(403, "p", "m", "anything", false, true);
        assert!(is_hard_skip_error(&err));
    }

    #[test]
    fn adv_timeout_error_not_hard_skip() {
        let err = CoreError::UpstreamTimeout {
            phase: "headers".to_string(),
            ms: 5000,
        };
        assert!(!is_hard_skip_error(&err));
    }

    #[test]
    fn adv_connection_error_not_hard_skip() {
        let err = CoreError::UpstreamConnection("refused".into());
        assert!(!is_hard_skip_error(&err));
    }

    #[test]
    fn adv_unicode_body_ascii_marker_still_matches() {
        let body = "\u{1F600}\u{1F4A5} some VALIDATION_REQUIRED here";
        assert_eq!(
            classify_upstream_error(403, body),
            UpstreamErrorClass::ValidationRequired
        );
    }

    #[test]
    fn adv_400_text_marker_function_name_empty() {
        assert_eq!(
            classify_upstream_error(400, "function name or parameters is empty"),
            UpstreamErrorClass::MalformedToolCall
        );
    }

    #[test]
    fn adv_400_text_marker_in_json_error_envelope() {
        let body = r#"{"error":{"message":"function name or parameters is empty","code":400}}"#;
        assert_eq!(
            classify_upstream_error(400, body),
            UpstreamErrorClass::MalformedToolCall
        );
    }

    #[test]
    fn adv_all_variants_hard_skip_correctness() {
        assert!(UpstreamErrorClass::ValidationRequired.is_hard_skip());
        assert!(UpstreamErrorClass::PermissionDenied.is_hard_skip());
        assert!(UpstreamErrorClass::ResourceExhausted.is_hard_skip());
        assert!(UpstreamErrorClass::MalformedToolCall.is_hard_skip());
        assert!(!UpstreamErrorClass::Generic.is_hard_skip());
    }

    #[test]
    fn test_codebuddy_error_classification() {
        for code in ["6000", "6001", "6005", "6008"] {
            let body = format!(r#"{{"code": {code}, "message": "rate limited"}}"#);
            assert_eq!(
                classify_upstream_error(429, &body),
                UpstreamErrorClass::ResourceExhausted
            );
        }

        let body_14014 = r#"{"code": 14014, "message": "UsageLimitEnterpriseExhausted"}"#;
        assert_eq!(
            classify_upstream_error(400, body_14014),
            UpstreamErrorClass::ResourceExhausted
        );

        let body_14018 = r#"{"code": 14018, "message": "UsageLimitUserExhausted"}"#;
        assert_eq!(
            classify_upstream_error(403, body_14018),
            UpstreamErrorClass::ResourceExhausted
        );

        let body_auth = r#"{"code": 14015, "message": "license expired"}"#;
        assert_eq!(
            classify_upstream_error(401, body_auth),
            UpstreamErrorClass::PermissionDenied
        );

        // JSON-RPC shell: outer code -32603, inner business code 11115.
        let body_nested = r#"{"status": 400, "error": {"code": -32603, "data": {"code": 11115, "statusCode": 400}}}"#;
        assert_eq!(
            classify_upstream_error(400, body_nested),
            UpstreamErrorClass::InvalidPayload
        );
    }

    #[test]
    fn test_no_false_positive_on_unrelated_numbers() {
        // `body.contains("6000")` would read this token limit as a CodeBuddy
        // rate-limit code and trip the circuit breaker, so the code must be
        // parsed out of the envelope rather than substring-matched.
        let body = r#"{"error":{"message":"Invalid parameter: max_tokens 6000 exceeds maximum allowable value","code":400}}"#;
        assert_ne!(
            classify_upstream_error(400, body),
            UpstreamErrorClass::ResourceExhausted
        );
        assert_eq!(
            classify_upstream_error(400, body),
            UpstreamErrorClass::Generic
        );

        let body_invalid_val =
            r#"{"error":{"message":"Invalid value for max_tokens: 6000","code":400}}"#;
        assert_eq!(
            classify_upstream_error(400, body_invalid_val),
            UpstreamErrorClass::InvalidPayload
        );
    }
}
