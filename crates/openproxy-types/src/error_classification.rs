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

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize,
)]
pub enum UpstreamErrorClass {
    #[default]
    Generic,
    ValidationRequired,
    PermissionDenied,
    ResourceExhausted,
    MalformedToolCall,
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
        };
        f.write_str(s)
    }
}