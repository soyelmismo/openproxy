use crate::PipelineResult;
use crate::retry::RetryPolicy;
use openproxy_types::error::CoreError;
use std::sync::Arc;

/// Trigger for the `live_limited_models` insert. Pure so it can be
/// unit-tested without a full `Pipeline`.
pub(crate) fn should_mark_live_limited(err: &CoreError) -> bool {
    let CoreError::UpstreamError { status, body, .. } = err else {
        return false;
    };
    *status == 429 && body.contains("RESOURCE_EXHAUSTED")
}

/// Live-limit sentinel for `(account_id, model_id)`. The trigger reads
/// `(status, body)` directly, so it stays independent of GAP-4's
/// `UpstreamErrorClass`.
///
/// The writer lock is taken and released inside the `spawn_blocking`
/// closure: no guard crosses an `.await` (AGENTS.md §4.3). The handle is
/// dropped explicitly (fire-and-forget).
pub(crate) fn mark_live_limited_inner(
    conn_arc: Arc<parking_lot::Mutex<rusqlite::Connection>>,
    aid: openproxy_types::ids::AccountId,
    model_id: &openproxy_types::ids::ModelId,
    err: &CoreError,
) {
    if !should_mark_live_limited(err) {
        return;
    }
    let model_id = model_id.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let conn = conn_arc.lock();
        let until = (chrono::Utc::now() + chrono::Duration::minutes(5)).to_rfc3339();
        if let Err(e) = openproxy_db::live_limited::mark_limited(
            &conn,
            aid,
            &model_id,
            &until,
            "RESOURCE_EXHAUSTED",
        ) {
            tracing::warn!(
                account_id = aid.0,
                model = %model_id.as_str(),
                error = %e,
                "failed to mark live_limited_models from circuit breaker"
            );
        }
    });
    std::mem::drop(handle);
}

pub(crate) fn extract_error_summary(body: &str) -> Option<String> {
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(body)
        && let Some(msg) = val
            .get("error")
            .and_then(|e| e.get("message"))
            .or_else(|| val.get("message"))
            .and_then(|m| m.as_str())
    {
        return Some(msg.trim().to_string());
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    let end = trimmed
        .char_indices()
        .take(120)
        .last()
        .map_or(0, |(i, c)| i + c.len_utf8());
    Some(trimmed[..end].to_string())
}

pub(crate) fn detect_account_terminal_error(err: &CoreError) -> Option<String> {
    let CoreError::UpstreamError { status, body, .. } = err else {
        return None;
    };
    if *status == 402 {
        return Some(
            extract_error_summary(body)
                .unwrap_or_else(|| "Payment Required (insufficient balance)".into()),
        );
    }
    if *status == 401 {
        return Some(
            extract_error_summary(body)
                .unwrap_or_else(|| "Unauthorized (invalid or revoked API key)".into()),
        );
    }
    let lower = body.to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "insufficient balance",
        "insufficient_quota",
        "insufficient_balance",
        "credit balance is too low",
        "billing_hard_limit_reached",
        "account_deactivated",
    ];
    if PATTERNS.iter().any(|p| lower.contains(p)) {
        return Some(
            extract_error_summary(body)
                .unwrap_or_else(|| "Insufficient balance or quota exhausted".into()),
        );
    }
    if lower.contains("invalid_api_key") || lower.contains("incorrect api key") {
        return Some(
            extract_error_summary(body).unwrap_or_else(|| "Invalid or expired API key".into()),
        );
    }
    None
}

pub(crate) fn update_circuit_breaker_on_result(
    pipeline: &crate::Pipeline,
    target: &openproxy_types::ComboTarget,
    model: &openproxy_types::models::Model,
    result: &PipelineResult,
) {
    let Some(aid) = target.account_id else {
        return;
    };
    let key = crate::circuit_breaker::CircuitBreakerKey::from_target(
        aid,
        target.rate_limit_scope,
        target.model_row_id,
    );

    // Terminal account fault (402 balance exhausted, 401 invalid key).
    // Mark unhealthy in SQLite immediately and force circuit breaker open in memory.
    if let Some(err) = &result.error
        && let Some(reason) = detect_account_terminal_error(err)
    {
        tracing::warn!(account_id = aid.0, provider = %target.provider_id.as_str(), reason = %reason, "terminal account error; marking unhealthy");
        pipeline.circuit_breaker.force_unhealthy(key);
        let conn_clone = Arc::clone(&pipeline.conn);
        let handle = tokio::task::spawn_blocking(move || {
            let conn = conn_clone.lock();
            let _ = openproxy_db::accounts::set_health_with_error(
                &conn,
                aid,
                openproxy_types::accounts::HealthStatus::Unhealthy,
                Some(&reason),
            );
        });
        std::mem::drop(handle);
        return;
    }

    // The per-(account, model) live-limit sentinel has to be written
    // even when the breaker records a success below (the catch-all arm
    // for non-retryable errors, or the hard_skip arm). The decision is
    // a substring check on `429 RESOURCE_EXHAUSTED`, independent of
    // `error_classification::UpstreamErrorClass`.
    if let Some(err) = &result.error {
        mark_live_limited_inner(Arc::clone(&pipeline.conn), aid, &model.model_id, err);
    }

    match &result.error {
        Some(CoreError::Cancelled(openproxy_types::CancelReason::ClientDisconnected)) => {
            tracing::debug!(
                account_id = aid.0,
                "client cancelled; leaving circuit breaker untouched"
            );
        }
        // Request-shaped errors must not count against the breaker.
        Some(e) if e.is_hard_skip() => {
            tracing::debug!(
                account_id = aid.0,
                class = ?e.upstream_error_class(),
                "non-account error class; recording success to keep circuit breaker calm"
            );
            pipeline.circuit_breaker.record_success(key);
        }
        Some(e) if RetryPolicy::is_retryable(e, pipeline.config.idle_chunk_retryable) => {
            let outcome = pipeline.circuit_breaker.record_failure_outcome(key);
            if outcome.just_opened {
                notify_circuit_breaker_opened(
                    pipeline,
                    target,
                    model,
                    aid.0,
                    outcome.consecutive_failures.into(),
                    outcome.threshold.into(),
                );
            }
        }
        _ => {
            pipeline.circuit_breaker.record_success(key);
        }
    }
}

