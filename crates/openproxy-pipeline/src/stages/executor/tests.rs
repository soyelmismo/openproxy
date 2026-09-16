use super::helpers::{is_max_request_timeout, should_skip_preventive_target};
use openproxy_types::{CancelReason, CoreError, UpstreamErrorClass};

#[test]
fn test_is_max_request_timeout() {
    let total_timeout = CoreError::UpstreamTimeout {
        phase: "total".to_string(),
        ms: 30000,
    };
    assert!(is_max_request_timeout(&total_timeout));

    let total_ms_timeout = CoreError::UpstreamTimeout {
        phase: "total_ms".to_string(),
        ms: 30000,
    };
    assert!(is_max_request_timeout(&total_ms_timeout));

    let headers_timeout = CoreError::UpstreamTimeout {
        phase: "headers".to_string(),
        ms: 5000,
    };
    assert!(!is_max_request_timeout(&headers_timeout));

    let watchdog = CoreError::Cancelled(CancelReason::WatchdogTimeout);
    assert!(is_max_request_timeout(&watchdog));

    let client_disc = CoreError::Cancelled(CancelReason::ClientDisconnected);
    assert!(!is_max_request_timeout(&client_disc));

    let gateway_timeout = CoreError::UpstreamError {
        status: 504,
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        body: "gateway timeout while waiting for total response".to_string(),
        is_proxy_rotated: false,
        class: UpstreamErrorClass::Generic,
        is_hard_skip: false,
    };
    assert!(is_max_request_timeout(&gateway_timeout));

    let internal_error = CoreError::UpstreamError {
        status: 500,
        provider: "openai".to_string(),
        model: "gpt-4o".to_string(),
        body: "internal server error".to_string(),
        is_proxy_rotated: false,
        class: UpstreamErrorClass::Generic,
        is_hard_skip: false,
    };
    assert!(!is_max_request_timeout(&internal_error));
}

#[test]
fn test_should_skip_preventive_target_disabled_cooldown() {
    use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
    use openproxy_types::config::CooldownMode;
    use openproxy_types::providers::RateLimitScope;
    use openproxy_types::{ComboId, ComboTargetId};

    let limiter = crate::predictive_rate_limit::PredictiveRateLimiter::new();

    let mut combo = Combo {
        id: ComboId(1),
        name: "test".into(),
        strategy: Strategy::Priority,
        race_size: 1,
        preventive_rate_limit: true,
        created_at: "now".into(),
        context_window: None,
        priority_mode: PriorityMode::Strict,
        cooldown_mode: CooldownMode::Flat,
        cooldown_base_secs: Some(60),
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
    };

    let target_a = crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(1),
            combo_id: ComboId(1),
            provider_id: openproxy_types::ProviderId("openai".into()),
            account_id: Some(openproxy_types::AccountId(1)),
            model_row_id: None,
            sub_combo_id: None,
            priority_order: 1,
            weight: 1,
            active: true,
            rate_limit_scope: RateLimitScope::Account,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            thinking_effort: None,
        },
        model: openproxy_types::models::Model {
            row_id: openproxy_types::ModelRowId(1),
            provider_id: openproxy_types::ProviderId("openai".into()),
            model_id: "gpt-4o".into(),
            ..Default::default()
        },
        api_key: "key1".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let target_b = crate::context::ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(2),
            account_id: Some(openproxy_types::AccountId(2)),
            ..target_a.target.clone()
        },
        model: target_a.model.clone(),
        api_key: "key2".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let now_ms = crate::predictive_rate_limit::PredictiveRateLimiter::now_ms();
    let key_a =
        crate::predictive_rate_limit::PredictiveRateLimiter::compute_target_key(&target_a.target);

    // Saturate target A
    limiter.report_rate_limited_key(key_a, Some(60), now_ms);

    // When cooldown is enabled on target A and B is healthy -> should skip A
    assert!(should_skip_preventive_target(
        &limiter,
        &combo,
        &target_a,
        key_a,
        std::slice::from_ref(&target_b),
        now_ms,
    ));

    // When target A explicitly disables cooldown via cooldown_mode -> DO NOT skip A
    let mut target_a_none = target_a.clone();
    target_a_none.target.cooldown_mode = Some(CooldownMode::None);
    assert!(!should_skip_preventive_target(
        &limiter,
        &combo,
        &target_a_none,
        key_a,
        std::slice::from_ref(&target_b),
        now_ms,
    ));

    // When target A explicitly disables cooldown via cooldown_base_secs = 0 -> DO NOT skip A
    let mut target_a_base0 = target_a.clone();
    target_a_base0.target.cooldown_base_secs = Some(0);
    assert!(!should_skip_preventive_target(
        &limiter,
        &combo,
        &target_a_base0,
        key_a,
        std::slice::from_ref(&target_b),
        now_ms,
    ));

    // When combo disables cooldown -> DO NOT skip A
    combo.cooldown_mode = CooldownMode::None;
    assert!(!should_skip_preventive_target(
        &limiter,
        &combo,
        &target_a,
        key_a,
        std::slice::from_ref(&target_b),
        now_ms,
    ));

    // When target B is also saturated in limiter, but target B has cooldown disabled,
    // target A (cooldown enabled) CAN skip because target B is an available healthy fallback
    combo.cooldown_mode = CooldownMode::Flat;
    let key_b =
        crate::predictive_rate_limit::PredictiveRateLimiter::compute_target_key(&target_b.target);
    limiter.report_rate_limited_key(key_b, Some(60), now_ms);

    let mut target_b_disabled = target_b;
    target_b_disabled.target.cooldown_mode = Some(CooldownMode::None);
    assert!(should_skip_preventive_target(
        &limiter,
        &combo,
        &target_a,
        key_a,
        &[target_b_disabled],
        now_ms,
    ));
}
