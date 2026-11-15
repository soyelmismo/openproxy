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

    #[error(
        "rate limited: the {window} quota is used up (${spent_usd:.2} of the ${cap_usd:.2} cap); it resets at {resets_at}"
    )]
    QuotaExceeded {
        window: String,
        spent_usd: f64,
        cap_usd: f64,
        resets_at: String,
    },

    #[error(
        "plan credits exhausted (balance_usd={balance_usd:.4}): buy a plan at /checkout to keep using the API"
    )]
    InsufficientCredits { balance_usd: f64 },

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

    /// The writer lock could not be acquired within its timeout budget.
    /// Maps to HTTP 503: a transient service condition the caller retries
    /// after a short backoff.
    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    /// Generic "not found". `oauth_tickets::mark_consumed` uses it for a
    /// double-redeem attempt, where the row exists but the WHERE clause
    /// `consumed_at IS NULL` no longer matches. Maps to HTTP 404.
    #[error("{what} not found: {id}")]
    NotFound { what: String, id: String },

    /// The usage journal has reached its maximum pending capacity limit.
    /// Callers must apply backpressure or wait for draining workers.
    #[error("usage journal capacity exhausted; admission rejected")]
    JournalCapacityExhausted,

    /// A target failure reduced to what a caller routed through a combo is
    /// allowed to see.
    ///
    /// Built by [`CoreError::masked_for_client`] only: the real cause (provider
    /// id, model id, upstream body, timeout phase, internal quota state) stays
    /// in the `usage` row and the trace log, and never in the response. `status`
    /// is the class the caller can act on (4xx for its own payload, 429 for
    /// upstream pressure, 5xx for availability), never the raw upstream code
    /// when that code would accuse the caller's own key.
    #[error("{message}")]
    MaskedUpstream {
        status: u16,
        message: String,
        /// Upstream retry hint, preserved so the client keeps receiving
        /// `Retry-After` without learning which provider asked for it.
        retry_after_ms: Option<u64>,
    },
}

impl CoreError {
    pub fn upstream(
        status: u16,
        provider: impl Into<String>,
        model: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self::UpstreamError {
            status,
            provider: provider.into(),
            model: model.into(),
            body: body.into(),
            is_proxy_rotated: false,
            class: crate::UpstreamErrorClass::Generic,
            is_hard_skip: false,
        }
    }

    pub fn upstream_retryable(
        status: u16,
        provider: impl Into<String>,
        model: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self::UpstreamError {
            status,
            provider: provider.into(),
            model: model.into(),
            body: body.into(),
            is_proxy_rotated: false,
            class: crate::UpstreamErrorClass::Generic,
            is_hard_skip: false,
        }
    }

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

    /// True when the failure happened inside an upstream target: pressure from
    /// a provider, a refused/failed HTTP call, a timeout, a connection loss or
    /// a combo with no usable target left.
    ///
    /// These variants name the provider, the model, the internal quota state
    /// and the upstream body, so they are the ones a public combo must never
    /// leak verbatim.
    #[inline]
    #[must_use]
    pub fn is_upstream_failure(&self) -> bool {
        matches!(
            self,
            CoreError::UpstreamError { .. }
                | CoreError::UpstreamTimeout { .. }
                | CoreError::UpstreamConnection(_)
                | CoreError::RateLimited { .. }
                | CoreError::NoHealthyTargets(_)
        )
    }

    /// True when the proxy itself is refusing the caller's own key: its prepaid
    /// plan balance is spent or one of its quota windows is closed.
    ///
    /// These errors are produced by this proxy, never by a target, so they are
    /// the one family the response filter must leave untouched: they are the
    /// caller's own limits and its only actionable information.
    #[inline]
    #[must_use]
    pub fn is_caller_quota_rejection(&self) -> bool {
        matches!(
            self,
            CoreError::QuotaExceeded { .. } | CoreError::InsufficientCredits { .. }
        )
    }

