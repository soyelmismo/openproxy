use super::*;
use crate::context::ResolvedTarget;
use openproxy_types::accounts::{Account, HealthStatus};
use openproxy_types::combos::ComboTarget;
use openproxy_types::ids::{AccountId, ComboId, ComboTargetId, ModelId, ModelRowId, ProviderId};
use openproxy_types::models::Model;
use openproxy_types::{RateLimitScope, TargetFormat};

fn make_test_account(provider_specific: Option<&str>) -> Account {
    Account {
        id: AccountId(1),
        provider_id: ProviderId("minimax".into()),
        label: Some("Test Account".into()),
        priority: 10,
        health_status: HealthStatus::Healthy,
        extra_config_json: None,
        rate_limited_until: None,
        quota_session_used: None,
        quota_session_limit: None,
        quota_session_reset_at: None,
        quota_weekly_used: None,
        quota_weekly_limit: None,
        quota_weekly_reset_at: None,
        quota_plan_name: Some("Free".into()),
        quota_last_fetched_at: None,
        quota_fetch_error: None,
        quota_pools: None,
        quota_model_details: None,
        auth_type: "oauth".into(),
        email: None,
        oauth_scope: None,
        oauth_provider_specific: provider_specific.map(|s| s.to_string().into_boxed_str()),
        expires_at: None,
        created_at: "2026-09-19".into(),
        current_proxy_id: None,
    }
}

