//! Account-aware routing for ZCode Starter and the independent Coding Plan.
//! Credentials stay in memory and must never be included in diagnostic output.

use openproxy_types::accounts::Account;
use openproxy_types::quota::{QuotaPool, QuotaPoolStatus, QuotaSource};
use openproxy_types::{CoreError, Result};

use super::{ZAI_DEFAULT_BASE_URL, ZaiTokens};

pub const ZCODE_STARTER_BASE_URL: &str = "https://zcode.z.ai/api/v1/zcode-plan/anthropic";

pub struct ZaiInferenceRoute {
    pub source: QuotaSource,
    pub url: String,
    pub credential: String,
}

fn messages_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.ends_with("/messages") {
        base.to_owned()
    } else if base.ends_with("/v1") {
        format!("{base}/messages")
    } else {
        format!("{base}/v1/messages")
    }
}

/// An unavailable Starter snapshot cannot establish that spending paid quota
/// is necessary. Do not silently turn authentication/transport errors into
/// a billable fallback, even when the independent paid pool is available.
pub fn select_zai_inference_source(
    pools: &[QuotaPool],
    model: &str,
    now_secs: u64,
) -> Result<QuotaSource> {
    if pools
        .iter()
        .any(|p| p.source == QuotaSource::ZcodeStarter && p.is_usable_for_model(now_secs, model))
    {
        return Ok(QuotaSource::ZcodeStarter);
    }
    if !pools.iter().any(|p| p.source == QuotaSource::ZcodeStarter)
        || pools.iter().any(|p| {
            p.source == QuotaSource::ZcodeStarter
                && !p.is_known_ineligible_for_model(now_secs, model)
        })
    {
        return Err(CoreError::ServiceUnavailable(
            "ZCode Starter quota could not be verified; paid fallback was not attempted".into(),
        ));
    }
    if pools
        .iter()
        .any(|p| p.source == QuotaSource::CodingPlan && p.is_usable_for_model(now_secs, model))
    {
        return Ok(QuotaSource::CodingPlan);
    }
    if pools
        .iter()
        .any(|p| p.status == QuotaPoolStatus::Unavailable)
    {
        return Err(CoreError::ServiceUnavailable(
            "Z.ai quota could not be verified for the requested model".into(),
        ));
    }
    Err(CoreError::Validation(format!(
        "no active ZCode Starter or Coding Plan quota authorizes model {model}"
    )))
}

pub fn resolve_zai_inference_route(
    account: &Account,
    fallback_credential: &str,
    model: &str,
    now_secs: u64,
    paid_base_url: &str,
) -> Result<ZaiInferenceRoute> {
    let tokens = ZaiTokens::from_provider_specific_and_args(
        "",
        Some(fallback_credential),
        account.oauth_provider_specific.as_deref(),
    );
    let pools = account.quota_pools.as_deref().ok_or_else(|| {
        CoreError::ServiceUnavailable("Z.ai quota has not been fetched yet".into())
    })?;
    let source = select_zai_inference_source(pools, model, now_secs)?;
    let (base, credential) = match source {
        QuotaSource::ZcodeStarter => (
            ZCODE_STARTER_BASE_URL,
            tokens
                .zcode_jwt_token
                .as_deref()
                .filter(|v| !v.trim().is_empty()),
        ),
        QuotaSource::CodingPlan => (
            paid_base_url,
            tokens.api_key.as_deref().filter(|v| !v.trim().is_empty()),
        ),
    };
    let credential = credential.ok_or_else(|| {
        CoreError::Auth(match source {
            QuotaSource::ZcodeStarter => "ZCode Starter requires a ZCode OAuth token".into(),
            QuotaSource::CodingPlan => "Z.ai Coding Plan requires its API key".into(),
        })
    })?;
    Ok(ZaiInferenceRoute {
        source,
        url: messages_url(base),
        credential: credential.to_owned(),
    })
}

pub fn resolve_default_zai_inference_route(
    account: &Account,
    fallback_credential: &str,
    model: &str,
    now_secs: u64,
) -> Result<ZaiInferenceRoute> {
    resolve_zai_inference_route(
        account,
        fallback_credential,
        model,
        now_secs,
        ZAI_DEFAULT_BASE_URL,
    )
}