pub(crate) fn update_predictive_limiter_on_result(
    pipeline: &crate::Pipeline,
    target: &openproxy_types::ComboTarget,
    result: &PipelineResult,
) {
    let key = crate::predictive_rate_limit::PredictiveRateLimiter::compute_target_key(target);
    let now = crate::predictive_rate_limit::PredictiveRateLimiter::now_ms();
    match &result.error {
        None => {
            pipeline
                .predictive_limiter
                .report_success_key(key, None, None, now);
        }
        Some(CoreError::Cancelled(_) | CoreError::RaceLost) => {
            pipeline.predictive_limiter.release_in_flight_key(key);
        }
        Some(CoreError::RateLimited { retry_after_ms, .. }) => {
            let retry_after_secs = Some(*retry_after_ms / 1000);
            pipeline
                .predictive_limiter
                .report_rate_limited_key(key, retry_after_secs, now);
        }
        Some(_) => {
            record_predictive_limiter_upstream_error(&pipeline.predictive_limiter, key, result, now)
        }
    }
}

pub(crate) fn update_account_rate_limited_until_on_result(
    pipeline: &crate::Pipeline,
    target: &openproxy_types::ComboTarget,
    result: &PipelineResult,
) {
    let Some(aid) = target.account_id else {
        return;
    };
    if let Some(err) = &result.error {
        let retry_secs = match err {
            CoreError::RateLimited { retry_after_ms, .. } => Some((*retry_after_ms / 1000).max(5)),
            _ if result.status_code == 429 => Some(60),
            _ => None,
        };
        if let Some(secs) = retry_secs {
            let until = (chrono::Utc::now() + chrono::Duration::seconds(secs as i64)).to_rfc3339();
            let conn_clone = Arc::clone(&pipeline.conn);
            let handle = tokio::task::spawn_blocking(move || {
                let conn = conn_clone.lock();
                if let Err(e) =
                    openproxy_db::accounts::set_rate_limited_until(&conn, aid, Some(&until))
                {
                    tracing::warn!(account_id = aid.0, error = %e, "failed to update rate_limited_until");
                }
            });
            std::mem::drop(handle);
        }
    } else {
        let conn_clone = Arc::clone(&pipeline.conn);
        let handle = tokio::task::spawn_blocking(move || {
            let conn = conn_clone.lock();
            let _ = openproxy_db::accounts::set_rate_limited_until(&conn, aid, None);
        });
        std::mem::drop(handle);
    }
}

pub(crate) fn record_predictive_limiter_upstream_error(
    limiter: &crate::predictive_rate_limit::PredictiveRateLimiter,
    key: u64,
    result: &PipelineResult,
    now: u64,
) {
    if result.status_code == 429 {
        limiter.report_rate_limited_key(key, None, now);
    } else {
        let fingerprint = result
            .error
            .as_ref()
            .map_or(0, crate::predictive_rate_limit::compute_error_fingerprint);
        limiter.report_upstream_error_with_fingerprint_key(key, fingerprint, now);
    }
}

pub(crate) fn notify_circuit_breaker_opened(
    pipeline: &crate::Pipeline,
    target: &openproxy_types::ComboTarget,
    model: &openproxy_types::models::Model,
    account_id: i64,
    consecutive_failures: u32,
    threshold: u32,
) {
    let provider_id_str = target.provider_id.to_string();
    let model_id_str = model.model_id.as_str().to_string();
    let combo_target_id = target.id.0;
    let dedup_key = format!("circuit_open:{account_id}");
    let payload = serde_json::json!({
        "code": "circuit_open",
        "message": format!(
            "Circuit breaker opened for account {} on {} ({}) — {}/{} failures",
            account_id, provider_id_str, model_id_str,
            consecutive_failures, threshold,
        ),
        "provider_id": &provider_id_str,
        "details": {
            "combo_target_id": combo_target_id,
            "account_id": account_id,
            "provider_id": &provider_id_str,
            "model_id": &model_id_str,
            "failure_count": consecutive_failures,
            "threshold": threshold,
        },
    });
    let repo = pipeline.repo();
    tokio::task::spawn_blocking(move || {
        let _ = repo.insert_and_broadcast_notification(
            "system",
            &payload,
            Some(&dedup_key),
            Some(&provider_id_str),
        );
    });
}
