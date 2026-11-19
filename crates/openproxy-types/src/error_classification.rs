//! Pipeline-side classification of upstream error categories.
//!
//! The enum is type-only and lives in `openproxy-types` so it can be
//! embedded in `CoreError::UpstreamError`. The matching policy
//! (`classify_upstream_error`) lives in
//! `openproxy-pipeline/src/error_classification.rs` next to the
//! dispatcher that calls it.
//!
//! Per GAP-4 in `docs/specs/antigravity-gaps-p2.md` §2.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub enum UpstreamErrorClass {
    #[default]
    Generic,
    ValidationRequired,
    PermissionDenied,
    ResourceExhausted,
    MalformedToolCall,
    InvalidPayload,
    /// Google CloudCode rejected the CLIENT fingerprint itself (404 "Requested
    /// entity was not found" on Argon-class models). Rotating accounts cannot
    /// fix this — every account egresses the same signature. Antigravity-
    /// Manager 9b82fb7e documents this as the hub-version gate: update the
    /// fingerprint, don't hammer the pool.
    FingerprintGate,
}

impl UpstreamErrorClass {
    #[inline]
    #[must_use]
    pub const fn is_hard_skip(self) -> bool {
        matches!(
            self,
            Self::ValidationRequired
                | Self::PermissionDenied
                | Self::ResourceExhausted
                | Self::MalformedToolCall
                | Self::InvalidPayload
                | Self::FingerprintGate
        )
    }
}

impl std::fmt::Display for UpstreamErrorClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Generic => "generic",
            Self::ValidationRequired => "validation_required",
            Self::PermissionDenied => "permission_denied",
            Self::ResourceExhausted => "resource_exhausted",
            Self::MalformedToolCall => "malformed_tool_call",
            Self::InvalidPayload => "invalid_payload",
            Self::FingerprintGate => "fingerprint_gate",
        };
        f.write_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_upstream_error_class_default() {
        assert_eq!(UpstreamErrorClass::default(), UpstreamErrorClass::Generic);
    }

    #[test]
    fn test_upstream_error_class_is_hard_skip() {
        assert!(!UpstreamErrorClass::Generic.is_hard_skip());
        assert!(UpstreamErrorClass::ValidationRequired.is_hard_skip());
        assert!(UpstreamErrorClass::PermissionDenied.is_hard_skip());
        assert!(UpstreamErrorClass::ResourceExhausted.is_hard_skip());
        assert!(UpstreamErrorClass::MalformedToolCall.is_hard_skip());
        assert!(UpstreamErrorClass::InvalidPayload.is_hard_skip());
        assert!(UpstreamErrorClass::FingerprintGate.is_hard_skip());
    }

    #[test]
    fn test_upstream_error_class_display() {
        assert_eq!(UpstreamErrorClass::Generic.to_string(), "generic");
        assert_eq!(
            UpstreamErrorClass::ValidationRequired.to_string(),
            "validation_required"
        );
        assert_eq!(
            UpstreamErrorClass::PermissionDenied.to_string(),
            "permission_denied"
        );
        assert_eq!(
            UpstreamErrorClass::ResourceExhausted.to_string(),
            "resource_exhausted"
        );
        assert_eq!(
            UpstreamErrorClass::MalformedToolCall.to_string(),
            "malformed_tool_call"
        );
        assert_eq!(
            UpstreamErrorClass::InvalidPayload.to_string(),
            "invalid_payload"
        );
        assert_eq!(
            UpstreamErrorClass::FingerprintGate.to_string(),
            "fingerprint_gate"
        );
    }
}