    /// Build the response a combo caller gets for an HTTP status that came out
    /// of a target.
    ///
    /// The class is the status itself, so the same variant answered with `400`
    /// or `500` produces different classes, and two different variants with the
    /// same status produce the same one. The mapping keeps the status when it
    /// is actionable and collapses the rest into availability, so a
    /// provider-side `401`/`402`/`403` never accuses the caller's own key of an
    /// operator-side credential or billing problem:
    ///
    /// - `400`, `413`, `422` → the payload was refused, keep the code;
    /// - `404` → nothing matched the requested model/combo, keep the code;
    /// - `408`, `504`, `529` → `504`, the model did not answer in time;
    /// - `429` → `429` plus its `Retry-After`, retrying later is correct;
    /// - anything else (`401`, `402`, `403`, `5xx`, capacity, internal) → `502`.
    ///
    /// `retry_after_ms` is carried through untouched: it is the only upstream
    /// detail that helps the caller and reveals nothing about the provider.
    #[must_use]
    pub fn masked_upstream(status: u16, retry_after_ms: Option<u64>) -> CoreError {
        const UNAVAILABLE: &str = "The model is temporarily unavailable. Retry the request.";
        const TIMEOUT: &str = "The model did not respond in time. Retry the request.";
        const RATE_LIMITED: &str =
            "The model is temporarily rate limited. Retry after the indicated delay.";
        const REJECTED: &str = "The request was rejected by the model provider.";
        const NOT_FOUND: &str = "The requested model or combo was not found.";

        let (status, message) = match status {
            400 | 413 | 422 => (status, REJECTED),
            404 => (404, NOT_FOUND),
            408 | 504 | 529 => (504, TIMEOUT),
            429 => (429, RATE_LIMITED),
            _ => (502, UNAVAILABLE),
        };
        CoreError::MaskedUpstream {
            status,
            message: message.to_string(),
            retry_after_ms,
        }
    }

    /// Mask this error according to the status it would answer.
    ///
    /// Convenience over [`CoreError::masked_upstream`] for callers that hold the
    /// error but not the recorded status. The pipeline prefers the recorded
    /// `status_code`, which is the target's real one even when the error was
    /// rebuilt into a different variant.
    ///
    /// Returns `None` when there is nothing to mask: no error response (a
    /// caller that disconnected, `499`) is not something to rewrite.
    #[must_use]
    pub fn masked_for_client(&self) -> Option<CoreError> {
        let source_status = self.http_status();
        if source_status < 400 || source_status == 499 {
            return None;
        }
        let retry_after_ms = match self {
            CoreError::RateLimited { retry_after_ms, .. } => Some(*retry_after_ms),
            CoreError::MaskedUpstream { retry_after_ms, .. } => *retry_after_ms,
            _ => None,
        };
        Some(CoreError::masked_upstream(source_status, retry_after_ms))
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
            CoreError::QuotaExceeded { .. } => 429,
            CoreError::InsufficientCredits { .. } => 402,
            CoreError::UpstreamError { status, .. } => *status,
            CoreError::UpstreamTimeout { .. } => 529,
            CoreError::UpstreamConnection(_) | CoreError::NoHealthyTargets(_) => 502,
            CoreError::Cancelled(CancelReason::ClientDisconnected) | CoreError::RaceLost => 499,
            CoreError::Cancelled(CancelReason::WatchdogTimeout) => 504,
            CoreError::ServiceUnavailable(_) | CoreError::JournalCapacityExhausted => 503,
            CoreError::MaskedUpstream { status, .. } => *status,
            _ => 500,
        }
    }

    /// Returns `true` if this error represents journal capacity exhaustion.
    #[inline]
    #[must_use]
    pub fn is_journal_capacity_exhausted(&self) -> bool {
        matches!(self, Self::JournalCapacityExhausted)
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
            CoreError::QuotaExceeded { .. } => "quota_exceeded",
            CoreError::InsufficientCredits { .. } => "insufficient_credits",
            CoreError::Parse(_) => "parse_error",
            CoreError::Cancelled(r) => r.as_str(),
            CoreError::RaceLost => "race_lost",
            CoreError::Database { .. } => "database",
            CoreError::Migration { .. } => "migration",
            CoreError::Config(_) => "config",
            CoreError::Internal(_) => "internal",
            CoreError::ServiceUnavailable(_) => "service_unavailable",
            CoreError::NotFound { .. } => "not_found",
            CoreError::JournalCapacityExhausted => "journal_capacity_exhausted",
            // One code for every masked target failure: the caller learns the
            // class from the status, never the internal phase/variant.
            CoreError::MaskedUpstream { .. } => "upstream_error",
        }
    }

    /// Reconstructs a [`CoreError`] from a canonical error code and message.
    pub fn from_code_and_message(code: &str, message: &str) -> Option<Self> {
        let msg = message.to_string();
        match code {
            "journal_capacity_exhausted" => Some(CoreError::JournalCapacityExhausted),
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
    fn ctx_parse(self, msg: impl fmt::Display) -> Result<T>;
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

    #[inline]
    fn ctx_parse(self, msg: impl fmt::Display) -> Result<T> {
        self.map_err(|e| CoreError::Parse(format!("{msg}: {e}")))
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
mod tests;
