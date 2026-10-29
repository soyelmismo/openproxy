use openproxy_db::secrets::MasterKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaStatus {
    Available,
    Protected,
    Exhausted,
}

/// Resolve the per-model quota detail that governs `candidates`.
///
/// Several names can describe the same request: the model the client asked for
/// (possibly a combo alias such as `nerd`) and the upstream model the target
/// actually bills (`claude-opus-4-6-thinking`). Quota is consumed by the model
/// that runs, so the resolved name is tried first and the requested one is kept
/// as a fallback for providers that key their details by the requested id.
fn find_model_quota_detail(
    account: &openproxy_types::accounts::Account,
    candidates: &[&str],
) -> Option<openproxy_types::quota::ModelQuotaDetail> {
    let details = account.quota_model_details.as_deref()?;

    // Pass 1: exact match (case-insensitive) with zero allocations
    for candidate in candidates {
        if let Some(detail) = details
            .iter()
            .find(|d| candidate.eq_ignore_ascii_case(&d.model_id))
        {
            return Some(detail.clone());
        }
    }

    // Pass 2: fallback normalized match
    for candidate in candidates {
        let norm_req = openproxy_types::model_normalize::normalize_model_id(candidate);
        if let Some(detail) = details.iter().find(|d| {
            let norm_detail = openproxy_types::model_normalize::normalize_model_id(&d.model_id);
            norm_req.eq_ignore_ascii_case(&norm_detail)
        }) {
            return Some(detail.clone());
        }
    }
    None
}

fn is_monthly_window_exhausted(account: &openproxy_types::accounts::Account) -> bool {
    let Some(details) = account.quota_model_details.as_deref() else {
        return false;
    };
    details.iter().any(|d| {
        (d.model_id == "Monthly Limit" || d.model_id == "Monthly Window")
            && (d.remaining_fraction <= 0.0
                || (d.session_limit > 0 && d.session_used >= d.session_limit))
    })
}

fn parse_number_value(val: &serde_json::Value) -> Option<f64> {
    if let Some(n) = val.as_f64() {
        Some(n)
    } else if let Some(s) = val.as_str() {
        s.trim().replace(',', "").parse::<f64>().ok()
    } else {
        None
    }
}

fn parse_credits_and_total_from_json(raw: &str) -> Option<(f64, Option<f64>)> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    let rem_val = v
        .get("credit_balance")
        .or_else(|| v.get("credits"))
        .or_else(|| v.get("op_credit_balance"))
        .or_else(|| v.get("remains"))
        .or_else(|| v.get("remaining_amount"))
        .or_else(|| v.get("total_remaining_amount"))?;

    let rem = parse_number_value(rem_val)?;

    let total_val = v
        .get("total_amount")
        .or_else(|| v.get("total_credits"))
        .or_else(|| v.get("initial_credits"))
        .or_else(|| v.get("credit_limit"))
        .or_else(|| v.get("total_limit"))
        .or_else(|| v.get("total"));

    let total = total_val.and_then(parse_number_value);

    Some((rem, total))
}

fn get_credit_balance_info(
    account: &openproxy_types::accounts::Account,
) -> Option<(f64, Option<f64>)> {
    account
        .oauth_provider_specific
        .as_deref()
        .and_then(parse_credits_and_total_from_json)
        .or_else(|| {
            account
                .extra_config_json
                .as_deref()
                .and_then(parse_credits_and_total_from_json)
        })
}

const MINIMAX_DEFAULT_CREDIT_BASELINE: f64 = 1_000_000.0;

fn get_credit_balance_fraction(account: &openproxy_types::accounts::Account) -> Option<f64> {
    let (rem, total_opt) = get_credit_balance_info(account)?;
    if rem <= 0.0 {
        return Some(0.0);
    }
    if let Some(total) = total_opt
        && total > 0.0
    {
        return Some((rem / total).clamp(0.0, 1.0));
    }
    if let Some(limit) = account.quota_session_limit
        && limit > 0
    {
        return Some((rem / limit as f64).clamp(0.0, 1.0));
    }
    if account.provider_id.0.eq_ignore_ascii_case("minimax") {
        return Some((rem / MINIMAX_DEFAULT_CREDIT_BASELINE).clamp(0.0, 1.0));
    }
    if rem <= 1.0 {
        return Some(rem.clamp(0.0, 1.0));
    }
    None
}

