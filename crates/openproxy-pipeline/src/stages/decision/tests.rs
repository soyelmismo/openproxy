use super::*;
use openproxy_types::ProviderId;
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::ids::{ComboId, ComboTargetId, ModelRowId};
use openproxy_types::message::OpenAIMessage;
use openproxy_types::models::Model;

#[test]
fn test_extract_prompt_state_utf8_safe() {
    let req = openproxy_types::OpenAIRequest {
        model: "test".into(),
        messages: vec![OpenAIMessage {
            role: "user".into(),
            content: Some(serde_json::Value::String("¡Hola, mundo! 🚀".into())),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            extra: Default::default(),
        }],
        stream: false,
        temperature: None,
        max_tokens: None,
        top_p: None,
        top_k: None,
        user: None,
        stop: None,
        tools: None,
        tool_choice: None,
        extra: Default::default(),
    };

    let state = extract_prompt_state(&req, 100);
    assert_eq!(state, "¡Hola, mundo! 🚀");

    // Truncation should not break UTF-8 boundary
    let short = extract_prompt_state(&req, 5);
    assert!(std::str::from_utf8(short.as_bytes()).is_ok());
}

#[test]
fn test_hysteresis_damping_constants() {
    const { assert!(ELASTIC_HYSTERESIS_MARGIN > 0.0) };
    const { assert!(ELASTIC_CONFIDENCE_THRESHOLD > 0.0) };

    // Simulation: ambiguous prompt (margin 3.31% < 5%) -> switch rejected
    let prob_chosen = 0.1369;
    let prob_pinned = 0.1038;
    let margin = prob_chosen - prob_pinned;
    assert!(margin < ELASTIC_HYSTERESIS_MARGIN);

    // Simulation: clear escalation (margin 7.62% >= 5%) -> switch accepted
    let prob_chosen = 0.1460;
    let prob_pinned = 0.0698;
    let margin = prob_chosen - prob_pinned;
    assert!(margin >= ELASTIC_HYSTERESIS_MARGIN);
}

#[test]
fn test_filter_decision_candidates_filtering() {
    let limiter = crate::predictive_rate_limit::PredictiveRateLimiter::new();
    let combo = Combo {
        id: ComboId(10),
        name: "test-combo".into(),
        priority_mode: openproxy_types::combos::PriorityMode::Decision,
        preventive_rate_limit: true,
        ..Default::default()
    };

    let make_rt = |id: i64, active: bool, model_active: bool, desc: Option<&str>| ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(id),
            combo_id: ComboId(10),
            provider_id: ProviderId("openai".into()),
            active,
            description: desc.map(String::from),
            ..Default::default()
        },
        model: Model {
            row_id: ModelRowId(id),
            provider_id: ProviderId("openai".into()),
            model_id: format!("model-{id}").into(),
            active: model_active,
            ..Default::default()
        },
        api_key: "k".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let t1 = make_rt(1, true, true, Some("Fast coding model"));
    let t2 = make_rt(2, false, true, Some("Disabled target"));
    let t3 = make_rt(3, true, false, Some("Disabled model"));
    let t4 = make_rt(4, true, true, Some("In-cooldown model"));
    let t5 = make_rt(5, true, true, Some("Strong reasoning model"));
    let t6 = make_rt(6, true, true, Some("Fast coding model")); // duplicate
    let t7 = make_rt(7, true, true, None); // no desc

    let targets = vec![t1, t2, t3, t4, t5, t6, t7];
    let mut active_cooldowns = std::collections::HashSet::new();
    active_cooldowns.insert(ComboTargetId(4));

    let now_ms = crate::predictive_rate_limit::PredictiveRateLimiter::now_ms();
    let candidates =
        filter_decision_candidates(&targets, &combo, &active_cooldowns, &limiter, now_ms);

    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].0, "1");
    assert_eq!(candidates[0].1, "Fast coding model");
    assert_eq!(candidates[1].0, "5");
    assert_eq!(candidates[1].1, "Strong reasoning model");
}

#[test]
fn test_filter_decision_candidates_predictive_saturation() {
    let limiter = crate::predictive_rate_limit::PredictiveRateLimiter::new();
    let combo = Combo {
        id: ComboId(10),
        name: "test-combo".into(),
        priority_mode: openproxy_types::combos::PriorityMode::Decision,
        preventive_rate_limit: true,
        ..Default::default()
    };

    let make_rt = |id: i64, desc: &str| ResolvedTarget {
        target: ComboTarget {
            id: ComboTargetId(id),
            combo_id: ComboId(10),
            provider_id: ProviderId(format!("prov-{id}")),
            active: true,
            description: Some(desc.into()),
            ..Default::default()
        },
        model: Model {
            row_id: ModelRowId(id),
            provider_id: ProviderId(format!("prov-{id}")),
            model_id: format!("model-{id}").into(),
            active: true,
            ..Default::default()
        },
        api_key: "k".into(),
        api_key_label: None,
        custom_meta: None,
    };

    let t1 = make_rt(1, "Target 1");
    let t2 = make_rt(2, "Target 2");

    let key1 = crate::predictive_rate_limit::PredictiveRateLimiter::compute_target_key(&t1.target);
    let now_ms = 1_000_000;
    limiter.report_rate_limited_key(key1, Some(60), now_ms);

    let targets = vec![t1, t2];
    let active_cooldowns = std::collections::HashSet::new();

    let candidates =
        filter_decision_candidates(&targets, &combo, &active_cooldowns, &limiter, now_ms + 200);
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].0, "2");
}
