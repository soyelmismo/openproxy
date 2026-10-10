mod common {
    use crate::quota::*;

    pub(super) const NOW: u64 = 1_700_000_000;

    pub(super) fn pool(
        source: QuotaSource,
        status: QuotaPoolStatus,
        model_ids: &[&str],
        used: Option<i64>,
        limit: Option<i64>,
        remaining: Option<i64>,
    ) -> QuotaPool {
        QuotaPool {
            id: format!("{source:?}-1"),
            source,
            plan_name: Some("plan".into()),
            status,
            unit: "units".into(),
            used,
            limit,
            remaining,
            reset_at: None,
            expires_at: None,
            starts_at: None,
            model_ids: model_ids.iter().map(|s| s.to_string()).collect(),
            model_details: None,
            fetch_error: None,
            last_fetched_at: "0".into(),
        }
    }
}

mod pool_eligibility {
    use super::common::{NOW, pool};
    use crate::quota::*;

    #[test]
    fn serde_uses_snake_case_variants() {
        assert_eq!(
            serde_json::to_string(&QuotaSource::ZcodeStarter).unwrap(),
            r#""zcode_starter""#
        );
        assert_eq!(
            serde_json::to_string(&QuotaSource::CodingPlan).unwrap(),
            r#""coding_plan""#
        );
        for (s, v) in [
            ("active", QuotaPoolStatus::Active),
            ("absent", QuotaPoolStatus::Absent),
            ("exhausted", QuotaPoolStatus::Exhausted),
            ("expired", QuotaPoolStatus::Expired),
            ("unavailable", QuotaPoolStatus::Unavailable),
        ] {
            assert_eq!(
                serde_json::to_string(&v).unwrap(),
                format!("\"{s}\""),
                "status must serialize snake_case"
            );
            let back: QuotaPoolStatus = serde_json::from_str(&format!("\"{s}\"")).unwrap();
            assert_eq!(back, v, "roundtrip {s}");
            assert_eq!(v.as_str(), s);
            assert_eq!(QuotaPoolStatus::parse(s), Ok(v));
        }
        assert!(QuotaPoolStatus::parse("nope").is_err());
        assert!(QuotaSource::parse("nope").is_err());
    }

