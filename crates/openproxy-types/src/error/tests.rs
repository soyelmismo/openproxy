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

#[test]
fn test_journal_capacity_exhausted() {
    let err = CoreError::JournalCapacityExhausted;
    assert!(err.is_journal_capacity_exhausted());
    assert_eq!(
        err.to_string(),
        "usage journal capacity exhausted; admission rejected"
    );
    assert_eq!(err.http_status(), 503);
    assert_eq!(err.code(), "journal_capacity_exhausted");
    assert!(matches!(
        CoreError::from_code_and_message("journal_capacity_exhausted", ""),
        Some(CoreError::JournalCapacityExhausted)
    ));
    assert!(!CoreError::Internal("test".into()).is_journal_capacity_exhausted());
}

fn upstream_error(status: u16) -> CoreError {
    CoreError::UpstreamError {
        status,
        provider: "openai".into(),
        model: "gpt-4o".into(),
        body: r#"{"error":{"message":"Incorrect API key provided: sk-proj-SECRET"}}"#.into(),
        is_proxy_rotated: false,
        class: crate::UpstreamErrorClass::Generic,
        is_hard_skip: false,
    }
}

#[test]
fn masked_for_client_drops_provider_model_and_body() {
    for status in [400u16, 401, 403, 404, 413, 422, 500, 502, 503] {
        let masked = upstream_error(status)
            .masked_for_client()
            .expect("an error status is always masked");
        let text = masked.to_string();
        assert!(
            !text.contains("openai"),
            "{status}: leaked provider in {text}"
        );
        assert!(!text.contains("gpt-4o"), "{status}: leaked model in {text}");
        assert!(!text.contains("sk-proj"), "{status}: leaked body in {text}");
        assert_eq!(masked.code(), "upstream_error");
        assert!(matches!(masked, CoreError::MaskedUpstream { .. }));
    }
}

#[test]
fn masked_for_client_keeps_the_actionable_class() {
    // The caller's payload was refused: it can fix it, so the code survives.
    for status in [400u16, 413, 422] {
        let masked = upstream_error(status).masked_for_client().unwrap();
        assert_eq!(masked.http_status(), status);
    }
    // A missing model/combo stays a 404, with a generic message.
    let not_found = upstream_error(404).masked_for_client().unwrap();
    assert_eq!(not_found.http_status(), 404);
    assert!(!not_found.to_string().contains("gpt-4o"));
    // An upstream auth/billing/quota failure must not accuse the caller's
    // key: it leaves as availability, not as 401/402/403.
    for status in [401u16, 402, 403, 500, 503] {
        let masked = upstream_error(status).masked_for_client().unwrap();
        assert_eq!(masked.http_status(), 502, "status {status}");
    }
    // Upstream pressure keeps its class: retrying later is correct.
    let limited = upstream_error(429).masked_for_client().unwrap();
    assert_eq!(limited.http_status(), 429);
    let timeout = CoreError::UpstreamTimeout {
        phase: "stream".into(),
        ms: 30_000,
    }
    .masked_for_client()
    .unwrap();
    assert_eq!(timeout.http_status(), 504);
    let conn = CoreError::UpstreamConnection("dial tcp 10.0.0.4:443".into())
        .masked_for_client()
        .unwrap();
    assert_eq!(conn.http_status(), 502);
    assert!(!conn.to_string().contains("10.0.0.4"));
}

#[test]
fn masked_for_client_keeps_retry_after_without_the_provider() {
    let masked = CoreError::RateLimited {
        provider: "anthropic".into(),
        retry_after_ms: 12_000,
        is_proxy_rotated: true,
    }
    .masked_for_client()
    .unwrap();
    assert_eq!(masked.http_status(), 429);
    assert!(matches!(
        masked,
        CoreError::MaskedUpstream {
            status: 429,
            retry_after_ms: Some(12_000),
            ..
        }
    ));
    assert!(!masked.to_string().contains("anthropic"));
}

#[test]
fn masked_for_client_has_nothing_to_do_without_an_error_response() {
    // 499: the caller closed the request; there is no response to rewrite.
    assert!(
        CoreError::Cancelled(CancelReason::ClientDisconnected)
            .masked_for_client()
            .is_none()
    );
}

#[test]
fn the_caller_own_limits_are_excluded_by_the_caller_not_by_the_mapping() {
    // These carry status 429/402, so the status mapping would rewrite them.
    // The pipeline checks `is_caller_quota_rejection` first and never calls
    // the mapping for them: their message and Retry-After reach the client.
    let quota = CoreError::QuotaExceeded {
        window: "daily".into(),
        spent_usd: 5.0,
        cap_usd: 5.0,
        resets_at: "2026-11-15T00:00:00Z".into(),
    };
    assert!(quota.is_caller_quota_rejection());
    assert!(CoreError::InsufficientCredits { balance_usd: 0.0 }.is_caller_quota_rejection());
    // Nothing else is exempt.
    assert!(!upstream_error(429).is_caller_quota_rejection());
    assert!(
        !CoreError::RateLimited {
            provider: "x".into(),
            retry_after_ms: 1,
            is_proxy_rotated: false,
        }
        .is_caller_quota_rejection()
    );
}
