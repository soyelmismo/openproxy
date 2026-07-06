

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaStatus {
    Available,
    Protected,
    Exhausted,
}

pub(crate) fn evaluate_account_quota(
    quota_protection_enabled: bool,
    threshold_percentage: u32,
    account: &crate::accounts::Account,
    requested_model: &str,
) -> QuotaStatus {
    if let (Some(used), Some(limit)) = (account.quota_session_used, account.quota_session_limit)
        && used >= limit {
            return QuotaStatus::Exhausted;
        }
    if let (Some(used), Some(limit)) = (account.quota_weekly_used, account.quota_weekly_limit)
        && used >= limit {
            return QuotaStatus::Exhausted;
        }

    if let Some(ref details_val) = account.quota_model_details
        && let Ok(details) = serde_json::from_value::<Vec<crate::quota::ModelQuotaDetail>>(details_val.clone()) {
            let norm_req = crate::model_normalize::normalize_model_id(requested_model);

            for detail in details {
                let norm_detail = crate::model_normalize::normalize_model_id(&detail.model_id);

                let is_match = norm_req.to_lowercase() == norm_detail.to_lowercase()
                    || requested_model.to_lowercase() == detail.model_id.to_lowercase();

                if is_match {
                    if detail.remaining_fraction <= 0.0 {
                        return QuotaStatus::Exhausted;
                    }
                    if quota_protection_enabled {
                        let threshold_fraction = (threshold_percentage as f64) / 100.0;
                        if detail.remaining_fraction <= threshold_fraction {
                            return QuotaStatus::Protected;
                        }
                    }
                    break;
                }
            }
        }

    QuotaStatus::Available
}


pub(crate) fn get_account_remaining_fraction(
    account: &crate::accounts::Account,
    requested_model: &str,
) -> f64 {
#[allow(clippy::ptr_arg)]
    if let Some(ref details_val) = account.quota_model_details
        && let Ok(details) = serde_json::from_value::<Vec<crate::quota::ModelQuotaDetail>>(details_val.clone()) {
            let norm_req = crate::model_normalize::normalize_model_id(requested_model);

            for detail in details {
                let norm_detail = crate::model_normalize::normalize_model_id(&detail.model_id);

                let is_match = norm_req.to_lowercase() == norm_detail.to_lowercase()
                    || requested_model.to_lowercase() == detail.model_id.to_lowercase();

                if is_match {
                    return detail.remaining_fraction;
                }
            }
        }

    if let (Some(used), Some(limit)) = (account.quota_session_used, account.quota_session_limit)
        && limit > 0 {
            return (limit.saturating_sub(used) as f64) / (limit as f64);
        }

    if let (Some(used), Some(limit)) = (account.quota_weekly_used, account.quota_weekly_limit)
        && limit > 0 {
            return (limit.saturating_sub(used) as f64) / (limit as f64);
        }

    1.0
}


pub(crate) fn apply_quota_routing(
    quota_protection_enabled: bool,
    threshold_percentage: u32,
    conn: &rusqlite::Connection,
    targets: Vec<crate::pipeline::context::ResolvedTarget>,
    requested_model: &str,
) -> Vec<crate::pipeline::context::ResolvedTarget> {
    struct TargetWithQuota {
        resolved_target: crate::pipeline::context::ResolvedTarget,
        status: QuotaStatus,
        remaining_fraction: f64,
        priority: i32,
    }

    let mut processed_targets = Vec::with_capacity(targets.len());

    for t in targets {
        let Some(aid) = t.target.account_id else {
            processed_targets.push(TargetWithQuota {
                resolved_target: t,
                status: QuotaStatus::Available,
                remaining_fraction: 1.0,
                priority: 0,
            });
            continue;
        };

        match crate::accounts::get(&conn, aid) {
            Ok(Some(account)) => {
                let status = evaluate_account_quota(quota_protection_enabled, threshold_percentage, &account, requested_model);
                let remaining_fraction = get_account_remaining_fraction(&account, requested_model);
                processed_targets.push(TargetWithQuota {
                    resolved_target: t,
                    status,
                    remaining_fraction,
                    priority: account.priority,
                });
            }
            _ => {
                processed_targets.push(TargetWithQuota {
                    resolved_target: t,
                    status: QuotaStatus::Available,
                    remaining_fraction: 1.0,
                    priority: 0,
                });
            }
        }
    }

    let non_exhausted: Vec<TargetWithQuota> = processed_targets
        .into_iter()
        .filter(|t| t.status != QuotaStatus::Exhausted)
        .collect();

    let has_available = non_exhausted.iter().any(|t| t.status == QuotaStatus::Available);

    let mut final_targets: Vec<TargetWithQuota> = if has_available {
        non_exhausted
            .into_iter()
            .filter(|t| t.status == QuotaStatus::Available)
            .collect()
    } else {
        non_exhausted
    };

    final_targets.sort_by(|a, b| {
        let pri_cmp = a.priority.cmp(&b.priority);
        if pri_cmp != std::cmp::Ordering::Equal {
            return pri_cmp;
        }

        let quota_cmp = b.remaining_fraction.partial_cmp(&a.remaining_fraction).unwrap_or(std::cmp::Ordering::Equal);
        if quota_cmp != std::cmp::Ordering::Equal {
            return quota_cmp;
        }

        a.resolved_target.target.priority_order.cmp(&b.resolved_target.target.priority_order)
    });

    final_targets.into_iter().map(|t| t.resolved_target).collect()
}