    #[test]
    fn matches_model_is_exact_case_insensitive_only() {
        let starter = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &["GLM-4.6"],
            None,
            None,
            None,
        );
        assert!(starter.matches_model("glm-4.6"));
        assert!(starter.matches_model("GLM-4.6"));
        // No substring/alias matching: longer ids sharing the prefix must
        // not be covered by the shorter bucket.
        assert!(!starter.matches_model("glm-4.6-thinking-plus"));
        assert!(!starter.matches_model("glm-4"));
        assert!(!starter.matches_model(""));
        assert!(!starter.matches_model("   "));
    }

    #[test]
    fn empty_model_ids_only_means_all_for_coding_plan() {
        let coding = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Active,
            &[],
            None,
            None,
            None,
        );
        assert!(coding.matches_model("anything-at-all"));

        let starter_empty = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &[],
            None,
            None,
            None,
        );
        // Starter buckets are capability-scoped: an empty list authorizes
        // nothing, never the whole catalog.
        assert!(!starter_empty.matches_model("glm-4.6"));
    }

    #[test]
    fn is_usable_checks_status_window_and_budget() {
        let mut p = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &["glm-4.6"],
            Some(10),
            Some(100),
            Some(90),
        );
        assert!(p.is_usable_for_model(NOW, "glm-4.6"));

        // wrong model
        assert!(!p.is_usable_for_model(NOW, "glm-4.5"));

        // non-Active statuses
        for status in [
            QuotaPoolStatus::Absent,
            QuotaPoolStatus::Exhausted,
            QuotaPoolStatus::Expired,
            QuotaPoolStatus::Unavailable,
        ] {
            p.status = status;
            assert!(!p.is_usable_for_model(NOW, "glm-4.6"), "{status:?}");
        }
        p.status = QuotaPoolStatus::Active;

        // fetch_error poisons eligibility even with Active status
        p.fetch_error = Some("upstream 500".into());
        assert!(!p.is_usable_for_model(NOW, "glm-4.6"));
        p.fetch_error = None;

        // not started yet
        p.starts_at = Some((NOW + 100).to_string());
        assert!(!p.is_usable_for_model(NOW, "glm-4.6"));
        p.starts_at = None;

        // expired (both epoch-seconds and ISO forms)
        p.expires_at = Some((NOW - 1).to_string());
        assert!(!p.is_usable_for_model(NOW, "glm-4.6"));
        p.expires_at = Some("2023-11-14T22:13:19Z".to_string());
        assert!(!p.is_usable_for_model(NOW, "glm-4.6"));
        // still-valid ISO expiry does not disqualify
        p.expires_at = Some("2100-01-01T00:00:00Z".to_string());
        assert!(p.is_usable_for_model(NOW, "glm-4.6"));
        p.expires_at = None;

        // remaining <= 0 when known
        p.remaining = Some(0);
        assert!(!p.is_usable_for_model(NOW, "glm-4.6"));
        p.remaining = Some(-3);
        assert!(!p.is_usable_for_model(NOW, "glm-4.6"));
        p.remaining = Some(90);

        // known limit/used exhaustion even when remaining looks fine
        p.used = Some(100);
        assert!(!p.is_usable_for_model(NOW, "glm-4.6"));
        p.used = Some(10);

        // A starter bucket reporting no numbers at all makes no budget claim
        // and fails closed (it is capability-scoped and always metered).
        let starter_unknown = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &["glm-4.6"],
            None,
            None,
            None,
        );
        assert!(!starter_unknown.is_usable_for_model(NOW, "glm-4.6"));

        // A CodingPlan with no numbers is its contract's "entitled, size not
        // reported", so it stays usable.
        let coding_unknown = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Active,
            &[],
            None,
            None,
            None,
        );
        assert!(coding_unknown.is_usable_for_model(NOW, "glm-4.6"));
    }

    #[test]
    fn remaining_fraction_prefers_starter_then_coding_plan() {
        let starter = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &["glm-4.6"],
            Some(20),
            Some(100),
            Some(80),
        );
        let coding = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Active,
            &[],
            Some(50),
            Some(200),
            Some(150),
        );

        // Starter eligible → its fraction (0.8), NOT min(0.8, 0.75).
        let pools = [starter.clone(), coding.clone()];
        let frac = zai_remaining_fraction(&pools, "glm-4.6", NOW);
        assert!((frac.unwrap() - 0.8).abs() < 1e-9, "got {frac:?}");

        // Starter exhausted → falls through to CodingPlan (0.75).
        let mut starter_dead = starter.clone();
        starter_dead.remaining = Some(0);
        starter_dead.status = QuotaPoolStatus::Exhausted;
        let pools = [starter_dead, coding.clone()];
        let frac = zai_remaining_fraction(&pools, "glm-4.6", NOW);
        assert!((frac.unwrap() - 0.75).abs() < 1e-9, "got {frac:?}");

        // Both usable → starter still wins even though coding has MORE
        // remaining in absolute terms (100 vs 80): alternatives, not max.
        let mut coding_rich = coding;
        coding_rich.remaining = Some(199);
        coding_rich.used = Some(1);
        let pools = [starter, coding_rich];
        let frac = zai_remaining_fraction(&pools, "glm-4.6", NOW);
        assert!((frac.unwrap() - 0.8).abs() < 1e-9, "got {frac:?}");
    }

    #[test]
    fn remaining_fraction_unknown_vs_zero() {
        // Only Unavailable pools → None (unknown, caller must not treat as 0).
        let unavailable = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Unavailable,
            &["glm-4.6"],
            Some(10),
            Some(100),
            Some(90),
        );
        assert_eq!(zai_remaining_fraction(&[unavailable], "glm-4.6", NOW), None);

        // No pools at all → None.
        assert_eq!(zai_remaining_fraction(&[], "glm-4.6", NOW), None);

        let absent_starter = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Absent,
            &[],
            None,
            None,
            None,
        );
        // Paid status is authoritative only with a complete Starter verdict.
        let complete = |paid| [absent_starter.clone(), paid];
        // Active, authorized, but remaining = 0 → Some(0.0): known dead.
        let mut dead = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Active,
            &[],
            Some(100),
            Some(100),
            Some(0),
        );
        assert_eq!(
            zai_remaining_fraction(&complete(dead.clone()), "glm-4.6", NOW),
            Some(0.0)
        );

        // Active, authorized, but limit already consumed → Some(0.0) even
        // though the remaining counter was not reported.
        dead.remaining = None;
        assert_eq!(
            zai_remaining_fraction(&complete(dead.clone()), "glm-4.6", NOW),
            Some(0.0)
        );

        // Readable-but-exhausted/absent verdicts are known negatives.
        for status in [QuotaPoolStatus::Exhausted, QuotaPoolStatus::Absent] {
            let mut d = dead.clone();
            d.status = status;
            assert_eq!(
                zai_remaining_fraction(&complete(d), "glm-4.6", NOW),
                Some(0.0),
                "{status:?}"
            );
        }

        // A live CodingPlan whose ceiling we could not read is neither zero
        // nor unknown: usable, so it reports full availability.
        let mut unknown_ceiling = dead;
        unknown_ceiling.used = None;
        unknown_ceiling.limit = None;
        unknown_ceiling.remaining = None;
        assert_eq!(
            zai_remaining_fraction(&complete(unknown_ceiling), "glm-4.6", NOW),
            Some(1.0)
        );

        // The same empty reading on a STARTER bucket is a malformed payload,
        // not "entitled": it claims no budget at all and fails closed.
        let starter_no_numbers = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &["glm-4.6"],
            None,
            None,
            None,
        );
        assert!(!starter_no_numbers.is_usable_for_model(NOW, "glm-4.6"));
        assert_eq!(
            zai_remaining_fraction(&[starter_no_numbers], "glm-4.6", NOW),
            None
        );

        // A pool that never mentions `model` is not authoritative about it,
        // even when it is readable and exhausted: a CodingPlan pinned to a
        // different model says nothing about this one → None.
        let coding_other_model = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Exhausted,
            &["other-model"],
            Some(100),
            Some(100),
            Some(0),
        );
        assert_eq!(
            zai_remaining_fraction(&[coding_other_model], "glm-4.6", NOW),
            None
        );

        // An expired entitlement is a readable negative: the pool
        // authorizes `model` but its window has closed → Some(0.0).
        let expired_zero = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Expired,
            &["glm-4.6"],
            Some(100),
            Some(100),
            Some(0),
        );
        assert_eq!(
            zai_remaining_fraction(std::slice::from_ref(&expired_zero), "glm-4.6", NOW),
            Some(0.0)
        );
        // ...and it does not shadow a coding plan that is still usable.
        let coding = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Active,
            &[],
            Some(25),
            Some(100),
            Some(75),
        );
        let pools = [expired_zero, coding];
        let frac = zai_remaining_fraction(&pools, "glm-4.6", NOW);
        assert!((frac.unwrap() - 0.75).abs() < 1e-9, "got {frac:?}");
    }
}