fn is_credit_balance_exhausted(account: &openproxy_types::accounts::Account) -> bool {
    get_credit_balance_info(account).is_some_and(|(rem, _)| rem <= 0.0)
}

fn check_quota_windows_exhausted(
    account: &openproxy_types::accounts::Account,
    candidates: &[&str],
) -> bool {
    // A per-model detail that matches the requested model is authoritative for
    // that model. Antigravity exposes several *disjoint* pools under one login
    // (a Gemini pool and a Claude pool that share no budget), while the
    // account-wide session/weekly counters hold only the worst bucket across
    // all of them. Consulting those aggregates here would retire the whole
    // account for a model whose own pool is untouched.
    //
    // Credit balance is not a window: it is real money, so it keeps vetoing
    // regardless of any per-model reading.
    if find_model_quota_detail(account, candidates).is_some() {
        return is_credit_balance_exhausted(account);
    }

    let session_exhausted = matches!(
        (account.quota_session_used, account.quota_session_limit),
        (Some(used), Some(limit)) if used >= limit
    );
    let weekly_exhausted = matches!(
        (account.quota_weekly_used, account.quota_weekly_limit),
        (Some(used), Some(limit)) if used >= limit
    );
    session_exhausted
        || weekly_exhausted
        || is_monthly_window_exhausted(account)
        || is_credit_balance_exhausted(account)
}

pub(crate) fn evaluate_account_quota(
    quota_protection_enabled: bool,
    threshold_percentage: u32,
    account: &openproxy_types::accounts::Account,
    candidates: &[&str],
) -> QuotaStatus {
    if account.provider_id.as_str() == "zai" {
        let Some(pools) = account.quota_pools.as_deref().filter(|p| !p.is_empty()) else {
            return QuotaStatus::Available;
        };
        let now = openproxy_types::now_unix_secs_str()
            .parse::<u64>()
            .unwrap_or(0);
        // Stale counters must reach route preparation, which refreshes both
        // sources. Otherwise a depleted cached pool permanently retires a
        // newly renewed entitlement before its balance can ever be read.
        if !pools
            .iter()
            .any(|p| p.source == openproxy_types::QuotaSource::ZcodeStarter)
            || !pools
                .iter()
                .any(|p| p.source == openproxy_types::QuotaSource::CodingPlan)
            || pools.iter().any(|pool| {
                pool.last_fetched_at.parse::<u64>().map_or(true, |fetched| {
                    fetched > now || now.saturating_sub(fetched) > 60
                })
            })
        {
            return QuotaStatus::Available;
        }
        let remaining = openproxy_types::quota::zai_remaining_fraction(
            pools,
            candidates.first().copied().unwrap_or_default(),
            now,
        );
        return match remaining {
            Some(value) if value <= 0.0 => QuotaStatus::Exhausted,
            Some(value)
                if quota_protection_enabled && value <= f64::from(threshold_percentage) / 100.0 =>
            {
                QuotaStatus::Protected
            }
            _ => QuotaStatus::Available,
        };
    }
    if check_quota_windows_exhausted(account, candidates) {
        return QuotaStatus::Exhausted;
    }

    let remaining = get_account_remaining_fraction(account, candidates);
    if remaining <= 0.0 {
        return QuotaStatus::Exhausted;
    }

    if quota_protection_enabled {
        let threshold_fraction = f64::from(threshold_percentage) / 100.0;
        if remaining <= threshold_fraction {
            return QuotaStatus::Protected;
        }
    }

    QuotaStatus::Available
}

fn calculate_remaining_fraction(used: Option<i64>, limit: Option<i64>) -> Option<f64> {
    let (used, limit) = (used?, limit?);
    (limit > 0).then(|| (limit.saturating_sub(used) as f64) / (limit as f64))
}

