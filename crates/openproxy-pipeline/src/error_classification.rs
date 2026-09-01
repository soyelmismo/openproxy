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