use super::*;

#[test]
fn test_predictive_learning_and_recovery_cycle() {
    let limiter = PredictiveRateLimiter::new();
    let (combo, target) = (ComboId(1), ComboTargetId(10));
    let mut now = 100_000;

    for _ in 0..2 {
        assert!(limiter.acquire_target(combo, target, now));
        limiter.report_success(combo, target, None, None, now);
    }
    assert!(limiter.acquire_target(combo, target, now));
    limiter.report_rate_limited(combo, target, Some(60), now);

    match limiter.evaluate_target(combo, target, now) {
        TargetReadiness::Saturated { learned_burst, .. } => assert_eq!(learned_burst, 2),
        _ => panic!("must be saturated"),
    }
    assert!(!limiter.acquire_target(combo, target, now));

    now += 61_000;
    assert_eq!(
        limiter.evaluate_target(combo, target, now),
        TargetReadiness::Probe
    );
    assert!(limiter.acquire_target(combo, target, now));
    assert!(!limiter.acquire_target(combo, target, now));

    limiter.report_success(combo, target, None, None, now);
    assert_eq!(
        limiter.evaluate_target(combo, target, now),
        TargetReadiness::Ready
    );
}

#[test]
fn test_elastic_additive_increase() {
    let limiter = PredictiveRateLimiter::new();
    let (combo, target) = (ComboId(1), ComboTargetId(20));
    let now = 100_000;

    assert!(limiter.acquire_target(combo, target, now));
    limiter.report_success(combo, target, None, None, now);
    assert!(limiter.acquire_target(combo, target, now));
    limiter.report_rate_limited(combo, target, Some(10), now);

    match limiter.evaluate_target(combo, target, now) {
        TargetReadiness::Saturated { learned_burst, .. } => assert_eq!(learned_burst, 1),
        _ => panic!("must be saturated"),
    }

    let after_cd = now + 15_000;
    assert!(limiter.acquire_target(combo, target, after_cd));
    limiter.report_success(combo, target, None, None, after_cd);

    for i in 0..2 {
        limiter.report_success(combo, target, None, None, after_cd + 1000 * (i + 1));
    }

    let key = PredictiveRateLimiter::compute_key(combo, target);
    let state = limiter
        .shard_for(key)
        .inner
        .read()
        .get(&key)
        .cloned()
        .unwrap();
    assert_eq!(state.learned_burst, 3);
}

#[test]
fn test_independent_target_prediction_isolation() {
    let limiter = PredictiveRateLimiter::new();
    let (combo, ta, tb) = (ComboId(1), ComboTargetId(101), ComboTargetId(102));
    let now = 100_000;
    assert!(limiter.acquire_target(combo, ta, now));
    limiter.report_rate_limited(combo, ta, Some(60), now);
    assert!(!limiter.acquire_target(combo, ta, now));
    assert_eq!(
        limiter.evaluate_target(combo, tb, now),
        TargetReadiness::Ready
    );
    assert!(limiter.acquire_target(combo, tb, now));
}

#[test]
fn test_chain_skipping_sequential_decision() {
    let limiter = PredictiveRateLimiter::new();
    let (c, t1, t2, t3) = (
        ComboId(1),
        ComboTargetId(201),
        ComboTargetId(202),
        ComboTargetId(203),
    );
    let now = 100_000;
    assert!(limiter.acquire_target(c, t1, now));
    limiter.report_success(c, t1, None, None, now);
    assert!(limiter.acquire_target(c, t1, now));
    limiter.report_rate_limited(c, t1, Some(60), now);

    assert!(matches!(
        limiter.evaluate_target(c, t1, now),
        TargetReadiness::Saturated {
            learned_burst: 1,
            ..
        }
    ));
    assert_eq!(limiter.evaluate_target(c, t2, now), TargetReadiness::Ready);
    assert!(limiter.acquire_target(c, t2, now));
    limiter.report_success(c, t2, None, None, now);
    assert_eq!(limiter.evaluate_target(c, t3, now), TargetReadiness::Ready);
}

#[test]
fn test_upstream_error_blocks_and_recovers() {
    let limiter = PredictiveRateLimiter::new();
    let (c, t) = (ComboId(1), ComboTargetId(300));
    let now = 100_000;

    assert!(limiter.acquire_target(c, t, now));
    limiter.report_upstream_error(c, t, now);
    assert!(limiter.evaluate_target(c, t, now).is_saturated());

    let key = PredictiveRateLimiter::compute_key(c, t);
    let reset_at = limiter
        .shard_for(key)
        .inner
        .read()
        .get(&key)
        .unwrap()
        .reset_at_ms;

    let after = reset_at + 1;
    assert_eq!(limiter.evaluate_target(c, t, after), TargetReadiness::Probe);
    assert!(limiter.acquire_target(c, t, after));
    limiter.report_success(c, t, None, None, after);
    assert_eq!(limiter.evaluate_target(c, t, after), TargetReadiness::Ready);
}

