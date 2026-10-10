use super::*;
use openproxy_types::quota::{QuotaPool, QuotaPoolStatus, QuotaSource};

fn pool(source: QuotaSource, status: QuotaPoolStatus) -> QuotaPool {
    QuotaPool {
        id: "fixture".into(),
        source,
        status,
        plan_name: None,
        unit: "token".into(),
        used: Some(1),
        limit: Some(100),
        remaining: Some(99),
        reset_at: None,
        expires_at: None,
        starts_at: None,
        model_ids: vec!["glm-test".into()],
        model_details: None,
        fetch_error: None,
        last_fetched_at: "1000".into(),
    }
}

#[test]
fn independent_pool_failure_does_not_retire_usable_starter() {
    let pools = [
        pool(QuotaSource::ZcodeStarter, QuotaPoolStatus::Active),
        pool(QuotaSource::CodingPlan, QuotaPoolStatus::Unavailable),
    ];
    assert_eq!(quota_pool_health(&pools), accounts::HealthStatus::Healthy);
}

#[test]
fn quota_unknown_is_degraded_not_unhealthy() {
    let pools = [
        pool(QuotaSource::ZcodeStarter, QuotaPoolStatus::Unavailable),
        pool(QuotaSource::CodingPlan, QuotaPoolStatus::Absent),
    ];
    assert_eq!(quota_pool_health(&pools), accounts::HealthStatus::Degraded);
}

#[test]
fn expired_entitlements_are_not_invalid_authentication() {
    let pools = [
        pool(QuotaSource::ZcodeStarter, QuotaPoolStatus::Expired),
        pool(QuotaSource::CodingPlan, QuotaPoolStatus::Absent),
    ];
    assert_eq!(quota_pool_health(&pools), accounts::HealthStatus::Healthy);
}
