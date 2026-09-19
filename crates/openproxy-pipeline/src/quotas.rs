use openproxy_db::secrets::MasterKey;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaStatus {
    Available,
    Protected,
    Exhausted,
}

fn find_model_quota_detail(
    account: &openproxy_types::accounts::Account,
    requested_model: &str,
) -> Option<openproxy_types::quota::ModelQuotaDetail> {
    let details = account.quota_model_details.as_deref()?;

    // Pass 1: exact match (case-insensitive) with zero allocations
    if let Some(detail) = details
        .iter()
        .find(|d| requested_model.eq_ignore_ascii_case(&d.model_id))
    {
        return Some(detail.clone());
    }

    // Pass 2: fallback normalized match
    let norm_req = openproxy_types::model_normalize::normalize_model_id(requested_model);
    details
        .iter()
        .find(|d| {
            let norm_detail = openproxy_types::model_normalize::normalize_model_id(&d.model_id);
            norm_req.eq_ignore_ascii_case(&norm_detail)
        })
        .cloned()
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
    if account.provider_id.0.eq_ignore_ascii_case("minimax") {
        return Some((rem / MINIMAX_DEFAULT_CREDIT_BASELINE).clamp(0.0, 1.0));
    }
    if let Some(limit) = account.quota_session_limit
        && limit > 0
    {
        return Some((rem / limit as f64).clamp(0.0, 1.0));
    }
    if rem <= 1.0 {
        return Some(rem.clamp(0.0, 1.0));
    }
    None
}

fn is_credit_balance_exhausted(account: &openproxy_types::accounts::Account) -> bool {
    get_credit_balance_info(account).is_some_and(|(rem, _)| rem <= 0.0)
}

fn check_quota_windows_exhausted(account: &openproxy_types::accounts::Account) -> bool {
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
    requested_model: &str,
) -> QuotaStatus {
    if check_quota_windows_exhausted(account) {
        return QuotaStatus::Exhausted;
    }

    let remaining = get_account_remaining_fraction(account, requested_model);
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
    requested_model: &str,
) -> f64 {
    if is_credit_balance_exhausted(account) {
        return 0.0;
    }

    let mut min_fraction: Option<f64> = None;

    if let Some(detail) = find_model_quota_detail(account, requested_model) {
        min_fraction = Some(detail.remaining_fraction);
    }

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
}

fn enrich_target_with_quota(
    t: crate::context::ResolvedTarget,
    quota_protection_enabled: bool,
    threshold_percentage: u32,
    repo: &dyn crate::repository::PipelineRepository,
    master_key: &MasterKey,
    requested_model: &str,
) -> TargetWithQuota {
    let Some(aid) = t.target.account_id else {
        return TargetWithQuota {
            resolved_target: t,
            status: QuotaStatus::Available,
            remaining_fraction: 1.0,
            priority: 0,
        };
    };

    match repo.get_account(aid, master_key) {
        Ok(Some(account)) => {
            let status = evaluate_account_quota(
                quota_protection_enabled,
                threshold_percentage,
                &account,
                requested_model,
            );
            let remaining_fraction = get_account_remaining_fraction(&account, requested_model);
            TargetWithQuota {
                resolved_target: t,
                status,
                remaining_fraction,
                priority: account.priority,
            }
        }
        _ => TargetWithQuota {
            resolved_target: t,
            status: QuotaStatus::Available,
            remaining_fraction: 1.0,
            priority: 0,
        },
    }
}