#[test]
fn test_success_closes_open_state_before_reset() {
    // Regresión: opencode-zen devolvió 429 y el estado Open sobrevivió a un 200
    // real del mismo provider+modelo (petición directa con combo_id=0).
    // `apply_success` solo cerraba el estado HalfOpen, así que el target seguía
    // saturado hasta el reset pese a la evidencia de que responde.
    let limiter = PredictiveRateLimiter::new();
    let key = PredictiveRateLimiter::compute_key_parts(
        &ProviderId::new("opencode-zen"),
        None,
        Some(ModelRowId(12_713_086)),
        RateLimitScope::Account,
    );
    let t0 = 1_000_000;

    assert!(limiter.acquire_key(key, t0));
    limiter.report_rate_limited_key(key, Some(60), t0);
    assert!(limiter.evaluate_key(key, t0 + 1).is_saturated());

    let t_ok = t0 + 5_000;
    limiter.report_success_key(key, None, None, t_ok);
    assert!(
        !limiter.evaluate_key(key, t_ok).is_saturated(),
        "un 200 real demuestra que la ventana no está agotada: el estado Open debe cerrarse"
    );
}

#[test]
fn test_half_open_probe_slot_does_not_pin_target_forever() {
    // Regresión observada en producción: skips con `reset_in_ms = 0` durante
    // horas. La causa es un `in_flight` huérfano tras el cambio Open→HalfOpen
    // (camino de salida temprano que no reporta resultado): el target queda
    // Saturado para siempre y nunca se reintenta.
    let limiter = PredictiveRateLimiter::new();
    let (combo, target) = (ComboId(7), ComboTargetId(701));
    let t0 = 5_000_000;

    assert!(limiter.acquire_target(combo, target, t0));
    limiter.report_rate_limited(combo, target, Some(3), t0);

    let after_open = t0 + 3_001;
    assert_eq!(
        limiter.evaluate_target(combo, target, after_open),
        TargetReadiness::Probe
    );
    assert!(limiter.acquire_target(combo, target, after_open));
    // Nadie reporta el resultado de este probe: el slot queda ocupado.
    assert!(
        limiter
            .evaluate_target(combo, target, after_open + 1)
            .is_saturated()
    );

    let after_lease = after_open + PROBE_LEASE_MS + 1;
    assert_eq!(
        limiter.evaluate_target(combo, target, after_lease),
        TargetReadiness::Probe,
        "el lease debe liberar el slot huérfano y permitir un nuevo probe"
    );
    assert!(limiter.acquire_target(combo, target, after_lease));
}

#[test]
fn test_rate_limited_penalty_is_capped() {
    // Un Retry-After absurdo (cuota diaria, cabecera mal formada) no puede
    // dejar el target sin ninguna oportunidad de probe durante horas.
    let limiter = PredictiveRateLimiter::new();
    let (combo, target) = (ComboId(9), ComboTargetId(901));
    let t0 = 2_000_000;

    assert!(limiter.acquire_target(combo, target, t0));
    limiter.report_rate_limited(combo, target, Some(75_770), t0);
    assert!(
        limiter
            .evaluate_target(combo, target, t0 + 1)
            .is_saturated()
    );

    let after_cap = t0 + MAX_OPEN_PENALTY_MS + 1;
    assert_eq!(
        limiter.evaluate_target(combo, target, after_cap),
        TargetReadiness::Probe,
        "tras el techo de penalización el target debe poder probarse"
    );
}

#[test]
fn test_upstream_error_escalating_penalty() {
    let limiter = PredictiveRateLimiter::new();
    let (c, t) = (ComboId(1), ComboTargetId(400));
    let mut now = 100_000;

    assert!(limiter.acquire_target(c, t, now));
    limiter.report_upstream_error(c, t, now);
    let key = PredictiveRateLimiter::compute_key(c, t);
    let reset_1 = limiter
        .shard_for(key)
        .inner
        .read()
        .get(&key)
        .unwrap()
        .reset_at_ms;
    let pen_1 = reset_1 - now;

    now = reset_1 + 1;
    assert!(limiter.acquire_target(c, t, now));
    limiter.report_upstream_error(c, t, now);
    let reset_2 = limiter
        .shard_for(key)
        .inner
        .read()
        .get(&key)
        .unwrap()
        .reset_at_ms;
    let pen_2 = reset_2 - now;

    assert!(pen_2 >= pen_1 && pen_2 <= 120_000);
}