#[test]
fn test_credit_balance_exhaustion() {
    let acc_positive = make_test_account(Some(r#"{"credit_balance":"773467"}"#));
    assert!(!is_credit_balance_exhausted(&acc_positive));
    assert_eq!(
        evaluate_account_quota(true, 10, &acc_positive, &["MiniMax-M3"]),
        QuotaStatus::Available
    );

    let acc_zero = make_test_account(Some(r#"{"credit_balance":"0"}"#));
    assert!(is_credit_balance_exhausted(&acc_zero));
    assert_eq!(
        evaluate_account_quota(true, 10, &acc_zero, &["MiniMax-M3"]),
        QuotaStatus::Exhausted
    );
    assert_eq!(
        get_account_remaining_fraction(&acc_zero, &["MiniMax-M3"]),
        0.0
    );

    let acc_zero_float = make_test_account(Some(r#"{"credit_balance":0.0}"#));
    assert!(is_credit_balance_exhausted(&acc_zero_float));
    assert_eq!(
        evaluate_account_quota(true, 10, &acc_zero_float, &["MiniMax-M3"]),
        QuotaStatus::Exhausted
    );

    let acc_comma = make_test_account(Some(r#"{"credit_balance":"773,467"}"#));
    assert!(!is_credit_balance_exhausted(&acc_comma));

    let mut acc_extra_config = make_test_account(None);
    acc_extra_config.extra_config_json = Some(r#"{"credits":"0"}"#.into());
    assert!(is_credit_balance_exhausted(&acc_extra_config));
    assert_eq!(
        evaluate_account_quota(true, 10, &acc_extra_config, &["MiniMax-M3"]),
        QuotaStatus::Exhausted
    );

    let acc_no_specific = make_test_account(None);
    assert!(!is_credit_balance_exhausted(&acc_no_specific));
    assert_eq!(
        evaluate_account_quota(true, 10, &acc_no_specific, &["MiniMax-M3"]),
        QuotaStatus::Available
    );
}

#[test]
fn test_credit_balance_quota_protection() {
    // MiniMax default baseline is 1,000,000. 50,000 / 1,000,000 = 0.05 (5%)
    let acc_low = make_test_account(Some(r#"{"credit_balance":"50000"}"#));
    assert!(!is_credit_balance_exhausted(&acc_low));
    let fraction = get_account_remaining_fraction(&acc_low, &["MiniMax-M3"]);
    assert!((fraction - 0.05).abs() < 1e-6);

    // Protected when threshold (10%) >= 5%
    assert_eq!(
        evaluate_account_quota(true, 10, &acc_low, &["MiniMax-M3"]),
        QuotaStatus::Protected
    );

    // Available when quota protection is disabled
    assert_eq!(
        evaluate_account_quota(false, 10, &acc_low, &["MiniMax-M3"]),
        QuotaStatus::Available
    );

    // Available when threshold (4%) < 5%
    assert_eq!(
        evaluate_account_quota(true, 4, &acc_low, &["MiniMax-M3"]),
        QuotaStatus::Available
    );

    // Explicit total_credits: 80 / 1000 = 8%
    let acc_explicit = make_test_account(Some(r#"{"credit_balance":80,"total_credits":1000}"#));
    let fraction_explicit = get_account_remaining_fraction(&acc_explicit, &["MiniMax-M3"]);
    assert!((fraction_explicit - 0.08).abs() < 1e-6);
    assert_eq!(
        evaluate_account_quota(true, 10, &acc_explicit, &["MiniMax-M3"]),
        QuotaStatus::Protected
    );
    assert_eq!(
        evaluate_account_quota(true, 5, &acc_explicit, &["MiniMax-M3"]),
        QuotaStatus::Available
    );
}

#[test]
fn test_session_window_quota_protection() {
    let mut acc = make_test_account(None);
    acc.quota_session_used = Some(95);
    acc.quota_session_limit = Some(100);

    let fraction = get_account_remaining_fraction(&acc, &["any-model"]);
    assert!((fraction - 0.05).abs() < 1e-6);

    assert_eq!(
        evaluate_account_quota(true, 10, &acc, &["any-model"]),
        QuotaStatus::Protected
    );
    assert_eq!(
        evaluate_account_quota(false, 10, &acc, &["any-model"]),
        QuotaStatus::Available
    );
    assert_eq!(
        evaluate_account_quota(true, 3, &acc, &["any-model"]),
        QuotaStatus::Available
    );
}

/// Build the `ComboTarget` fixture shared by the quota comparator tests.
fn make_sort_combo_target(target_id: i64, priority_order: i32) -> ComboTarget {
    ComboTarget {
        id: ComboTargetId(target_id),
        combo_id: ComboId(1),
        provider_id: ProviderId("minimax".into()),
        account_id: None,
        model_row_id: Some(ModelRowId(10)),
        priority_order,
        sub_combo_id: None,
        weight: 1,
        active: true,
        rate_limit_scope: RateLimitScope::Account,
        cooldown_mode: None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        thinking_effort: None,
        ..Default::default()
    }
}

/// Build the `Model` fixture shared by the quota comparator tests.
fn make_sort_model() -> Model {
    Model {
        row_id: ModelRowId(10),
        provider_id: ProviderId("minimax".into()),
        model_id: ModelId::new("MiniMax-M3"),
        target_format: TargetFormat::Openai,
        ..Default::default()
    }
}

/// Wrap the shared combo-target/model fixtures into the `TargetWithQuota`
/// record the quota comparator consumes.
fn make_sort_target(
    target_id: i64,
    priority_order: i32,
    target_position: usize,
    remaining_fraction: f64,
    priority: i32,
    api_key: String,
) -> TargetWithQuota {
    TargetWithQuota {
        resolved_target: ResolvedTarget {
            target: make_sort_combo_target(target_id, priority_order),
            model: make_sort_model(),
            api_key,
            api_key_label: None,
            custom_meta: None,
        },
        status: QuotaStatus::Available,
        remaining_fraction,
        priority,
        target_position,
    }
}

#[test]
fn test_compare_targets_with_quota_sorts_by_credits() {
    let make_target = |acc_id: i64, fraction: f64| {
        make_sort_target(1, 1, 0, fraction, 10, format!("key-{acc_id}"))
    };

    let t_high = make_target(1, 0.8);
    let t_low = make_target(2, 0.2);

    // Higher remaining fraction should sort before lower fraction
    assert_eq!(
        compare_targets_with_quota(&t_high, &t_low),
        std::cmp::Ordering::Less
    );
    assert_eq!(
        compare_targets_with_quota(&t_low, &t_high),
        std::cmp::Ordering::Greater
    );
}

#[test]
fn test_compare_targets_with_quota_preserves_target_routing_order() {
    let make_target_with_pos = |target_id: i64, prio_order: i32, pos: usize| {
        make_sort_target(target_id, prio_order, pos, 1.0, 0, "key".into())
    };

    // Target B was placed at position 0 (e.g. by Shuffle or RoundRobin),
    // even though its priority_order is 20 (higher number = lower strict priority).
    let t_first = make_target_with_pos(2, 20, 0);
    let t_second = make_target_with_pos(1, 10, 1);

    assert_eq!(
        compare_targets_with_quota(&t_first, &t_second),
        std::cmp::Ordering::Less
    );
    assert_eq!(
        compare_targets_with_quota(&t_second, &t_first),
        std::cmp::Ordering::Greater
    );
}

/// Antigravity exposes two disjoint pools under one login. The
/// account-wide `quota_session_used` / `quota_weekly_used` columns hold the
/// *worst* bucket of both, so an exhausted Gemini pool used to retire the
/// account for Claude too. Measured on production account 67: Claude 5h and
/// weekly at 1.0, Gemini at 0.0, weekly aggregate 1000/1000.
fn make_split_pool_account() -> Account {
    let mut acc = make_test_account(None);
    acc.provider_id = ProviderId("antigravity".into());
    // Worst-bucket aggregate: Gemini exhausted.
    acc.quota_session_used = Some(0);
    acc.quota_session_limit = Some(1000);
    acc.quota_weekly_used = Some(1000);
    acc.quota_weekly_limit = Some(1000);
    acc.quota_model_details = Some(
        vec![
            openproxy_types::ModelQuotaDetail {
                model_id: "claude-opus-4-6-thinking".into(),
                session_used: 0,
                session_limit: 1000,
                session_reset_at: None,
                remaining_fraction: 1.0,
            },
            openproxy_types::ModelQuotaDetail {
                model_id: "gemini-3.8-flash-tiered".into(),
                session_used: 1000,
                session_limit: 1000,
                session_reset_at: None,
                remaining_fraction: 0.0,
            },
        ]
        .into_boxed_slice(),
    );
    acc
}

#[test]
fn claude_pool_survives_exhausted_gemini_pool() {
    let acc = make_split_pool_account();

    assert_eq!(
        evaluate_account_quota(true, 10, &acc, &["claude-opus-4-6-thinking"]),
        QuotaStatus::Available,
        "a full Claude pool must not be retired by an exhausted Gemini pool"
    );
    assert!(
        (get_account_remaining_fraction(&acc, &["claude-opus-4-6-thinking"]) - 1.0).abs() < 1e-9
    );
}

#[test]
fn gemini_pool_still_reports_exhausted() {
    let acc = make_split_pool_account();

    assert_eq!(
        evaluate_account_quota(true, 10, &acc, &["gemini-3.8-flash-tiered"]),
        QuotaStatus::Exhausted
    );
    assert_eq!(
        get_account_remaining_fraction(&acc, &["gemini-3.8-flash-tiered"]),
        0.0
    );
}

#[test]
fn combo_alias_resolves_to_upstream_model_pool() {
    let acc = make_split_pool_account();

    // A combo request arrives as `nerd`; the target bills
    // `claude-opus-4-6-thinking`. The resolved name must win, otherwise the
    // alias misses the catalog and the aggregate retires the account.
    assert_eq!(
        evaluate_account_quota(true, 10, &acc, &["claude-opus-4-6-thinking", "nerd"]),
        QuotaStatus::Available
    );
    assert_eq!(
        evaluate_account_quota(true, 10, &acc, &["gemini-3.8-flash-tiered", "nerd"]),
        QuotaStatus::Exhausted
    );
}

#[test]
fn model_without_detail_keeps_aggregate_behaviour() {
    let acc = make_split_pool_account();

    // No detail for this model: the old aggregate path applies unchanged.
    // The status still comes from the weekly window being spent.
    assert_eq!(
        evaluate_account_quota(true, 10, &acc, &["some-unlisted-model"]),
        QuotaStatus::Exhausted
    );
    // The fraction keeps the pre-existing precedence rule: `session` wins
    // over `weekly` when both are present, so it reports 1.0 here. That
    // number only drives sort order; `evaluate_account_quota` already
    // returned Exhausted above, which is what removes the target.
    assert_eq!(
        get_account_remaining_fraction(&acc, &["some-unlisted-model"]),
        1.0
    );
}

#[test]
fn credit_balance_still_vetoes_a_full_model_pool() {
    let mut acc = make_split_pool_account();
    acc.oauth_provider_specific = Some(r#"{"credit_balance":"0"}"#.into());

    assert_eq!(
        evaluate_account_quota(true, 10, &acc, &["claude-opus-4-6-thinking"]),
        QuotaStatus::Exhausted,
        "real money is not a per-model window"
    );
}

#[test]
fn exhausted_model_detail_reports_exhausted_without_aggregates() {
    let mut acc = make_test_account(None);
    acc.provider_id = ProviderId("antigravity".into());
    acc.quota_session_used = Some(0);
    acc.quota_session_limit = Some(1000);
    acc.quota_model_details = Some(
        vec![openproxy_types::ModelQuotaDetail {
            model_id: "claude-sonnet-4-6".into(),
            session_used: 1000,
            session_limit: 1000,
            session_reset_at: None,
            remaining_fraction: 0.0,
        }]
        .into_boxed_slice(),
    );

    assert_eq!(
        evaluate_account_quota(true, 10, &acc, &["claude-sonnet-4-6"]),
        QuotaStatus::Exhausted
    );
}
