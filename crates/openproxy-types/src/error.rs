//! Crate-wide error types. Every error carries a request_id and trace_id for traceability.

use crate::ids::{RequestId, TraceId};
use std::fmt;
use thiserror::Error;

impl_string_enum! {
    /// The reason why a request was cancelled.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum CancelReason {
        ClientDisconnected => "client_disconnected",
        WatchdogTimeout => "watchdog_timeout",
    }
    error: "cancel reason"
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorContext {
    pub request_id: RequestId,
    pub trace_id: TraceId,
    pub phase: &'static str,
    pub provider: Option<String>,
    pub account: Option<i64>,
    pub model: Option<String>,
}

fn format_opt_field<T: fmt::Display>(
    f: &mut fmt::Formatter<'_>,
    name: &str,
    val: Option<T>,
) -> fmt::Result {
    if let Some(v) = val {
        write!(f, " {name}={v}")?;
    }
    Ok(())
}

impl fmt::Display for ErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "req={} trace={} phase={}",
            self.request_id, self.trace_id, self.phase
        )?;
        format_opt_field(f, "provider", self.provider.as_deref())?;
        format_opt_field(f, "account", self.account)?;
        format_opt_field(f, "model", self.model.as_deref())
    }
}

#[derive(Debug, Clone, Error)]
pub enum CoreError {
    #[error("config: {0}")]
    Config(String),

    #[error("database: {message}")]
    Database {
        message: String,
        source: Option<std::sync::Arc<dyn std::error::Error + Send + Sync>>,
    },

    #[error("migration {version} failed: {message}")]
    Migration { version: i64, message: String },

    #[error("provider not found: {0}")]
    ProviderNotFound(String),

    #[error("account not found: {0}")]
    AccountNotFound(i64),

    #[error("combo not found: {0}")]
    ComboNotFound(i64),

    #[error("model not found: provider={provider} model={model}")]
    ModelNotFound { provider: String, model: String },

    #[error("no healthy targets for combo {0}")]
    NoHealthyTargets(i64),

    #[error("upstream timeout in phase {phase} after {ms}ms")]
    UpstreamTimeout { phase: String, ms: u64 },

    #[error("upstream error: status={status} provider={provider} model={model} body={body}")]
    UpstreamError {
        status: u16,
        provider: String,
        model: String,
        body: String,
        is_proxy_rotated: bool,
        class: crate::UpstreamErrorClass,
        is_hard_skip: bool,
    },

    #[error("upstream connection error: {0}")]
    UpstreamConnection(String),

    #[error("rate limited: provider={provider} retry_after_ms={retry_after_ms}")]
    RateLimited {
        provider: String,
        retry_after_ms: u64,
        is_proxy_rotated: bool,
    },

    #[error("parse error: {0}")]
    Parse(String),

    #[error("cancelled: {0}")]
    Cancelled(CancelReason),

    #[error("race cancelled: this attempt was a race loser")]
    RaceLost,

    #[error("auth: {0}")]
    Auth(String),

    #[error("validation: {0}")]
    Validation(String),

    #[error("internal: {0}")]
    Internal(String),

    /// LOW fix (#14): the writer lock could not be acquired
    /// within its timeout budget. Maps to HTTP 503 in
    /// `http_status()` — a transient service condition, not a
    /// bug. The caller should retry after a short backoff.
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    /// LOW fix (#12): a generic "not found" for resources that
    /// don't warrant a dedicated variant. Used by
    /// `oauth_tickets::mark_consumed` to signal a double-redeem
    /// attempt (the row exists but the WHERE clause
    /// `consumed_at IS NULL` no longer matches). Maps to HTTP 404.
    #[error("{what} not found: {id}")]
    NotFound { what: String, id: String },
}