/// Only explicit depletion/expiry is eligible for re-selection after a
/// rejected Starter request. WAF, auth, generic rate-limit and connection
/// failures must not consume an unrelated paid plan as a side effect.
pub fn is_zcode_entitlement_exhaustion(error: &CoreError) -> bool {
    let CoreError::UpstreamError { status, body, .. } = error else {
        return false;
    };
    if !matches!(status, 400 | 402 | 403 | 429) {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return false;
    };
    let code = value
        .get("error")
        .and_then(|v| v.get("code"))
        .or_else(|| value.get("code"))
        .and_then(serde_json::Value::as_str);
    matches!(
        code,
        Some(
            "insufficient_quota"
                | "quota_exhausted"
                | "entitlement_exhausted"
                | "entitlement_expired"
        )
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(source: QuotaSource, status: QuotaPoolStatus, models: &[&str]) -> QuotaPool {
        QuotaPool {
            id: "fixture".into(),
            source,
            plan_name: None,
            status,
            unit: "token".into(),
            used: Some(10),
            limit: Some(100),
            remaining: Some(90),
            reset_at: None,
            expires_at: Some("2000".into()),
            starts_at: Some("500".into()),
            model_ids: models.iter().map(|s| s.to_string()).collect(),
            model_details: None,
            fetch_error: None,
            last_fetched_at: "1000".into(),
        }
    }

    #[test]
    fn starter_precedes_paid_and_requires_exact_model() {
        let pools = [
            pool(QuotaSource::CodingPlan, QuotaPoolStatus::Active, &[]),
            pool(
                QuotaSource::ZcodeStarter,
                QuotaPoolStatus::Active,
                &["glm-5.3-flash"],
            ),
        ];
        assert_eq!(
            select_zai_inference_source(&pools, "GLM-5.3-Flash", 1000).unwrap(),
            QuotaSource::ZcodeStarter
        );
        assert_eq!(
            select_zai_inference_source(&pools, "glm-5.3", 1000).unwrap(),
            QuotaSource::CodingPlan
        );
        assert_eq!(
            select_zai_inference_source(&pools, "prefix-glm-5.3-flash", 1000).unwrap(),
            QuotaSource::CodingPlan
        );
    }

    #[test]
    fn exhausted_expired_and_future_starter_allow_known_paid_pool() {
        let paid = pool(QuotaSource::CodingPlan, QuotaPoolStatus::Active, &[]);
        for status in [
            QuotaPoolStatus::Absent,
            QuotaPoolStatus::Expired,
            QuotaPoolStatus::Exhausted,
        ] {
            let pools = [
                pool(QuotaSource::ZcodeStarter, status, &["glm-5.3-flash"]),
                paid.clone(),
            ];
            assert_eq!(
                select_zai_inference_source(&pools, "glm-5.3-flash", 1000).unwrap(),
                QuotaSource::CodingPlan
            );
        }
        let mut starter = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &["glm-5.3-flash"],
        );
        starter.starts_at = Some("1500".into());
        assert_eq!(
            select_zai_inference_source(&[starter.clone(), paid.clone()], "glm-5.3-flash", 1000)
                .unwrap(),
            QuotaSource::CodingPlan
        );
        starter.starts_at = None;
        starter.expires_at = Some("1000".into());
        assert_eq!(
            select_zai_inference_source(&[starter, paid], "glm-5.3-flash", 1000).unwrap(),
            QuotaSource::CodingPlan
        );
    }

    #[test]
    fn unavailable_starter_never_silently_spends_paid_quota() {
        let pools = [
            pool(QuotaSource::ZcodeStarter, QuotaPoolStatus::Unavailable, &[]),
            pool(QuotaSource::CodingPlan, QuotaPoolStatus::Active, &[]),
        ];
        assert!(matches!(
            select_zai_inference_source(&pools, "glm-5.3-flash", 1000),
            Err(CoreError::ServiceUnavailable(_))
        ));
    }

    #[test]
    fn good_starter_survives_paid_failure_or_exhaustion() {
        for status in [QuotaPoolStatus::Unavailable, QuotaPoolStatus::Exhausted] {
            let pools = [
                pool(QuotaSource::CodingPlan, status, &[]),
                pool(
                    QuotaSource::ZcodeStarter,
                    QuotaPoolStatus::Active,
                    &["glm-5.3-flash"],
                ),
            ];
            assert_eq!(
                select_zai_inference_source(&pools, "glm-5.3-flash", 1000).unwrap(),
                QuotaSource::ZcodeStarter
            );
        }
    }

    #[test]
    fn no_quota_never_falls_back_to_general_api_billing() {
        assert!(select_zai_inference_source(&[], "glm-5.3-flash", 1000).is_err());
        let pools = [
            pool(
                QuotaSource::ZcodeStarter,
                QuotaPoolStatus::Active,
                &["glm-5.3-flash"],
            ),
            pool(QuotaSource::CodingPlan, QuotaPoolStatus::Absent, &[]),
        ];
        assert!(select_zai_inference_source(&pools, "glm-5.3", 1000).is_err());
    }

    #[test]
    fn partial_snapshot_and_unknown_capabilities_cannot_spend_paid() {
        let paid = pool(QuotaSource::CodingPlan, QuotaPoolStatus::Active, &[]);
        assert!(
            select_zai_inference_source(std::slice::from_ref(&paid), "glm-5.3-flash", 1000)
                .is_err()
        );
        let starter = pool(QuotaSource::ZcodeStarter, QuotaPoolStatus::Active, &[]);
        assert!(select_zai_inference_source(&[starter, paid], "glm-5.3-flash", 1000).is_err());
    }

    #[test]
    fn ambiguous_active_budget_and_dates_never_enable_paid_fallback() {
        let paid = pool(QuotaSource::CodingPlan, QuotaPoolStatus::Active, &[]);
        let starter = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &["glm-5.3-flash"],
        );
        let mut unknown = starter.clone();
        unknown.used = None;
        unknown.limit = None;
        unknown.remaining = None;
        assert!(
            select_zai_inference_source(&[unknown, paid.clone()], "glm-5.3-flash", 1000).is_err()
        );
        let mut invalid_date = starter;
        invalid_date.expires_at = Some("invalid".into());
        assert!(select_zai_inference_source(&[invalid_date, paid], "glm-5.3-flash", 1000).is_err());
    }

    #[test]
    fn waf_and_generic_rate_limits_are_not_quota_exhaustion() {
        for (status, body) in [
            (
                405,
                r#"{"code":3012,"msg":"request has been blocked due to unusual activity."}"#,
            ),
            (429, r#"{"error":{"code":"rate_limit_exceeded"}}"#),
            (401, r#"{"error":{"code":"insufficient_quota"}}"#),
        ] {
            assert!(!is_zcode_entitlement_exhaustion(
                &CoreError::upstream_error(status, "zai", "glm", body, false)
            ));
        }
        assert!(is_zcode_entitlement_exhaustion(&CoreError::upstream_error(
            429,
            "zai",
            "glm",
            r#"{"error":{"code":"insufficient_quota"}}"#,
            false
        )));
    }
}
