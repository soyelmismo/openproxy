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
    if target.provider_id.as_str() != "zai"
        && let Some(err) = &result.error
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

/// Classify a Devin daily/weekly usage-quota exhaustion. Only explicit
/// account-wide quota exhaustion on a 429 upgrades the cooldown to the
/// window's persisted reset; monthly ACU messages, generic rate limits and
/// explicit `RateLimited` errors (which already carry `retry_after_ms`,
/// matching upstream's "explicit Retry-After wins" rule) keep the default
/// backoff. Pure so it can be unit-tested without a full `Pipeline`.
fn devin_quota_exhaustion_kind(
    err: &CoreError,
    status_code: u16,
    provider_id: &str,
) -> Option<openproxy_adapters::adapters::devin::DevinQuotaKind> {
    let CoreError::UpstreamError { body, .. } = err else {
        return None;
    };
    if status_code != 429 || provider_id != "devin" {
        return None;
    }
    openproxy_adapters::adapters::devin::classify_devin_quota_exhaustion(
        openproxy_adapters::adapters::devin::devin_error_body_message(body),
    )
}

/// Upstream parks a Devin credential until the reset of the quota window
/// named in the exhaustion message (8-day plausibility cap, matching
/// `devinQuotaResetMaxDelay`). Returns the reset instant as RFC3339, or
/// `None` when no plausible reset is stored — the caller then keeps the
/// default backoff.
fn devin_quota_reset_until(
    conn: &rusqlite::Connection,
    aid: openproxy_types::ids::AccountId,
    kind: openproxy_adapters::adapters::devin::DevinQuotaKind,
) -> Option<String> {
    use openproxy_adapters::adapters::devin::DevinQuotaKind;
    let (session_reset, weekly_reset) = openproxy_db::accounts::quota_reset_at(conn, aid).ok()?;
    let reset = match kind {
        DevinQuotaKind::Daily => session_reset,
        DevinQuotaKind::Weekly => weekly_reset,
    }?;
    let reset_at = chrono::DateTime::parse_from_rfc3339(&reset).ok()?;
    let delay = reset_at.timestamp() - chrono::Utc::now().timestamp();
    (delay > 0 && delay <= 8 * 24 * 3600).then(|| reset_at.to_rfc3339())
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
        // Devin reports daily/weekly usage-quota exhaustion in the trailer
        // error body; upstream parks the credential until that window's
        // persisted reset instead of a fixed backoff (devin_quota.go).
        let devin_quota =
            devin_quota_exhaustion_kind(err, result.status_code, target.provider_id.as_str());
        let retry_secs = match err {
            CoreError::RateLimited { retry_after_ms, .. } => Some((*retry_after_ms / 1000).max(5)),
            _ if result.status_code == 429 => Some(60),
            _ => None,
        };
        if let Some(secs) = retry_secs {
            let conn_clone = Arc::clone(&pipeline.conn);
            let handle = tokio::task::spawn_blocking(move || {
                let conn = conn_clone.lock();
                let until = devin_quota
                    .and_then(|kind| devin_quota_reset_until(&conn, aid, kind))
                    .unwrap_or_else(|| {
                        (chrono::Utc::now() + chrono::Duration::seconds(secs as i64)).to_rfc3339()
                    });
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
            // Success path runs on every request: only touch the row when a
            // limit is actually set, so the common case writes nothing (no
            // dirty page, no WAL frame, no fsync on the serialized writer).
            if let Err(e) = openproxy_db::accounts::clear_rate_limited_until(&conn, aid) {
                tracing::warn!(account_id = aid.0, error = %e, "failed to clear rate_limited_until");
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_adapters::adapters::devin::DevinQuotaKind;

    fn devin_err(status: u16, body: &str) -> CoreError {
        CoreError::UpstreamError {
            status,
            provider: "devin".into(),
            model: "m".into(),
            body: body.into(),
            is_proxy_rotated: false,
            class: crate::error_classification::classify_upstream_error(status, body),
            is_hard_skip: false,
        }
    }

    #[test]
    fn devin_quota_kind_matches_explicit_window_exhaustion() {
        assert_eq!(
            devin_quota_exhaustion_kind(
                &devin_err(
                    429,
                    "devin upstream error (resource_exhausted): daily quota exhausted"
                ),
                429,
                "devin"
            ),
            Some(DevinQuotaKind::Daily)
        );
        assert_eq!(
            devin_quota_exhaustion_kind(
                &devin_err(
                    429,
                    "devin upstream error (resource_exhausted): Weekly quota exhausted"
                ),
                429,
                "devin"
            ),
            Some(DevinQuotaKind::Weekly)
        );
        // Weekly wins when both periods are named.
        assert_eq!(
            devin_quota_exhaustion_kind(
                &devin_err(
                    429,
                    "devin upstream error (resource_exhausted): daily and weekly quota exhausted"
                ),
                429,
                "devin"
            ),
            Some(DevinQuotaKind::Weekly)
        );
    }

    #[test]
    fn devin_quota_kind_rejects_non_window_messages() {
        // Monthly ACU exhaustion is not a daily/weekly window.
        assert_eq!(
            devin_quota_exhaustion_kind(
                &devin_err(
                    429,
                    "devin upstream error (resource_exhausted): monthly acu quota exhausted"
                ),
                429,
                "devin"
            ),
            None
        );
        // Generic rate limit: no "quota" word.
        assert_eq!(
            devin_quota_exhaustion_kind(
                &devin_err(
                    429,
                    "devin upstream error (resource_exhausted): high demand"
                ),
                429,
                "devin"
            ),
            None
        );
        // The wrapper's code ("resource_exhausted") must not supply the
        // "exhausted" word: raw message lacks it.
        assert_eq!(
            devin_quota_exhaustion_kind(
                &devin_err(
                    429,
                    "devin upstream error (resource_exhausted): weekly quota limit"
                ),
                429,
                "devin"
            ),
            None
        );
        // Non-429 status, non-devin provider, and RateLimited (explicit
        // retry_after_ms takes precedence upstream) never match.
        assert_eq!(
            devin_quota_exhaustion_kind(
                &devin_err(400, "devin upstream error (x): daily quota exhausted"),
                400,
                "devin"
            ),
            None
        );
        assert_eq!(
            devin_quota_exhaustion_kind(
                &devin_err(429, "devin upstream error (x): daily quota exhausted"),
                429,
                "other"
            ),
            None
        );
        assert_eq!(
            devin_quota_exhaustion_kind(
                &CoreError::RateLimited {
                    provider: "devin".into(),
                    retry_after_ms: 1000,
                    is_proxy_rotated: false,
                },
                429,
                "devin"
            ),
            None
        );
    }

    fn account_with_resets(session: Option<&str>, weekly: Option<&str>) -> rusqlite::Connection {
        let conn = openproxy_db::test_db::open_in_memory();
        openproxy_db::test_db::seed_antigravity_provider(&conn);
        conn.execute(
            "INSERT INTO accounts(provider_id, label) VALUES ('antigravity', 'a1')",
            [],
        )
        .expect("insert account");
        conn.execute(
            "UPDATE accounts SET quota_session_reset_at = ?1, quota_weekly_reset_at = ?2 \
             WHERE id = 1",
            rusqlite::params![session, weekly],
        )
        .expect("set resets");
        conn
    }

    #[test]
    fn devin_quota_reset_until_uses_window_reset() {
        let future = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let weekly_future = (chrono::Utc::now() + chrono::Duration::days(3)).to_rfc3339();
        let conn = account_with_resets(Some(&future), Some(&weekly_future));
        let aid = openproxy_types::ids::AccountId(1);

        let daily = devin_quota_reset_until(&conn, aid, DevinQuotaKind::Daily).expect("daily");
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(&daily)
                .unwrap()
                .timestamp(),
            chrono::DateTime::parse_from_rfc3339(&future)
                .unwrap()
                .timestamp()
        );
        let weekly = devin_quota_reset_until(&conn, aid, DevinQuotaKind::Weekly).expect("weekly");
        assert_eq!(
            chrono::DateTime::parse_from_rfc3339(&weekly)
                .unwrap()
                .timestamp(),
            chrono::DateTime::parse_from_rfc3339(&weekly_future)
                .unwrap()
                .timestamp()
        );
    }

    #[test]
    fn devin_quota_reset_until_rejects_unusable_resets() {
        let aid = openproxy_types::ids::AccountId(1);
        let future = (chrono::Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let past = (chrono::Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
        let far = (chrono::Utc::now() + chrono::Duration::days(9)).to_rfc3339();

        // Past timestamp.
        let conn = account_with_resets(Some(&past), Some(&future));
        assert_eq!(
            devin_quota_reset_until(&conn, aid, DevinQuotaKind::Daily),
            None
        );
        assert!(devin_quota_reset_until(&conn, aid, DevinQuotaKind::Weekly).is_some());

        // Beyond the 8-day plausibility cap.
        let conn = account_with_resets(Some(&far), None);
        assert_eq!(
            devin_quota_reset_until(&conn, aid, DevinQuotaKind::Daily),
            None
        );

        // Unparseable timestamp.
        let conn = account_with_resets(Some("not-a-date"), None);
        assert_eq!(
            devin_quota_reset_until(&conn, aid, DevinQuotaKind::Daily),
            None
        );

        // Window without a stored reset, and a missing account row.
        let conn = account_with_resets(None, Some(&future));
        assert_eq!(
            devin_quota_reset_until(&conn, aid, DevinQuotaKind::Daily),
            None
        );
        assert_eq!(
            devin_quota_reset_until(
                &conn,
                openproxy_types::ids::AccountId(999),
                DevinQuotaKind::Daily
            ),
            None
        );
    }
}