impl CoreError {
    #[inline]
    pub fn upstream_error(
        status: u16,
        provider: impl Into<String>,
        model: impl Into<String>,
        body: impl Into<String>,
        is_proxy_rotated: bool,
    ) -> Self {
        CoreError::UpstreamError {
            status,
            provider: provider.into(),
            model: model.into(),
            body: body.into(),
            is_proxy_rotated,
            class: crate::UpstreamErrorClass::Generic,
            is_hard_skip: false,
        }
    }

    #[inline]
    pub fn upstream_error_with_skip(
        status: u16,
        provider: impl Into<String>,
        model: impl Into<String>,
        body: impl Into<String>,
        is_proxy_rotated: bool,
        is_hard_skip: bool,
    ) -> Self {
        CoreError::UpstreamError {
            status,
            provider: provider.into(),
            model: model.into(),
            body: body.into(),
            is_proxy_rotated,
            class: crate::UpstreamErrorClass::Generic,
            is_hard_skip,
        }
    }

    #[inline]
    pub fn upstream_error_classified(
        status: u16,
        provider: impl Into<String>,
        model: impl Into<String>,
        body: impl Into<String>,
        is_proxy_rotated: bool,
        class: crate::UpstreamErrorClass,
    ) -> Self {
        let is_hard_skip = class.is_hard_skip();
        CoreError::UpstreamError {
            status,
            provider: provider.into(),
            model: model.into(),
            body: body.into(),
            is_proxy_rotated,
            class,
            is_hard_skip,
        }
    }

    #[inline]
    pub fn model_not_found(provider: impl Into<String>, model: impl Into<String>) -> Self {
        CoreError::ModelNotFound {
            provider: provider.into(),
            model: model.into(),
        }
    }

    #[inline]
    pub fn not_found(what: impl Into<String>, id: impl Into<String>) -> Self {
        CoreError::NotFound {
            what: what.into(),
            id: id.into(),
        }
    }

    pub fn is_proxy_rotated(&self) -> bool {
        match self {
            CoreError::UpstreamError {
                is_proxy_rotated, ..
            }
            | CoreError::RateLimited {
                is_proxy_rotated, ..
            } => *is_proxy_rotated,
            _ => false,
        }
    }

    #[inline]
    #[must_use]
    pub fn is_hard_skip(&self) -> bool {
        match self {
            CoreError::UpstreamError { is_hard_skip, .. } => *is_hard_skip,
            _ => false,
        }
    }

    #[inline]
    #[must_use]
    pub fn upstream_error_class(&self) -> Option<crate::UpstreamErrorClass> {
        match self {
            CoreError::UpstreamError { class, .. } => Some(*class),
            _ => None,
        }
    }

    /// Produce a clone of the error.
    #[inline]
    pub fn clone_for_result(&self) -> CoreError {
        self.clone()
    }

    /// HTTP status code to return to the client.
    pub fn http_status(&self) -> u16 {
        match self {
            CoreError::Auth(_) => 401,
            CoreError::Validation(_) => 400,
            CoreError::ProviderNotFound(_)
            | CoreError::AccountNotFound(_)
            | CoreError::ComboNotFound(_)
            | CoreError::ModelNotFound { .. }
            | CoreError::NotFound { .. } => 404,
            CoreError::RateLimited { .. } => 429,
            CoreError::UpstreamError { status, .. } => *status,
            CoreError::UpstreamTimeout { .. } => 529,
            CoreError::UpstreamConnection(_) | CoreError::NoHealthyTargets(_) => 502,
            CoreError::Cancelled(CancelReason::ClientDisconnected) | CoreError::RaceLost => 499,
            CoreError::Cancelled(CancelReason::WatchdogTimeout) => 504,
            CoreError::Parse(_)
            | CoreError::Database { .. }
            | CoreError::Migration { .. }
            | CoreError::Config(_)
            | CoreError::Internal(_) => 500,
            // LOW fix (#14): 503 Service Unavailable for transient
            // resource exhaustion. The client (or the operator's
            // dashboard) should retry after a short backoff.
            CoreError::ServiceUnavailable(_) => 503,
        }
    }