mod snapshot_and_persistence {
    use super::common::{NOW, pool};
    use crate::quota::*;

    #[test]
    fn uncertainty_outranks_exhaustion() {
        // One source exhausted, the other unreadable → None, NOT Some(0.0):
        // a read failure must not retire an account on unknown data.
        let exhausted = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Exhausted,
            &["glm-4.6"],
            Some(100),
            Some(100),
            Some(0),
        );
        let unreadable = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Unavailable,
            &[],
            None,
            None,
            None,
        );
        assert_eq!(
            zai_remaining_fraction(&[exhausted.clone(), unreadable.clone()], "glm-4.6", NOW),
            None
        );

        // Reversed order: same verdict, order must not matter.
        assert_eq!(
            zai_remaining_fraction(&[unreadable, exhausted.clone()], "glm-4.6", NOW),
            None
        );

        // Once the unknown source becomes readable and exhausted, the account
        // is a known negative.
        let coding_dead = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Exhausted,
            &[],
            Some(100),
            Some(100),
            Some(0),
        );
        assert_eq!(
            zai_remaining_fraction(&[exhausted.clone(), coding_dead], "glm-4.6", NOW),
            Some(0.0)
        );

        // An Unavailable pool for an unrelated model leaves the verdict known.
        let other_model_unavailable = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Unavailable,
            &["other-model"],
            None,
            None,
            None,
        );
        assert_eq!(
            zai_remaining_fraction(&[exhausted, other_model_unavailable], "glm-4.6", NOW),
            Some(0.0)
        );
    }

    #[test]
    fn unusable_fraction_falls_back_to_limit_minus_used() {
        // remaining absent, limit/used present → budget is the difference,
        // not the whole ceiling.
        let p = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Active,
            &[],
            Some(25),
            Some(100),
            None,
        );
        assert!(p.is_usable_for_model(NOW, "glm-4.6"));
        let absent_starter = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Absent,
            &[],
            None,
            None,
            None,
        );
        let frac = zai_remaining_fraction(&[absent_starter, p], "glm-4.6", NOW);
        assert!((frac.unwrap() - 0.75).abs() < 1e-9, "got {frac:?}");

        // An explicit remaining counter wins over the limit/used difference.
        let p = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Active,
            &[],
            Some(25),
            Some(100),
            Some(50),
        );
        let absent_starter = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Absent,
            &[],
            None,
            None,
            None,
        );
        let frac = zai_remaining_fraction(&[absent_starter, p], "glm-4.6", NOW);
        assert!((frac.unwrap() - 0.5).abs() < 1e-9, "got {frac:?}");
    }

    #[test]
    fn invalid_validity_timestamps_fail_closed() {
        // A present-but-unparseable expiry is uncertainty about the window and
        // must not authorize inference.
        let mut bad_expires = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &["glm-4.6"],
            Some(10),
            Some(100),
            Some(90),
        );
        bad_expires.expires_at = Some("not-a-date".into());
        assert!(!bad_expires.is_usable_for_model(NOW, "glm-4.6"));
        assert_eq!(zai_remaining_fraction(&[bad_expires], "glm-4.6", NOW), None);

        // Same for a garbage start timestamp.
        let mut bad_starts = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &["glm-4.6"],
            Some(10),
            Some(100),
            Some(90),
        );
        bad_starts.starts_at = Some(String::new());
        assert!(!bad_starts.is_usable_for_model(NOW, "glm-4.6"));
    }

    #[test]
    fn account_quota_pools_backward_serde() {
        // Legacy JSON without `pools` must deserialize with pools=None.
        let legacy = r#"{
            "session_used": 5,
            "session_limit": 10,
            "session_reset_at": null,
            "weekly_used": null,
            "weekly_limit": null,
            "weekly_reset_at": null,
            "plan_name": "legacy",
            "last_fetched_at": "123",
            "fetch_error": null
        }"#;
        let q: AccountQuota = serde_json::from_str(legacy).unwrap();
        assert_eq!(q.session_used, Some(5));
        assert!(q.pools.is_none());
        let back = serde_json::to_string(&q).unwrap();
        assert!(!back.contains("pools"), "skip_serializing_if None: {back}");

        // New JSON with pools roundtrips.
        let starter = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &["glm-4.6"],
            Some(1),
            Some(10),
            Some(9),
        );
        let q = AccountQuota {
            pools: Some(vec![starter].into_boxed_slice()),
            ..AccountQuota::empty()
        };
        let json = serde_json::to_string(&q).unwrap();
        let back: AccountQuota = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pools, q.pools);
    }

    #[test]
    fn merge_preserves_pools_on_total_fetch_error() {
        let good_starter = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &["glm-4.6"],
            Some(1),
            Some(10),
            Some(9),
        );
        let previous = AccountQuota {
            pools: Some(vec![good_starter].into_boxed_slice()),
            ..AccountQuota::empty()
        };

        // Total fetch failure: fresh snapshot has error and no pools.
        let fresh = AccountQuota::with_error("connection reset");
        let merged = fresh.clone().merge_over_previous(Some(&previous));
        assert!(
            merged
                .fetch_error
                .as_deref()
                .is_some_and(|e| e == "connection reset")
        );
        let kept = &merged.pools.as_deref().unwrap()[0];
        assert_eq!(kept.remaining, Some(9));
        assert_eq!(kept.status, QuotaPoolStatus::Unavailable);
        assert_eq!(kept.fetch_error.as_deref(), Some("connection reset"));

        // Fresh healthy snapshot without pools keeps previous pools too
        // (the writer only clears what it re-read).
        let fresh_ok = AccountQuota::empty();
        let merged = fresh_ok.merge_over_previous(Some(&previous));
        assert_eq!(merged.pools, previous.pools);

        // Fresh snapshot with its own pools replaces them wholesale.
        let new_coding = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Active,
            &[],
            Some(0),
            Some(5),
            Some(5),
        );
        let fresh_new = AccountQuota {
            pools: Some(vec![new_coding].into_boxed_slice()),
            ..AccountQuota::empty()
        };
        let merged = fresh_new.merge_over_previous(Some(&previous));
        assert_eq!(merged.pools.as_ref().map(|p| p.len()), Some(1));
        assert_eq!(
            merged.pools.as_deref().unwrap()[0].source,
            QuotaSource::CodingPlan
        );

        // No previous: error-only snapshot stays pool-less.
        let merged = fresh.merge_over_previous(None);
        assert!(merged.pools.is_none());
    }

    #[test]
    fn partial_fetch_failure_preserves_balance_without_authorizing_it() {
        let mut p = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &["glm-4.6"],
            Some(1),
            Some(10),
            Some(9),
        );
        p.id = "bucket".into();
        let previous = AccountQuota {
            pools: Some(vec![p].into_boxed_slice()),
            ..AccountQuota::empty()
        };
        let mut failure = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Unavailable,
            &[],
            None,
            None,
            None,
        );
        failure.id = "zcode-starter".into();
        failure.fetch_error = Some("read unavailable".into());
        let fresh = AccountQuota {
            pools: Some(vec![failure].into_boxed_slice()),
            ..AccountQuota::empty()
        };
        let merged = fresh.merge_over_previous(Some(&previous));
        let kept = &merged.pools.as_deref().unwrap()[0];
        assert_eq!(kept.remaining, Some(9));
        assert_eq!(kept.id, "bucket");
        assert_eq!(kept.status, QuotaPoolStatus::Unavailable);
        assert!(!kept.is_usable_for_model(NOW, "glm-4.6"));
    }

    #[test]
    fn is_empty_accounts_for_pools() {
        // Starter-only account: every legacy aggregate field is None by contract,
        // so the legacy fields alone would call this snapshot empty.
        let starter = pool(
            QuotaSource::ZcodeStarter,
            QuotaPoolStatus::Active,
            &["glm-4.6"],
            Some(10),
            Some(100),
            Some(90),
        );
        let starter_only = AccountQuota {
            pools: Some(vec![starter].into_boxed_slice()),
            ..AccountQuota::empty()
        };
        assert!(!starter_only.is_empty(), "pools are signal on their own");
        // An empty pool list carries no signal either.
        let empty_pools = AccountQuota {
            pools: Some(Vec::new().into_boxed_slice()),
            ..AccountQuota::empty()
        };
        assert!(empty_pools.is_empty());
        // A legacy-only snapshot still behaves as before.
        assert!(AccountQuota::empty().is_empty());
        let legacy = AccountQuota {
            session_used: Some(1),
            ..AccountQuota::empty()
        };
        assert!(!legacy.is_empty());
    }
    #[test]
    fn carried_unavailable_pools_are_never_presented_as_fresh_active() {
        let coding = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Active,
            &[],
            Some(25),
            Some(100),
            Some(75),
        );
        let previous = AccountQuota {
            pools: Some(vec![coding].into_boxed_slice()),
            ..AccountQuota::empty()
        };
        // Total fetch failure: the good pool is conserved, but nothing about it is
        // re-labelled as a fresh read.
        let fresh = AccountQuota::with_error("connection reset");
        let merged = fresh.merge_over_previous(Some(&previous));
        let kept = merged.pools.as_deref().unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].source, QuotaSource::CodingPlan);
        assert_eq!(
            kept[0].status,
            QuotaPoolStatus::Unavailable,
            "a conserved failed pool cannot authorize new inference"
        );
        assert!(
            kept[0].fetch_error.as_deref() == Some("connection reset"),
            "the fresh read failure must be visible per source"
        );
        // A previously unreadable pool stays unreadable after the merge.
        let mut unreadable = pool(
            QuotaSource::CodingPlan,
            QuotaPoolStatus::Unavailable,
            &[],
            None,
            None,
            None,
        );
        unreadable.fetch_error = Some("upstream 502".into());
        unreadable.last_fetched_at = "1".into();
        let previous_bad = AccountQuota {
            pools: Some(vec![unreadable].into_boxed_slice()),
            ..AccountQuota::empty()
        };
        let mut fresh_stamped = AccountQuota::with_error("connection reset");
        fresh_stamped.last_fetched_at = "1700000000".into();
        let merged = fresh_stamped.merge_over_previous(Some(&previous_bad));
        let kept = merged.pools.as_deref().unwrap();
        assert_eq!(kept[0].status, QuotaPoolStatus::Unavailable);
        assert_eq!(kept[0].fetch_error.as_deref(), Some("connection reset"));
        assert_eq!(
            kept[0].last_fetched_at, "1700000000",
            "the unreadable pool is re-stamped as still-unreadable, not as fresh"
        );
        // ...and it still reads as unknown, never as zero.
        assert_eq!(zai_remaining_fraction(kept, "glm-4.6", NOW), None);
    }
}