fn compare_targets_with_quota(a: &TargetWithQuota, b: &TargetWithQuota) -> std::cmp::Ordering {
    a.resolved_target
        .target
        .priority_order
        .cmp(&b.resolved_target.target.priority_order)
        .then_with(|| {
            a.resolved_target
                .target
                .id
                .0
                .cmp(&b.resolved_target.target.id.0)
        })
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
    let processed_targets: Vec<TargetWithQuota> = targets
        .into_iter()
        .map(|t| {
            enrich_target_with_quota(
                t,
                quota_protection_enabled,
                threshold_percentage,
                repo,
                master_key,
                requested_model,
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
            evaluate_account_quota(true, 10, &acc_positive, "MiniMax-M3"),
            QuotaStatus::Available
        );

        let acc_zero = make_test_account(Some(r#"{"credit_balance":"0"}"#));
        assert!(is_credit_balance_exhausted(&acc_zero));
        assert_eq!(
            evaluate_account_quota(true, 10, &acc_zero, "MiniMax-M3"),
            QuotaStatus::Exhausted
        );
        assert_eq!(get_account_remaining_fraction(&acc_zero, "MiniMax-M3"), 0.0);

        let acc_zero_float = make_test_account(Some(r#"{"credit_balance":0.0}"#));
        assert!(is_credit_balance_exhausted(&acc_zero_float));
        assert_eq!(
            evaluate_account_quota(true, 10, &acc_zero_float, "MiniMax-M3"),
            QuotaStatus::Exhausted
        );

        let acc_comma = make_test_account(Some(r#"{"credit_balance":"773,467"}"#));
        assert!(!is_credit_balance_exhausted(&acc_comma));

        let mut acc_extra_config = make_test_account(None);
        acc_extra_config.extra_config_json = Some(r#"{"credits":"0"}"#.into());
        assert!(is_credit_balance_exhausted(&acc_extra_config));
        assert_eq!(
            evaluate_account_quota(true, 10, &acc_extra_config, "MiniMax-M3"),
            QuotaStatus::Exhausted
        );

        let acc_no_specific = make_test_account(None);
        assert!(!is_credit_balance_exhausted(&acc_no_specific));
        assert_eq!(
            evaluate_account_quota(true, 10, &acc_no_specific, "MiniMax-M3"),
            QuotaStatus::Available
        );
    }

    #[test]
    fn test_credit_balance_quota_protection() {
        // MiniMax default baseline is 1,000,000. 50,000 / 1,000,000 = 0.05 (5%)
        let acc_low = make_test_account(Some(r#"{"credit_balance":"50000"}"#));
        assert!(!is_credit_balance_exhausted(&acc_low));
        let fraction = get_account_remaining_fraction(&acc_low, "MiniMax-M3");
        assert!((fraction - 0.05).abs() < 1e-6);

        // Protected when threshold (10%) >= 5%
        assert_eq!(
            evaluate_account_quota(true, 10, &acc_low, "MiniMax-M3"),
            QuotaStatus::Protected
        );

        // Available when quota protection is disabled
        assert_eq!(
            evaluate_account_quota(false, 10, &acc_low, "MiniMax-M3"),
            QuotaStatus::Available
        );

        // Available when threshold (4%) < 5%
        assert_eq!(
            evaluate_account_quota(true, 4, &acc_low, "MiniMax-M3"),
            QuotaStatus::Available
        );

        // Explicit total_credits: 80 / 1000 = 8%
        let acc_explicit = make_test_account(Some(r#"{"credit_balance":80,"total_credits":1000}"#));
        let fraction_explicit = get_account_remaining_fraction(&acc_explicit, "MiniMax-M3");
        assert!((fraction_explicit - 0.08).abs() < 1e-6);
        assert_eq!(
            evaluate_account_quota(true, 10, &acc_explicit, "MiniMax-M3"),
            QuotaStatus::Protected
        );
        assert_eq!(
            evaluate_account_quota(true, 5, &acc_explicit, "MiniMax-M3"),
            QuotaStatus::Available
        );
    }

    #[test]
    fn test_session_window_quota_protection() {
        let mut acc = make_test_account(None);
        acc.quota_session_used = Some(95);
        acc.quota_session_limit = Some(100);

        let fraction = get_account_remaining_fraction(&acc, "any-model");
        assert!((fraction - 0.05).abs() < 1e-6);

        assert_eq!(
            evaluate_account_quota(true, 10, &acc, "any-model"),
            QuotaStatus::Protected
        );
        assert_eq!(
            evaluate_account_quota(false, 10, &acc, "any-model"),
            QuotaStatus::Available
        );
        assert_eq!(
            evaluate_account_quota(true, 3, &acc, "any-model"),
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
}