pub(crate) fn get_account_remaining_fraction(
    account: &openproxy_types::accounts::Account,
    candidates: &[&str],
) -> f64 {
    if account.provider_id.as_str() == "zai"
        && let Some(pools) = account.quota_pools.as_deref()
    {
        let now = openproxy_types::now_unix_secs_str()
            .parse::<u64>()
            .unwrap_or(0);
        return openproxy_types::quota::zai_remaining_fraction(
            pools,
            candidates.first().copied().unwrap_or_default(),
            now,
        )
        .unwrap_or(1.0);
    }
    if is_credit_balance_exhausted(account) {
        return 0.0;
    }

    // Per-model detail wins outright when it exists. The account-wide
    // session/weekly counters are a *minimum across every pool* (see
    // `parse_antigravity_models_response`), so folding them in with `min()`
    // would let an exhausted Gemini pool report 0% for a Claude model whose
    // own pool is full. Providers that publish no per-model detail keep the
    // old aggregate behaviour untouched.
    if let Some(detail) = find_model_quota_detail(account, candidates) {
        let credits = get_credit_balance_fraction(account).unwrap_or(1.0);
        return detail.remaining_fraction.min(credits);
    }

    let mut min_fraction: Option<f64> = None;

    if let Some(credits) = get_credit_balance_fraction(account) {
        min_fraction = Some(match min_fraction {
            Some(cur) => cur.min(credits),
            None => credits,
        });
    }

    let session_or_weekly =
        calculate_remaining_fraction(account.quota_session_used, account.quota_session_limit)
            .or_else(|| {
                calculate_remaining_fraction(account.quota_weekly_used, account.quota_weekly_limit)
            });

    if let Some(frac) = session_or_weekly {
        min_fraction = Some(match min_fraction {
            Some(cur) => cur.min(frac),
            None => frac,
        });
    }

    let monthly = account.quota_model_details.as_deref().and_then(|details| {
        details
            .iter()
            .find(|d| d.model_id == "Monthly Limit" || d.model_id == "Monthly Window")
            .map(|d| d.remaining_fraction)
    });

    if let Some(frac) = monthly {
        min_fraction = Some(match min_fraction {
            Some(cur) => cur.min(frac),
            None => frac,
        });
    }

    min_fraction.unwrap_or(1.0)
}

struct TargetWithQuota {
    resolved_target: crate::context::ResolvedTarget,
    status: QuotaStatus,
    remaining_fraction: f64,
    priority: i32,
    target_position: usize,
}

fn enrich_target_with_quota(
    t: crate::context::ResolvedTarget,
    quota_protection_enabled: bool,
    threshold_percentage: u32,
    repo: &dyn crate::repository::PipelineRepository,
    master_key: &MasterKey,
    requested_model: &str,
    target_position: usize,
) -> TargetWithQuota {
    let Some(aid) = t.target.account_id else {
        return TargetWithQuota {
            resolved_target: t,
            status: QuotaStatus::Available,
            remaining_fraction: 1.0,
            priority: 0,
            target_position,
        };
    };

    match repo.get_account(aid, master_key) {
        Ok(Some(account)) => {
            // The model this target actually bills comes first: for a combo the
            // requested name is the combo alias, which never appears in the
            // upstream quota catalog. The requested name stays as a fallback so
            // providers keying details by the requested id keep working.
            let resolved_model = t.model.model_id.as_str();
            let candidates: &[&str] = if resolved_model.eq_ignore_ascii_case(requested_model) {
                &[resolved_model]
            } else {
                &[resolved_model, requested_model]
            };
            let status = evaluate_account_quota(
                quota_protection_enabled,
                threshold_percentage,
                &account,
                candidates,
            );
            let remaining_fraction = get_account_remaining_fraction(&account, candidates);
            TargetWithQuota {
                resolved_target: t,
                status,
                remaining_fraction,
                priority: account.priority,
                target_position,
            }
        }
        _ => TargetWithQuota {
            resolved_target: t,
            status: QuotaStatus::Available,
            remaining_fraction: 1.0,
            priority: 0,
            target_position,
        },
    }
}

