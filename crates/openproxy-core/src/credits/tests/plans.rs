use super::*;

// ── tiers + leveraged packages ──────────────────────────────────────

fn tier_config(models: &[&str], weekly: f64) -> crate::credits::TierConfig {
    crate::credits::TierConfig {
        weekly_usd_cap: Some(weekly),
        five_hour_usd_cap: None,
        allowed_models: Some(models.iter().map(|m| m.to_string()).collect()),
        allowed_providers: None,
        allowed_combos: None,
        only_whitelisted: None,
    }
}

/// Sellable plan commerce: what the buyer pays, what they get, how long it lives.
fn plan(price: f64, credit: f64, ttl: i64) -> crate::credits::PlanCommerce {
    crate::credits::PlanCommerce {
        stock: None,
        price_usd: Some(price),
        credit_usd: Some(credit),
        ttl_days: ttl,
    }
}

/// A draft plan (no price): not listed for sale.
fn draft_plan() -> crate::credits::PlanCommerce {
    crate::credits::PlanCommerce {
        stock: None,
        price_usd: None,
        credit_usd: None,
        ttl_days: 30,
    }
}

#[test]
fn tier_crud_and_apply_forces_the_whitelist_gate() {
    let conn = test_conn();
    let key = seed_key(&conn, "tiered");

    let t = crate::credits::tiers::create(
        &conn,
        "flash",
        &tier_config(&["m-a", "m-b"], 30.0),
        &draft_plan(),
    )
    .expect("create tier");
    assert_eq!(t.key_count, 0);
    assert!(t.active);

    // Validation: blank name rejected, duplicate rejected by UNIQUE.
    assert!(
        crate::credits::tiers::create(&conn, "  ", &tier_config(&["m"], 1.0), &draft_plan())
            .is_err()
    );
    assert!(
        crate::credits::tiers::create(&conn, "flash", &tier_config(&["m"], 1.0), &draft_plan())
            .is_err()
    );

    crate::credits::tiers::apply_to_key(&conn, key, &t.config).expect("apply");
    let models: String = conn
        .query_row(
            "SELECT allowed_models_json FROM api_keys WHERE id = ?1",
            [key.0],
            |r| r.get(0),
        )
        .expect("models");
    assert_eq!(models, r#"["m-a","m-b"]"#);
    let gate: i64 = conn
        .query_row(
            "SELECT only_whitelisted FROM api_keys WHERE id = ?1",
            [key.0],
            |r| r.get(0),
        )
        .expect("gate");
    // Money invariant: an allowlist is a hard gate, not advisory.
    assert_eq!(gate, 1, "allowlist must force only_whitelisted");
    let caps = crate::api_key_quotas::get_caps(&conn, key)
        .expect("caps")
        .expect("caps row");
    assert_eq!(caps.weekly_usd_cap, Some(30.0));

    // Update replaces the payload wholesale.
    crate::credits::tiers::update(
        &conn,
        t.id,
        Some("flash2"),
        Some(&tier_config(&["m-c"], 40.0)),
        Some(false),
        None,
    )
    .expect("update");
    let t2 = crate::credits::tiers::get(&conn, t.id)
        .expect("get")
        .expect("tier");
    assert_eq!(t2.name, "flash2");
    assert!(!t2.active);
    assert_eq!(
        t2.config.allowed_models.as_deref(),
        Some(&["m-c".to_string()][..])
    );

    // Unused tier deletes cleanly.
    crate::credits::tiers::delete(&conn, t.id).expect("delete");
    assert!(
        crate::credits::tiers::get(&conn, t.id)
            .expect("get")
            .is_none()
    );
}

#[test]
fn tier_allowlist_partitions_combos_from_models() {
    let conn = test_conn();
    let key = seed_key(&conn, "part");

    // A live combo row — the bare-name detection target.
    conn.execute(
        "INSERT INTO combos (name, strategy) VALUES ('autos', 'priority')",
        [],
    )
    .expect("combo row");

    // The admin UI sends ONE merged selection: `combo:autos` rides inside
    // allowed_models. The stored tier must end up canonical: models in
    // models_json, combo NAMES in combos_json, never a combo in models.
    let t = crate::credits::tiers::create(
        &conn,
        "mixed",
        &crate::credits::TierConfig {
            allowed_models: Some(vec!["combo:autos".into(), "deepseek-v4.1-flash".into()]),
            allowed_combos: None,
            ..tier_config(&["deepseek-v4.1-flash"], 10.0)
        },
        &draft_plan(),
    )
    .expect("create mixed tier");
    let row_models: String = conn
        .query_row(
            "SELECT models_json FROM credit_tiers WHERE id = ?1",
            [t.id],
            |r| r.get(0),
        )
        .expect("row models");
    let row_combos: String = conn
        .query_row(
            "SELECT combos_json FROM credit_tiers WHERE id = ?1",
            [t.id],
            |r| r.get(0),
        )
        .expect("row combos");
    assert_eq!(
        row_models, r#"["deepseek-v4.1-flash"]"#,
        "combo must leave models_json"
    );
    assert_eq!(
        row_combos, r#"["autos"]"#,
        "combo name must land in combos_json"
    );

    // Applying to a key resolves the combo NAME to the numeric id the
    // routing gate consumes, and the allowlist still forces the hard gate.
    crate::credits::tiers::apply_to_key(&conn, key, &t.config).expect("apply");
    let key_combos: String = conn
        .query_row(
            "SELECT allowed_combos_json FROM api_keys WHERE id = ?1",
            [key.0],
            |r| r.get(0),
        )
        .expect("key combos");
    assert_eq!(
        key_combos, "[1]",
        "combo name must resolve to its numeric id"
    );
    let gate: i64 = conn
        .query_row(
            "SELECT only_whitelisted FROM api_keys WHERE id = ?1",
            [key.0],
            |r| r.get(0),
        )
        .expect("gate");
    assert_eq!(gate, 1, "allowlist must force only_whitelisted");
}

#[test]
fn leveraged_plan_charges_less_than_it_credits() {
    let conn = test_conn();
    let key = seed_key(&conn, "lev");
    let mk = mk();

    // The tier IS the product: price 1, credit 6 (6x), lives 7 days.
    let t = crate::credits::tiers::create(
        &conn,
        "flash",
        &tier_config(&["m-1"], 25.0),
        &plan(1.0, 6.0, 7),
    )
    .expect("tier");

    // The buyer is charged the PRICE (whole dollar + jitter); the order
    // records the CREDIT (6x) for the matcher to pay out.
    let order =
        orders::create_order(&conn, Some(key), None, USD, t.ttl_days, Some(t.id)).expect("order");
    assert_eq!(
        order.credit_units,
        6 * USD,
        "credit_units must carry the leverage"
    );
    assert!(order.amount_units >= USD && order.amount_units < 2 * USD);

    let outcome =
        matcher::apply_payment(&conn, &mk, "0xlev", 0, 1, order.amount_units, None, 250_000)
            .expect("apply");
    let credited = match outcome {
        PaymentOutcome::Credited { credited_units, .. } => credited_units,
        other => panic!("expected credited, got {other:?}"),
    };
    assert_eq!(
        credited,
        6 * USD,
        "the balance must be the leveraged credit"
    );

    // The tier shaping landed on the key together with the credit.
    let models: String = conn
        .query_row(
            "SELECT allowed_models_json FROM api_keys WHERE id = ?1",
            [key.0],
            |r| r.get(0),
        )
        .expect("models");
    assert_eq!(models, r#"["m-1"]"#);
    let gate: i64 = conn
        .query_row(
            "SELECT only_whitelisted FROM api_keys WHERE id = ?1",
            [key.0],
            |r| r.get(0),
        )
        .expect("gate");
    assert_eq!(gate, 1);
    let caps = crate::api_key_quotas::get_caps(&conn, key)
        .expect("caps")
        .expect("caps row");
    assert_eq!(caps.weekly_usd_cap, Some(25.0));
}
