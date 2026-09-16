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
            CoreError::ServiceUnavailable(_) => 503,
            _ => 500,
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
        let msg = message.to_string();
        match code {
            "auth" => Some(CoreError::Auth(msg)),
            "validation" => Some(CoreError::Validation(msg)),
            "provider_not_found" => Some(CoreError::ProviderNotFound(msg)),
            "account_not_found" => message.trim().parse().ok().map(CoreError::AccountNotFound),
            "combo_not_found" => message.trim().parse().ok().map(CoreError::ComboNotFound),
            "model_not_found" => Some(CoreError::ModelNotFound {
                provider: "<see message>".into(),
                model: msg,
            }),
            "no_healthy_targets" => message.trim().parse().ok().map(CoreError::NoHealthyTargets),
            "upstream_timeout" => Some(CoreError::UpstreamTimeout {
                phase: "<unknown>".into(),
                ms: 0,
            }),
            "upstream_connection" => Some(CoreError::UpstreamConnection(msg)),
            "upstream_error" => Some(CoreError::upstream_error(
                0,
                "<see message>",
                "<see message>",
                msg,
                false,
            )),
            "rate_limited" => Some(CoreError::RateLimited {
                provider: "<see message>".into(),
                retry_after_ms: 0,
                is_proxy_rotated: false,
            }),
            "parse_error" => Some(CoreError::Parse(msg)),
            "client_disconnected" => Some(CoreError::Cancelled(CancelReason::ClientDisconnected)),
            "watchdog_timeout" => Some(CoreError::Cancelled(CancelReason::WatchdogTimeout)),
            "race_lost" => Some(CoreError::RaceLost),
            "database" => Some(CoreError::Database {
                message: msg,
                source: None,
            }),
            "migration" => Some(CoreError::Migration {
                version: 0,
                message: msg,
            }),
            "config" => Some(CoreError::Config(msg)),
            "internal" => Some(CoreError::Internal(msg)),
            "service_unavailable" => Some(CoreError::ServiceUnavailable(msg)),
            "not_found" => {
                let (what, id) = message
                    .split_once(" not found: ")
                    .or_else(|| message.split_once(':'))
                    .unwrap_or(("resource", message));
                Some(CoreError::NotFound {
                    what: what.trim().to_string(),
                    id: id.trim().to_string(),
                })
            }
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

impl From<tokio::task::JoinError> for CoreError {
    fn from(err: tokio::task::JoinError) -> Self {
        CoreError::Internal(if err.is_panic() {
            "task panicked".to_string()
        } else {
            "task cancelled".to_string()
        })
    }
}

/// Blanket extension trait for attaching context to any [`std::result::Result`]
/// converting the error into a [`CoreError`].
pub trait ResultExt<T> {
    fn ctx_internal(self, msg: impl fmt::Display) -> Result<T>;
    fn ctx_validation(self, msg: impl fmt::Display) -> Result<T>;
    fn ctx_not_found(self, msg: impl fmt::Display) -> Result<T>;
    fn ctx_upstream(self, msg: impl fmt::Display) -> Result<T>;
}

impl<T, E: fmt::Display> ResultExt<T> for std::result::Result<T, E> {
    #[inline]
    fn ctx_internal(self, msg: impl fmt::Display) -> Result<T> {
        self.map_err(|e| CoreError::Internal(format!("{msg}: {e}")))
    }

    #[inline]
    fn ctx_validation(self, msg: impl fmt::Display) -> Result<T> {
        self.map_err(|e| CoreError::Validation(format!("{msg}: {e}")))
    }

    #[inline]
    fn ctx_not_found(self, msg: impl fmt::Display) -> Result<T> {
        self.map_err(|e| CoreError::NotFound {
            what: msg.to_string(),
            id: e.to_string(),
        })
    }

    #[inline]
    fn ctx_upstream(self, msg: impl fmt::Display) -> Result<T> {
        self.map_err(|e| CoreError::UpstreamConnection(format!("{msg}: {e}")))
    }
}

/// Extension trait for attaching contextual [`CoreError`] to an [`Option`].
pub trait OptionExt<T> {
    fn ctx_not_found(self, what: impl Into<String>, id: impl Into<String>) -> Result<T>;
    fn ctx_validation(self, msg: impl fmt::Display) -> Result<T>;
}

impl<T> OptionExt<T> for Option<T> {
    #[inline]
    fn ctx_not_found(self, what: impl Into<String>, id: impl Into<String>) -> Result<T> {
        self.ok_or_else(|| CoreError::NotFound {
            what: what.into(),
            id: id.into(),
        })
    }

    #[inline]
    fn ctx_validation(self, msg: impl fmt::Display) -> Result<T> {
        self.ok_or_else(|| CoreError::Validation(msg.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_core_error_clone_and_codes() {
        let err = CoreError::Database {
            message: "disk full".into(),
            source: None,
        };
        let cloned = err.clone();
        assert_eq!(err.code(), cloned.code());
        assert_eq!(err.http_status(), cloned.http_status());
        assert_eq!(err.clone_for_result().code(), "database");

        let distinct = [
            CoreError::Auth("x".into()),
            CoreError::Validation("x".into()),
            CoreError::ProviderNotFound("x".into()),
            CoreError::RaceLost,
            CoreError::Cancelled(CancelReason::ClientDisconnected),
        ];
        let set: std::collections::HashSet<_> = distinct.iter().map(|e| e.code()).collect();
        assert_eq!(set.len(), 5);
    }

    #[test]
    fn test_from_code_and_message() {
        assert!(
            matches!(CoreError::from_code_and_message("auth", "bad token"), Some(CoreError::Auth(m)) if m == "bad token")
        );
        assert!(
            matches!(CoreError::from_code_and_message("validation", "invalid"), Some(CoreError::Validation(m)) if m == "invalid")
        );
        assert!(
            matches!(CoreError::from_code_and_message("provider_not_found", "or"), Some(CoreError::ProviderNotFound(m)) if m == "or")
        );
        assert!(
            matches!(CoreError::from_code_and_message("service_unavailable", "ol"), Some(CoreError::ServiceUnavailable(m)) if m == "ol")
        );
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
        assert!(matches!(
            CoreError::from_code_and_message("watchdog_timeout", "t"),
            Some(CoreError::Cancelled(CancelReason::WatchdogTimeout))
        ));
        assert!(matches!(
            CoreError::from_code_and_message("client_disconnected", "d"),
            Some(CoreError::Cancelled(CancelReason::ClientDisconnected))
        ));
        assert!(matches!(
            CoreError::from_code_and_message("race_lost", "l"),
            Some(CoreError::RaceLost)
        ));
        assert!(
            matches!(CoreError::from_code_and_message("not_found", "ticket not found: abc-123"), Some(CoreError::NotFound { what, id }) if what == "ticket" && id == "abc-123")
        );
        assert!(
            matches!(CoreError::from_code_and_message("not_found", "user: 42"), Some(CoreError::NotFound { what, id }) if what == "user" && id == "42")
        );
        assert!(CoreError::from_code_and_message("unknown_code", "foo").is_none());
    }

    #[test]
    fn test_http_status_mapping() {
        let cases = [
            (CoreError::Auth("x".into()), 401),
            (CoreError::Validation("x".into()), 400),
            (
                CoreError::RateLimited {
                    provider: "p".into(),
                    retry_after_ms: 1000,
                    is_proxy_rotated: false,
                },
                429,
            ),
            (CoreError::Cancelled(CancelReason::ClientDisconnected), 499),
            (CoreError::Cancelled(CancelReason::WatchdogTimeout), 504),
            (
                CoreError::UpstreamTimeout {
                    phase: "ttft".into(),
                    ms: 100,
                },
                529,
            ),
        ];
        for (err, status) in cases {
            assert_eq!(err.http_status(), status);
        }
    }

    #[test]
    fn test_proxy_rotated_and_hard_skip() {
        let cases = [
            (
                CoreError::upstream_error(500, "t", "m", "e", true),
                true,
                false,
                None,
            ),
            (
                CoreError::upstream_error(500, "t", "m", "e", false),
                false,
                false,
                None,
            ),
            (
                CoreError::RateLimited {
                    provider: "t".into(),
                    retry_after_ms: 0,
                    is_proxy_rotated: true,
                },
                true,
                false,
                None,
            ),
            (
                CoreError::RateLimited {
                    provider: "t".into(),
                    retry_after_ms: 0,
                    is_proxy_rotated: false,
                },
                false,
                false,
                None,
            ),
            (CoreError::Auth("x".into()), false, false, None),
            (
                CoreError::upstream_error_with_skip(403, "ag", "g", "{}", false, true),
                false,
                true,
                None,
            ),
            (
                CoreError::upstream_error_classified(
                    403,
                    "ag",
                    "g",
                    "{}",
                    false,
                    crate::UpstreamErrorClass::ValidationRequired,
                ),
                false,
                true,
                Some(crate::UpstreamErrorClass::ValidationRequired),
            ),
        ];
        for (err, rot, skip, cls) in cases {
            assert_eq!(err.is_proxy_rotated(), rot);
            assert_eq!(err.is_hard_skip(), skip);
            if let Some(c) = cls {
                assert_eq!(err.upstream_error_class(), Some(c));
            }
        }
    }

    #[test]
    fn test_result_and_option_ext() {
        let err: std::result::Result<(), &str> = Err("fail");
        assert!(matches!(err.ctx_internal("io"), Err(CoreError::Internal(m)) if m == "io: fail"));
        assert!(matches!(err.ctx_validation("f"), Err(CoreError::Validation(m)) if m == "f: fail"));
        assert!(
            matches!(err.ctx_not_found("it"), Err(CoreError::NotFound { what, id }) if what == "it" && id == "fail")
        );
        assert!(
            matches!(err.ctx_upstream("c"), Err(CoreError::UpstreamConnection(m)) if m == "c: fail")
        );
        let ok: std::result::Result<u32, &str> = Ok(42);
        assert_eq!(ok.ctx_internal("x").unwrap(), 42);

        let none: Option<i32> = None;
        assert!(
            matches!(none.ctx_not_found("acc", "1"), Err(CoreError::NotFound { what, id }) if what == "acc" && id == "1")
        );
        assert!(matches!(none.ctx_validation("msg"), Err(CoreError::Validation(m)) if m == "msg"));
        assert_eq!(Some(100).ctx_not_found("a", "1").unwrap(), 100);
    }
}