    /// Short string code for the client.
    pub fn code(&self) -> &'static str {
        match self {
            CoreError::Auth(_) => "auth",
            CoreError::Validation(_) => "validation",
            CoreError::ProviderNotFound(_) => "provider_not_found",
            CoreError::AccountNotFound(_) => "account_not_found",
            CoreError::ComboNotFound(_) => "combo_not_found",
            CoreError::ModelNotFound { .. } => "model_not_found",
            CoreError::NoHealthyTargets(_) => "no_healthy_targets",
            CoreError::UpstreamTimeout { .. } => "upstream_timeout",
            CoreError::UpstreamConnection(_) => "upstream_connection",
            CoreError::UpstreamError { .. } => "upstream_error",
            CoreError::RateLimited { .. } => "rate_limited",
            CoreError::Parse(_) => "parse_error",
            CoreError::Cancelled(r) => r.as_str(),
            CoreError::RaceLost => "race_lost",
            CoreError::Database { .. } => "database",
            CoreError::Migration { .. } => "migration",
            CoreError::Config(_) => "config",
            CoreError::Internal(_) => "internal",
            CoreError::ServiceUnavailable(_) => "service_unavailable",
            CoreError::NotFound { .. } => "not_found",
        }
    }

    /// Reconstructs a [`CoreError`] from a canonical error code and message.
    pub fn from_code_and_message(code: &str, message: &str) -> Option<Self> {
        match code {
            "auth" => Some(CoreError::Auth(message.to_string())),
            "validation" => Some(CoreError::Validation(message.to_string())),
            "provider_not_found" => Some(CoreError::ProviderNotFound(message.to_string())),
            "account_not_found" => message
                .trim()
                .parse::<i64>()
                .ok()
                .map(CoreError::AccountNotFound),
            "combo_not_found" => message
                .trim()
                .parse::<i64>()
                .ok()
                .map(CoreError::ComboNotFound),
            "model_not_found" => Some(CoreError::ModelNotFound {
                provider: "<see message>".to_string(),
                model: message.to_string(),
            }),
            "no_healthy_targets" => message
                .trim()
                .parse::<i64>()
                .ok()
                .map(CoreError::NoHealthyTargets),
            "upstream_timeout" => Some(CoreError::UpstreamTimeout {
                phase: "<unknown>".to_string(),
                ms: 0,
            }),
            "upstream_connection" => Some(CoreError::UpstreamConnection(message.to_string())),
            "upstream_error" => Some(CoreError::UpstreamError {
                status: 0,
                provider: "<see message>".to_string(),
                model: "<see message>".to_string(),
                body: message.to_string(),
                is_proxy_rotated: false,
                class: crate::UpstreamErrorClass::Generic,
                is_hard_skip: false,
            }),
            "rate_limited" => Some(CoreError::RateLimited {
                provider: "<see message>".to_string(),
                retry_after_ms: 0,
                is_proxy_rotated: false,
            }),
            "parse_error" => Some(CoreError::Parse(message.to_string())),
            "client_disconnected" => Some(CoreError::Cancelled(CancelReason::ClientDisconnected)),
            "watchdog_timeout" => Some(CoreError::Cancelled(CancelReason::WatchdogTimeout)),
            "race_lost" => Some(CoreError::RaceLost),
            "database" => Some(CoreError::Database {
                message: message.to_string(),
                source: None,
            }),
            "migration" => Some(CoreError::Migration {
                version: 0,
                message: message.to_string(),
            }),
            "config" => Some(CoreError::Config(message.to_string())),
            "internal" => Some(CoreError::Internal(message.to_string())),
            "service_unavailable" => Some(CoreError::ServiceUnavailable(message.to_string())),
            "not_found" => {
                if let Some((what, id)) = message.split_once(" not found: ") {
                    Some(CoreError::NotFound {
                        what: what.trim().to_string(),
                        id: id.trim().to_string(),
                    })
                } else if let Some((what, id)) = message.split_once(':') {
                    Some(CoreError::NotFound {
                        what: what.trim().to_string(),
                        id: id.trim().to_string(),
                    })
                } else {
                    Some(CoreError::NotFound {
                        what: "resource".to_string(),
                        id: message.to_string(),
                    })
                }
            }
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

impl From<tokio::task::JoinError> for CoreError {
    fn from(err: tokio::task::JoinError) -> Self {
        CoreError::Internal(err.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_core_error_clone() {
        let err = CoreError::Database {
            message: "disk full".into(),
            source: None,
        };
        let cloned = err.clone();
        assert_eq!(err.code(), cloned.code());
        assert_eq!(err.http_status(), cloned.http_status());
        assert_eq!(err.clone_for_result().code(), "database");
    }

    #[test]
    fn test_from_code_and_message_simple() {
        assert!(matches!(
            CoreError::from_code_and_message("auth", "bad token"),
            Some(CoreError::Auth(msg)) if msg == "bad token"
        ));
        assert!(matches!(
            CoreError::from_code_and_message("validation", "invalid param"),
            Some(CoreError::Validation(msg)) if msg == "invalid param"
        ));
        assert!(matches!(
            CoreError::from_code_and_message("provider_not_found", "openrouter"),
            Some(CoreError::ProviderNotFound(msg)) if msg == "openrouter"
        ));
        assert!(matches!(
            CoreError::from_code_and_message("service_unavailable", "overloaded"),
            Some(CoreError::ServiceUnavailable(msg)) if msg == "overloaded"
        ));
        assert!(CoreError::from_code_and_message("unknown_code", "foo").is_none());
    }

    #[test]
    fn test_from_code_and_message_numeric() {
        assert!(matches!(
            CoreError::from_code_and_message("account_not_found", "42"),
            Some(CoreError::AccountNotFound(42))
        ));
        assert!(CoreError::from_code_and_message("account_not_found", "nan").is_none());
        assert!(matches!(
            CoreError::from_code_and_message("combo_not_found", "10"),
            Some(CoreError::ComboNotFound(10))
        ));
        assert!(matches!(
            CoreError::from_code_and_message("no_healthy_targets", "99"),
            Some(CoreError::NoHealthyTargets(99))
        ));
    }

    #[test]
    fn test_from_code_and_message_cancelled_race() {
        assert!(matches!(
            CoreError::from_code_and_message("watchdog_timeout", "timeout"),
            Some(CoreError::Cancelled(CancelReason::WatchdogTimeout))
        ));
        assert!(matches!(
            CoreError::from_code_and_message("client_disconnected", "drop"),
            Some(CoreError::Cancelled(CancelReason::ClientDisconnected))
        ));
        assert!(matches!(
            CoreError::from_code_and_message("race_lost", "loser"),
            Some(CoreError::RaceLost)
        ));
    }

    #[test]
    fn test_from_code_and_message_not_found() {
        assert!(matches!(
            CoreError::from_code_and_message("not_found", "ticket not found: abc-123"),
            Some(CoreError::NotFound { what, id }) if what == "ticket" && id == "abc-123"
        ));
        assert!(matches!(
            CoreError::from_code_and_message("not_found", "user: 42"),
            Some(CoreError::NotFound { what, id }) if what == "user" && id == "42"
        ));
    }

    #[test]
    fn http_status_mapping() {
        assert_eq!(CoreError::Auth("x".into()).http_status(), 401);
        assert_eq!(CoreError::Validation("x".into()).http_status(), 400);
        assert_eq!(
            CoreError::RateLimited {
                provider: "p".into(),
                retry_after_ms: 1000,
                is_proxy_rotated: false,
            }
            .http_status(),
            429
        );
        assert_eq!(
            CoreError::Cancelled(CancelReason::ClientDisconnected).http_status(),
            499
        );
        assert_eq!(
            CoreError::Cancelled(CancelReason::WatchdogTimeout).http_status(),
            504
        );
        assert_eq!(
            CoreError::UpstreamTimeout {
                phase: "ttft".into(),
                ms: 100
            }
            .http_status(),
            529
        );
    }

    #[test]
    fn codes_are_distinct() {
        let mut codes = std::collections::HashSet::new();
        for err in [
            CoreError::Auth("x".into()),
            CoreError::Validation("x".into()),
            CoreError::ProviderNotFound("x".into()),
            CoreError::RaceLost,
            CoreError::Cancelled(CancelReason::ClientDisconnected),
        ] {
            codes.insert(err.code());
        }
        assert_eq!(codes.len(), 5);
    }

    #[test]
    fn test_is_proxy_rotated() {
        assert!(
            CoreError::UpstreamError {
                status: 500,
                provider: "test".into(),
                model: "model".into(),
                body: "err".into(),
                is_proxy_rotated: true,
                class: crate::UpstreamErrorClass::Generic,
                is_hard_skip: false,
            }
            .is_proxy_rotated()
        );

        assert!(
            !CoreError::UpstreamError {
                status: 500,
                provider: "test".into(),
                model: "model".into(),
                body: "err".into(),
                is_proxy_rotated: false,
                class: crate::UpstreamErrorClass::Generic,
                is_hard_skip: false,
            }
            .is_proxy_rotated()
        );

        assert!(
            CoreError::RateLimited {
                provider: "test".into(),
                retry_after_ms: 0,
                is_proxy_rotated: true,
            }
            .is_proxy_rotated()
        );

        assert!(
            !CoreError::RateLimited {
                provider: "test".into(),
                retry_after_ms: 0,
                is_proxy_rotated: false,
            }
            .is_proxy_rotated()
        );

        assert!(!CoreError::Auth("x".into()).is_proxy_rotated());
    }

    #[test]
    fn test_is_hard_skip_defaults_to_false() {
        let legacy = CoreError::upstream_error(400, "p", "m", "x", false);
        assert!(!legacy.is_hard_skip());
    }

    #[test]
    fn test_is_hard_skip_explicit() {
        let hard_skip = CoreError::upstream_error_with_skip(
            403,
            "antigravity",
            "gemini-2.5",
            r#"{"error":"VALIDATION_REQUIRED"}"#,
            false,
            true,
        );
        assert!(hard_skip.is_hard_skip());

        let generic = CoreError::upstream_error_with_skip(
            500,
            "antigravity",
            "gemini-2.5",
            "boom",
            false,
            false,
        );
        assert!(!generic.is_hard_skip());
    }

    #[test]
    fn test_upstream_error_classified_sets_class_and_hard_skip() {
        let v = CoreError::upstream_error_classified(
            403,
            "antigravity",
            "gemini-2.5",
            r#"{"error":"VALIDATION_REQUIRED"}"#,
            false,
            crate::UpstreamErrorClass::ValidationRequired,
        );
        assert!(v.is_hard_skip());
        assert_eq!(
            v.upstream_error_class(),
            Some(crate::UpstreamErrorClass::ValidationRequired)
        );

        let g = CoreError::upstream_error_classified(
            500,
            "antigravity",
            "gemini-2.5",
            "boom",
            false,
            crate::UpstreamErrorClass::Generic,
        );
        assert!(!g.is_hard_skip());
    }

    #[test]
    fn test_is_hard_skip_false_for_non_upstream() {
        assert!(!CoreError::Auth("x".into()).is_hard_skip());
        assert!(!CoreError::Validation("x".into()).is_hard_skip());
        assert!(
            !CoreError::RateLimited {
                provider: "p".into(),
                retry_after_ms: 1000,
                is_proxy_rotated: false,
            }
            .is_hard_skip()
        );
        assert!(!CoreError::Internal("x".into()).is_hard_skip());
        assert_eq!(CoreError::Auth("x".into()).upstream_error_class(), None);
    }
}