#[test]
fn test_fingerprint_fast_fail_repeating_error() {
    let limiter = PredictiveRateLimiter::new();
    let (c, t) = (ComboId(1), ComboTargetId(500));
    let now = 100_000;
    let err = openproxy_types::CoreError::upstream_error(
        500,
        "opencode-zen",
        "muse-spark",
        "{\"type\":\"error\"}",
        false,
    );
    let fp = compute_error_fingerprint(&err);

    assert!(limiter.acquire_target(c, t, now));
    limiter.report_upstream_error_with_fingerprint(c, t, fp, now);
    assert!(!limiter.should_retry(c, t, fp, 1, now));
}

#[test]
fn test_success_resets_failure_fingerprints() {
    let limiter = PredictiveRateLimiter::new();
    let (c, t) = (ComboId(1), ComboTargetId(600));
    let mut now = 100_000;
    let fp = compute_error_fingerprint(&openproxy_types::CoreError::UpstreamConnection(
        "conn err".into(),
    ));

    assert!(limiter.acquire_target(c, t, now));
    limiter.report_upstream_error_with_fingerprint(c, t, fp, now);
    now += 30_000;
    assert!(limiter.acquire_target(c, t, now));
    limiter.report_success(c, t, None, None, now);
    assert!(limiter.should_retry(c, t, fp, 0, now));
}

#[test]
fn test_account_and_model_key_isolation() {
    let limiter = PredictiveRateLimiter::new();
    let prov = ProviderId("openai".into());
    let (acc1, acc2) = (Some(AccountId(10)), Some(AccountId(20)));
    let (m1, m2) = (Some(ModelRowId(100)), Some(ModelRowId(200)));

    let k_acc1 = PredictiveRateLimiter::compute_key_parts(&prov, acc1, m1, RateLimitScope::Account);
    let k_acc2 = PredictiveRateLimiter::compute_key_parts(&prov, acc2, m1, RateLimitScope::Account);
    assert_ne!(k_acc1, k_acc2);

    let k_m1 = PredictiveRateLimiter::compute_key_parts(&prov, acc1, m1, RateLimitScope::Model);
    let k_m2 = PredictiveRateLimiter::compute_key_parts(&prov, acc1, m2, RateLimitScope::Model);
    assert_ne!(k_m1, k_m2);

    let now = 100_000;
    assert!(limiter.acquire_key(k_acc1, now));
    limiter.report_rate_limited_key(k_acc1, Some(60), now);
    assert_eq!(
        limiter.evaluate_key(k_acc1, now),
        TargetReadiness::Saturated {
            learned_burst: 1,
            window_count: 1,
            reset_in_ms: 60_000
        }
    );
    assert_eq!(limiter.evaluate_key(k_acc2, now), TargetReadiness::Ready);
}

#[test]
fn test_unbound_account_model_isolation() {
    let prov = ProviderId("anthropic".into());
    let (m1, m2) = (Some(ModelRowId(10)), Some(ModelRowId(20)));
    let k1 = PredictiveRateLimiter::compute_key_parts(&prov, None, m1, RateLimitScope::Account);
    let k2 = PredictiveRateLimiter::compute_key_parts(&prov, None, m2, RateLimitScope::Account);
    assert_ne!(k1, k2);

    let k1_m = PredictiveRateLimiter::compute_key_parts(&prov, None, m1, RateLimitScope::Model);
    let k2_m = PredictiveRateLimiter::compute_key_parts(&prov, None, m2, RateLimitScope::Model);
    assert_ne!(k1_m, k2_m);
    assert_eq!(k1, k1_m);
}

#[test]
fn test_window_advance_additive_increase() {
    let mut state = TargetPredictiveState {
        state: TargetRateState::Closed,
        learned_burst: 10,
        window_count: 5,
        window_start_ms: 100_000,
        window_duration_ms: 60_000,
        ..Default::default()
    };
    state.refresh(120_000);
    assert_eq!(state.learned_burst, 10);
    assert_eq!(state.window_count, 5);

    state.refresh(160_000);
    assert_eq!(state.learned_burst, 12);
    assert_eq!(state.window_count, 0);

    state.refresh(230_000);
    assert_eq!(state.learned_burst, 12);
}

#[test]
fn test_recover_from_half_open_min_recovered_burst() {
    let mut state = TargetPredictiveState {
        state: TargetRateState::HalfOpen,
        learned_burst: 1,
        window_count: 1,
        ..Default::default()
    };
    state.apply_success(None, None, 100_000);
    assert_eq!(state.state, TargetRateState::Closed);
    assert_eq!(state.learned_burst, MIN_RECOVERED_BURST);
}
