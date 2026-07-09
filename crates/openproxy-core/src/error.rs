//! Crate-wide error types. Every error carries a request_id and trace_id for traceability.

use crate::ids::{RequestId, TraceId};
use std::fmt;
use thiserror::Error;

pub fn map_db_error<E: std::error::Error + Send + Sync + 'static>(e: E) -> CoreError {
    CoreError::Database {
        message: e.to_string(),
        source: Some(Box::new(e)),
    }
}

pub fn map_db_error_ctx<E: std::error::Error + Send + Sync + 'static>(
    ctx: impl Into<String>,
) -> impl FnOnce(E) -> CoreError {
    let ctx = ctx.into();
    move |e| CoreError::Database {
        message: format!("{}: {}", ctx, e),
        source: Some(Box::new(e)),
    }
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

impl fmt::Display for ErrorContext {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "req={} trace={} phase={}",
            self.request_id, self.trace_id, self.phase
        )?;
        if let Some(p) = &self.provider {
            write!(f, " provider={}", p)?;
        }
        if let Some(a) = self.account {
            write!(f, " account={}", a)?;
        }
        if let Some(m) = &self.model {
            write!(f, " model={}", m)?;
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("config: {0}")]
    Config(String),

    #[error("database: {message}")]
    Database {
        message: String,
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
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

    #[error("client disconnected")]
    ClientDisconnected,

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
    /// Produce a best-effort clone that drops any non-cloneable boxed source.
    ///
    /// `CoreError` cannot derive `Clone` because the `Database` variant holds a
    /// `Box<dyn Error + Send + Sync>`. Callers that need to ship a
    /// [`CoreError`] across an async boundary (e.g. into a `PipelineResult`)
    /// use this method, which copies the textual message and drops the
    /// source. The semantic content of the error is preserved: the variant,
    /// the message, and any structured fields (status, retry_after_ms,
    /// provider, model, account) survive.
    pub fn clone_for_result(&self) -> CoreError {
        match self {
            CoreError::Config(s) => CoreError::Config(s.clone()),
            CoreError::Database { message, source: _ } => CoreError::Database {
                message: message.clone(),
                source: None,
            },
            CoreError::Migration { version, message } => CoreError::Migration {
                version: *version,
                message: message.clone(),
            },
            CoreError::ProviderNotFound(s) => CoreError::ProviderNotFound(s.clone()),
            CoreError::AccountNotFound(i) => CoreError::AccountNotFound(*i),
            CoreError::ComboNotFound(i) => CoreError::ComboNotFound(*i),
            CoreError::ModelNotFound { provider, model } => CoreError::ModelNotFound {
                provider: provider.clone(),
                model: model.clone(),
            },
            CoreError::NoHealthyTargets(i) => CoreError::NoHealthyTargets(*i),
            CoreError::UpstreamTimeout { phase, ms } => CoreError::UpstreamTimeout {
                phase: phase.clone(),
                ms: *ms,
            },
            CoreError::UpstreamError {
                status,
                provider,
                model,
                body,
                is_proxy_rotated,
            } => CoreError::UpstreamError {
                status: *status,
                provider: provider.clone(),
                model: model.clone(),
                body: body.clone(),
                is_proxy_rotated: *is_proxy_rotated,
            },
            CoreError::UpstreamConnection(s) => CoreError::UpstreamConnection(s.clone()),
            CoreError::RateLimited {
                provider,
                retry_after_ms,
                is_proxy_rotated,
            } => CoreError::RateLimited {
                provider: provider.clone(),
                retry_after_ms: *retry_after_ms,
                is_proxy_rotated: *is_proxy_rotated,
            },
            CoreError::Parse(s) => CoreError::Parse(s.clone()),
            CoreError::ClientDisconnected => CoreError::ClientDisconnected,
            CoreError::RaceLost => CoreError::RaceLost,
            CoreError::Auth(s) => CoreError::Auth(s.clone()),
            CoreError::Validation(s) => CoreError::Validation(s.clone()),
            CoreError::Internal(s) => CoreError::Internal(s.clone()),
            CoreError::ServiceUnavailable(s) => CoreError::ServiceUnavailable(s.clone()),
            CoreError::NotFound { what, id } => CoreError::NotFound {
                what: what.clone(),
                id: id.clone(),
            },
        }
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
            CoreError::ClientDisconnected => 499,
            CoreError::RaceLost => 499,
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
            CoreError::ClientDisconnected => "client_disconnected",
            CoreError::RaceLost => "race_lost",
            CoreError::Database { .. } => "database",
            CoreError::Migration { .. } => "migration",
            CoreError::Config(_) => "config",
            CoreError::Internal(_) => "internal",
            CoreError::ServiceUnavailable(_) => "service_unavailable",
            CoreError::NotFound { .. } => "not_found",
        }
    }
}

pub type Result<T> = std::result::Result<T, CoreError>;

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(CoreError::ClientDisconnected.http_status(), 499);
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
            CoreError::ClientDisconnected,
        ] {
            codes.insert(err.code());
        }
        assert_eq!(codes.len(), 5);
    }
}
