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

fn check_quota_windows_exhausted(account: &openproxy_types::accounts::Account) -> bool {
    let session_exhausted = matches!(
        (account.quota_session_used, account.quota_session_limit),
        (Some(used), Some(limit)) if used >= limit
    );
    let weekly_exhausted = matches!(
        (account.quota_weekly_used, account.quota_weekly_limit),
        (Some(used), Some(limit)) if used >= limit
    );
    session_exhausted || weekly_exhausted || is_monthly_window_exhausted(account)
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

    if let Some(detail) = find_model_quota_detail(account, requested_model) {
        if detail.remaining_fraction <= 0.0 {
            return QuotaStatus::Exhausted;
        }
        if quota_protection_enabled {
            let threshold_fraction = f64::from(threshold_percentage) / 100.0;
            if detail.remaining_fraction <= threshold_fraction {
                return QuotaStatus::Protected;
            }
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
    if let Some(detail) = find_model_quota_detail(account, requested_model) {
        return detail.remaining_fraction;
    }

    let session_or_weekly =
        calculate_remaining_fraction(account.quota_session_used, account.quota_session_limit)
            .or_else(|| {
                calculate_remaining_fraction(account.quota_weekly_used, account.quota_weekly_limit)
            });

    let monthly = account
        .quota_model_details
        .as_deref()
        .and_then(|details| {
            details
                .iter()
                .find(|d| d.model_id == "Monthly Limit" || d.model_id == "Monthly Window")
                .map(|d| d.remaining_fraction)
        });

    match (session_or_weekly, monthly) {
        (Some(a), Some(b)) => a.min(b),
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => 1.0,
    }
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
