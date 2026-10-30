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

/// The pseudo-detail Antigravity publishes for the account-wide monthly
/// window, under either of the two names it has used.
fn monthly_window_detail(
    details: &[openproxy_types::quota::ModelQuotaDetail],
) -> Option<&openproxy_types::quota::ModelQuotaDetail> {
    details
        .iter()
        .find(|d| d.model_id == "Monthly Limit" || d.model_id == "Monthly Window")
}

fn is_monthly_window_exhausted(account: &openproxy_types::accounts::Account) -> bool {
    let Some(details) = account.quota_model_details.as_deref() else {
        return false;
    };
    monthly_window_detail(details).is_some_and(|d| {
        d.remaining_fraction <= 0.0 || (d.session_limit > 0 && d.session_used >= d.session_limit)
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

/// A used/limit pair is exhausted once the used count has reached the limit.
fn quota_window_exhausted(used: Option<i64>, limit: Option<i64>) -> bool {
    matches!((used, limit), (Some(used), Some(limit)) if used >= limit)
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

    let session_exhausted =
        quota_window_exhausted(account.quota_session_used, account.quota_session_limit);
    let weekly_exhausted =
        quota_window_exhausted(account.quota_weekly_used, account.quota_weekly_limit);
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

    let session_or_weekly =
        calculate_remaining_fraction(account.quota_session_used, account.quota_session_limit)
            .or_else(|| {
                calculate_remaining_fraction(account.quota_weekly_used, account.quota_weekly_limit)
            });

    let monthly = account
        .quota_model_details
        .as_deref()
        .and_then(monthly_window_detail)
        .map(|d| d.remaining_fraction);

    [
        get_credit_balance_fraction(account),
        session_or_weekly,
        monthly,
    ]
    .into_iter()
    .flatten()
    .reduce(f64::min)
    .unwrap_or(1.0)
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
#[path = "quotas_tests.rs"]
mod tests;