fn compare_targets_with_quota(a: &TargetWithQuota, b: &TargetWithQuota) -> std::cmp::Ordering {
    a.target_position
        .cmp(&b.target_position)
        .then_with(|| a.priority.cmp(&b.priority))
        .then_with(|| {
            b.remaining_fraction
                .partial_cmp(&a.remaining_fraction)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

pub(crate) fn apply_quota_routing(
    quota_protection_enabled: bool,
    threshold_percentage: u32,
    repo: &dyn crate::repository::PipelineRepository,
    master_key: &MasterKey,
    targets: Vec<crate::context::ResolvedTarget>,
    requested_model: &str,
) -> Vec<crate::context::ResolvedTarget> {
    let mut target_positions = std::collections::HashMap::new();
    let mut next_pos = 0usize;
    for t in &targets {
        target_positions.entry(t.target.id).or_insert_with(|| {
            let pos = next_pos;
            next_pos += 1;
            pos
        });
    }

    let processed_targets: Vec<TargetWithQuota> = targets
        .into_iter()
        .map(|t| {
            let target_position = *target_positions.get(&t.target.id).unwrap_or(&0);
            enrich_target_with_quota(
                t,
                quota_protection_enabled,
                threshold_percentage,
                repo,
                master_key,
                requested_model,
                target_position,
            )
        })
        .collect();

    let non_exhausted: Vec<TargetWithQuota> = processed_targets
        .into_iter()
        .filter(|t| t.status != QuotaStatus::Exhausted)
        .collect();

    let has_available = non_exhausted
        .iter()
        .any(|t| t.status == QuotaStatus::Available);

    let mut final_targets: Vec<TargetWithQuota> = if has_available {
        non_exhausted
            .into_iter()
            .filter(|t| t.status == QuotaStatus::Available)
            .collect()
    } else {
        non_exhausted
    };

    final_targets.sort_by(compare_targets_with_quota);

    final_targets
        .into_iter()
        .map(|t| t.resolved_target)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::accounts::{Account, HealthStatus};
    use openproxy_types::ids::{AccountId, ProviderId};

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
        assert_eq!(get_account_remaining_fraction(&acc_zero, &["MiniMax-M3"]), 0.0);

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

    #[test]
    fn test_compare_targets_with_quota_sorts_by_credits() {
        use crate::context::ResolvedTarget;
        use openproxy_types::combos::ComboTarget;
        use openproxy_types::ids::{ComboId, ComboTargetId, ModelId, ModelRowId};
        use openproxy_types::models::Model;
        use openproxy_types::{RateLimitScope, TargetFormat};

        let combo_target = ComboTarget {
            id: ComboTargetId(1),
            combo_id: ComboId(1),
            provider_id: ProviderId("minimax".into()),
            account_id: None,
            model_row_id: Some(ModelRowId(10)),
            priority_order: 1,
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
        };

        let make_target = |acc_id: i64, fraction: f64| TargetWithQuota {
            resolved_target: ResolvedTarget {
                target: combo_target.clone(),
                model: Model {
                    row_id: ModelRowId(10),
                    provider_id: ProviderId("minimax".into()),
                    model_id: ModelId::new("MiniMax-M3"),
                    target_format: TargetFormat::Openai,
                    ..Default::default()
                },
                api_key: format!("key-{acc_id}"),
                api_key_label: None,
                custom_meta: None,
            },
            status: QuotaStatus::Available,
            remaining_fraction: fraction,
            priority: 10,
            target_position: 0,
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
        use crate::context::ResolvedTarget;
        use openproxy_types::combos::ComboTarget;
        use openproxy_types::ids::{ComboId, ComboTargetId, ModelId, ModelRowId};
        use openproxy_types::models::Model;
        use openproxy_types::{RateLimitScope, TargetFormat};

        let make_target_with_pos = |target_id: i64, prio_order: i32, pos: usize| TargetWithQuota {
            resolved_target: ResolvedTarget {
                target: ComboTarget {
                    id: ComboTargetId(target_id),
                    combo_id: ComboId(1),
                    provider_id: ProviderId("minimax".into()),
                    account_id: None,
                    model_row_id: Some(ModelRowId(10)),
                    priority_order: prio_order,
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
                },
                model: Model {
                    row_id: ModelRowId(10),
                    provider_id: ProviderId("minimax".into()),
                    model_id: ModelId::new("MiniMax-M3"),
                    target_format: TargetFormat::Openai,
                    ..Default::default()
                },
                api_key: "key".into(),
                api_key_label: None,
                custom_meta: None,
            },
            status: QuotaStatus::Available,
            remaining_fraction: 1.0,
            priority: 0,
            target_position: pos,
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
        assert!((get_account_remaining_fraction(&acc, &["claude-opus-4-6-thinking"]) - 1.0).abs() < 1e-9);
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
}
