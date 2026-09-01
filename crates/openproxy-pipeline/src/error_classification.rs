//! Classify upstream error bodies into "account fault" vs. "request fault".
//!
//! Used by the circuit breaker to avoid tripping on request-shaped errors.
//! Per GAP-4 in `docs/specs/antigravity-gaps-p2.md` §2.
//!
//! The enum lives in `openproxy-types` (so it can be embedded in
//! `CoreError::UpstreamError`). This module re-exports it and adds the
//! classification policy.

pub use openproxy_types::UpstreamErrorClass;

use openproxy_types::CoreError;

#[must_use]
pub fn classify_upstream_error(status: u16, body: &str) -> UpstreamErrorClass {
    if status == 400
        && (body.contains("2013") || body.contains("function name or parameters is empty"))
    {
        return UpstreamErrorClass::MalformedToolCall;
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
    fn is_hard_skip_class_method() {
        assert!(UpstreamErrorClass::ValidationRequired.is_hard_skip());
        assert!(UpstreamErrorClass::PermissionDenied.is_hard_skip());
        assert!(UpstreamErrorClass::ResourceExhausted.is_hard_skip());
        assert!(UpstreamErrorClass::MalformedToolCall.is_hard_skip());
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

// ============================================================
// GAP-4: Adversarial tests for error classification
// ============================================================
#[cfg(test)]
mod adversarial_tests {
    use super::*;

    // --- Body with embedded error markers inside large payloads ---

    #[test]
    fn adv_large_body_with_marker_inside_json() {
        // 10MB body that contains VALIDATION_REQUIRED buried in JSON.
        // The classifier does substring matching, so it should find it.
        let mut body = String::from(r#"{"data":""#);
        // Add filler
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

    // --- Status/body mismatch: body marker present but wrong status ---

    #[test]
    fn adv_validation_required_with_wrong_status_500() {
        // VALIDATION_REQUIRED in body but status=500 → must NOT classify as
        // ValidationRequired (classification is status-gated).
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
        // "2013" in body but status=403 → must NOT classify as MalformedToolCall
        assert_eq!(
            classify_upstream_error(403, "code 2013 error"),
            UpstreamErrorClass::Generic
        );
    }

    // --- Body with no markers at various statuses ---

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

    // --- Body contains BOTH markers (priority test) ---

    #[test]
    fn adv_400_body_with_both_2013_and_permission_denied() {
        // Both markers in body, status=400 → MalformedToolCall wins (400 checked first)
        assert_eq!(
            classify_upstream_error(400, r#"{"error":"2013 PERMISSION_DENIED"}"#,),
            UpstreamErrorClass::MalformedToolCall
        );
    }

    #[test]
    fn adv_403_body_with_validation_and_permission_denied() {
        // Both VALIDATION_REQUIRED and PERMISSION_DENIED in body, status=403
        // → ValidationRequired wins (checked first in the 403 branch)
        assert_eq!(
            classify_upstream_error(
                403,
                r#"{"error":"VALIDATION_REQUIRED","detail":"PERMISSION_DENIED"}"#,
            ),
            UpstreamErrorClass::ValidationRequired
        );
    }

    // --- Body with control characters ---

    #[test]
    fn adv_body_with_null_bytes() {
        // The classification does substring matching — null bytes are
        // just more chars in the &str.
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

    // --- Body that's actually HTML ---

    #[test]
    fn adv_503_html_body_is_generic() {
        let body = "<html><body><h1>503 Service Unavailable</h1></body></html>";
        assert_eq!(
            classify_upstream_error(503, body),
            UpstreamErrorClass::Generic
        );
    }

    // --- Body with JSON array containing error codes ---

    #[test]
    fn adv_403_body_with_json_array_of_error_strings() {
        // Array of error strings: marker is present → must classify
        let body = r#"["VALIDATION_REQUIRED", "PERMISSION_DENIED"]"#;
        assert_eq!(
            classify_upstream_error(403, body),
            UpstreamErrorClass::ValidationRequired
        );
    }

    // --- is_hard_skip_error on UpstreamError without classification ---

    #[test]
    fn adv_is_hard_skip_default_upstream_error_is_false() {
        let err = CoreError::upstream_error(403, "p", "m", "VALIDATION_REQUIRED", false);
        // upstream_error() sets is_hard_skip=false (default).
        // But is_hard_skip_error() also re-runs classification on the body.
        // So this should return true because the body matches.
        assert!(is_hard_skip_error(&err));
    }

    #[test]
    fn adv_is_hard_skip_explicitly_set_true() {
        let err = CoreError::upstream_error_with_skip(403, "p", "m", "anything", false, true);
        assert!(is_hard_skip_error(&err));
    }

    // --- Timeout and connection errors are NOT hard_skip ---

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

    // --- Large body with Unicode characters ---

    #[test]
    fn adv_unicode_body_ascii_marker_still_matches() {
        // Unicode prefix before ASCII marker — should still match.
        let body = "\u{1F600}\u{1F4A5} some VALIDATION_REQUIRED here";
        assert_eq!(
            classify_upstream_error(403, body),
            UpstreamErrorClass::ValidationRequired
        );
    }

    // --- "function name or parameters is empty" variant ---

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

    // --- is_hard_skip is const fn ---

    #[test]
    fn adv_all_variants_hard_skip_correctness() {
        // Every non-Generic variant must be hard_skip
        assert!(UpstreamErrorClass::ValidationRequired.is_hard_skip());
        assert!(UpstreamErrorClass::PermissionDenied.is_hard_skip());
        assert!(UpstreamErrorClass::ResourceExhausted.is_hard_skip());
        assert!(UpstreamErrorClass::MalformedToolCall.is_hard_skip());
        assert!(!UpstreamErrorClass::Generic.is_hard_skip());
    }
}
