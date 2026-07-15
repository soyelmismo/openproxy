use crate::combos::{self, AddTargetInput, Strategy, list_targets_with_model};
use crate::error::CoreError;
use crate::ids::{AccountId, ModelRowId, ProviderId, RequestId};
use crate::pipeline::test_utils::*;
use crate::pipeline::*;
use crate::providers::{self, AuthType, ProviderFormat};
use crate::secrets::MasterKey;
use std::sync::Arc;

#[test]
fn test_circuit_breaker_len() {
    let db_path = std::env::temp_dir().join(format!("pipeline-test-{}.db", RequestId::new().0));
    let pool = crate::db::conn::DbPool::open(&db_path).unwrap();
    {
        let mut w = pool.writer();
        crate::db::migrations::run(&mut w).unwrap();
    }

    let config = test_config(Arc::new(MasterKey::generate()));
    let pipeline = Pipeline::new(pool.writer_arc(), config);
    assert_eq!(pipeline.circuit_breaker_len(), 0);
}

#[tokio::test]
async fn resolve_targets_with_empty_combo_returns_empty() {
    let (pool, conn, _path) = fresh_pool();
    let combo_id = {
        let writer = pool.writer();
        combos::create_combo(&writer, "empty", Strategy::Priority, 1).expect("create")
    };

    let cfg = test_config(Arc::new(MasterKey::generate()));
    let p = Pipeline::new(conn, cfg);

    let combo = combos::get_combo(&pool.writer(), combo_id)
        .expect("get")
        .expect("present");
    let targets = p
        .resolve_targets(&combo, None)
        .await
        .expect("resolve_targets");
    assert!(targets.is_empty(), "combo with no targets → empty vec");
}

#[tokio::test]
async fn resolve_targets_with_healthy_account_expands_to_one() {
    let (pool, conn, _path) = fresh_pool();
    let (_model, combo_id, mk) = {
        let writer = pool.writer();
        let model = seed_provider_and_model(&writer, "p", "m", crate::models::TargetFormat::Openai);
        let combo_id = combos::create_combo(&writer, "c", Strategy::Priority, 1).expect("create");
        combos::add_target(
            &writer,
            combos::AddTargetInput {
                combo_id,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(model),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add target");

        let mk = MasterKey::generate();
        crate::accounts::create(
            &writer,
            &ProviderId::new("p"),
            Some("sk-test-1"),
            &mk,
            None,
            1,
            None,
        )
        .expect("seed account");
        (model, combo_id, mk)
    };

    let cfg = test_config(Arc::new(mk));
    let p = Pipeline::new(conn, cfg);

    let combo = combos::get_combo(&pool.writer(), combo_id)
        .expect("get")
        .expect("present");
    let targets = p
        .resolve_targets(&combo, None)
        .await
        .expect("resolve_targets");
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].account_id, Some(AccountId(1)));
}

#[tokio::test]
async fn resolve_targets_with_no_healthy_accounts_drops_target() {
    let (pool, conn, _path) = fresh_pool();
    let combo_id = {
        let writer = pool.writer();
        let model = seed_provider_and_model(&writer, "p", "m", crate::models::TargetFormat::Openai);
        let combo_id = combos::create_combo(&writer, "c", Strategy::Priority, 1).expect("create");
        combos::add_target(
            &writer,
            combos::AddTargetInput {
                combo_id,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(model),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add target");
        combo_id
    };

    let cfg = test_config(Arc::new(MasterKey::generate()));
    let p = Pipeline::new(conn, cfg);

    let combo = combos::get_combo(&pool.writer(), combo_id)
        .expect("get")
        .expect("present");
    let targets = p
        .resolve_targets(&combo, None)
        .await
        .expect("resolve_targets");
    assert_eq!(targets.len(), 1, "target kept with account_id=None");
    assert!(targets[0].account_id.is_none());
}

#[test]
fn resolve_target_api_key_account_id_returns_decrypted_key() {
    let (pool, conn, _path) = fresh_pool();
    let mk = MasterKey::generate();
    let target = {
        let writer = pool.writer();
        seed_provider(&writer, "p", AuthType::Bearer);
        writer
            .execute(
                "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm', 'openai')",
                [],
            )
            .expect("seed model");
        let model_rowid: i64 = writer
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("last_insert_rowid");
        let account_id = crate::accounts::create(
            &writer,
            &ProviderId::new("p"),
            Some("sk-test"),
            &mk,
            None,
            1,
            None,
        )
        .expect("seed account");
        let combo_id = combos::create_combo(&writer, "c", Strategy::Priority, 1).expect("combo");
        let target_id = combos::add_target(
            &writer,
            combos::AddTargetInput {
                combo_id,
                provider_id: ProviderId::new("p"),
                account_id: Some(account_id),
                model_row_id: Some(ModelRowId(model_rowid)),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add target");
        combos::get_target(&writer, target_id)
            .expect("get target")
            .expect("target")
    };

    let cfg = test_config(Arc::new(mk));
    let p = Pipeline::new(conn, cfg);

    assert_eq!(p.resolve_target_api_key(&target).expect("key"), "sk-test");
}

#[test]
fn resolve_target_api_key_none_auth_type_returns_empty() {
    let (pool, conn, _path) = fresh_pool();
    let target = {
        let writer = pool.writer();
        seed_provider(&writer, "p", AuthType::None);
        writer
            .execute(
                "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm', 'openai')",
                [],
            )
            .expect("seed model");
        let model_rowid: i64 = writer
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("last_insert_rowid");
        let combo_id = combos::create_combo(&writer, "c", Strategy::Priority, 1).expect("combo");
        let target_id = combos::add_target(
            &writer,
            combos::AddTargetInput {
                combo_id,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(ModelRowId(model_rowid)),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add target");
        combos::get_target(&writer, target_id)
            .expect("get target")
            .expect("target")
    };

    let cfg = test_config(Arc::new(MasterKey::generate()));
    let p = Pipeline::new(conn, cfg);

    assert_eq!(p.resolve_target_api_key(&target).expect("key"), "");
}

#[test]
fn resolve_target_api_key_none_bearer_returns_auth_error() {
    let (pool, conn, _path) = fresh_pool();
    let target = {
        let writer = pool.writer();
        seed_provider(&writer, "p", AuthType::Bearer);
        writer
            .execute(
                "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm', 'openai')",
                [],
            )
            .expect("seed model");
        let model_rowid: i64 = writer
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("last_insert_rowid");
        let combo_id = combos::create_combo(&writer, "c", Strategy::Priority, 1).expect("combo");
        let target_id = combos::add_target(
            &writer,
            combos::AddTargetInput {
                combo_id,
                provider_id: ProviderId::new("p"),
                account_id: None,
                model_row_id: Some(ModelRowId(model_rowid)),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add target");
        combos::get_target(&writer, target_id)
            .expect("get target")
            .expect("target")
    };

    let cfg = test_config(Arc::new(MasterKey::generate()));
    let p = Pipeline::new(conn, cfg);

    match p.resolve_target_api_key(&target).expect_err("auth error") {
        CoreError::Auth(msg) => assert!(msg.contains("has no account_id after expansion")),
        other => panic!("expected Auth error, got {:?}", other),
    }
}

#[tokio::test]
async fn list_targets_with_model_includes_cooldown_fields() {
    let (pool, _conn, _path) = fresh_pool();
    let mk = MasterKey::generate();
    let (combo_id, target_id, _account_id, _model_id) = {
        let w = pool.writer();
        seed_target_with_account(
            &w,
            combos::create_combo(&w, "c", Strategy::Priority, 1).unwrap(),
            "p",
            "m",
            Some("sk-test"),
            &mk,
            10,
        )
    };
    {
        let w = pool.writer();
        let ts = list_targets_with_model(&w, combo_id).expect("list");
        assert_eq!(ts.len(), 1);
        assert!(!ts[0].in_cooldown);
        assert!(ts[0].cooldown_until.is_none());
        assert!(ts[0].cooldown_reason.is_none());
    }
    {
        let w = pool.writer();
        crate::cooldown::record_failure_with_mode(
            &w,
            target_id,
            "test_err",
            combos::CooldownMode::Flat,
            60,
            3600,
            2,
        )
        .expect("cooldown");
        let ts = list_targets_with_model(&w, combo_id).expect("list");
        assert_eq!(ts.len(), 1);
        assert!(ts[0].in_cooldown);
        assert!(ts[0].cooldown_until.is_some());
        assert_eq!(ts[0].cooldown_reason.as_deref(), Some("test_err"));
    }
}

#[test]
fn circuit_breaker_unhealthy_filter_drops_target_before_cooldown_snapshot() {
    let (pool, conn, _path) = fresh_pool();
    let mk = Arc::new(MasterKey::generate());

    let (combo_id, account_ids) = {
        let w = pool.writer();
        providers::create(
            &w,
            providers::NewProvider {
                id: &ProviderId::new("p"),
                name: "p",
                base_url: "https://example.com",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: crate::providers::RateLimitScope::Account,
            },
        )
        .expect("seed provider");
        w.execute(
            "INSERT INTO models(provider_id, model_id, target_format) \
             VALUES ('p', 'm', 'openai')",
            [],
        )
        .expect("seed model");
        let model_rowid: i64 = w
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("last_insert_rowid");
        let model_id = ModelRowId(model_rowid);
        let combo_id = combos::create_combo(&w, "c", Strategy::Priority, 1).expect("create combo");
        let mut aids = Vec::new();
        for (label, prio) in [("a1", 10_i32), ("a2", 20_i32), ("a3", 30_i32)] {
            let account_id = crate::accounts::create(
                &w,
                &ProviderId::new("p"),
                Some("sk-test"),
                mk.as_ref(),
                Some(label),
                prio,
                None,
            )
            .expect("seed account");
            combos::add_target(
                &w,
                AddTargetInput {
                    combo_id,
                    provider_id: ProviderId::new("p"),
                    account_id: Some(account_id),
                    model_row_id: Some(model_id),
                    sub_combo_id: None,
                    priority_order: prio,
                },
            )
            .expect("add target");
            aids.push(account_id);
        }
        (combo_id, aids)
    };
    assert_eq!(account_ids.len(), 3);

    let cfg = test_config(mk);
    let p = Pipeline::new(conn, cfg);
    for aid in &account_ids {
        p.circuit_breaker
            .force_unhealthy(crate::circuit_breaker::CircuitBreakerKey::Account(*aid));
    }

    let (req, _dis_tx) = make_request(combo_id);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(p.run(req));

    match &result.error {
        Some(CoreError::NoHealthyTargets(id)) => {
            panic!(
                "REGRESSION: pre-CB snapshot fallback did not engage — \
                 got NoHealthyTargets({id}) in 0ms, but the combo had {n} \
                 targets in DB and the eligible filter should have fallen \
                 through to the unfiltered list.",
                id = id,
                n = account_ids.len(),
            );
        }
        other => {
            assert!(
                other.is_some(),
                "dispatch loop should have surfaced an error, not Ok"
            );
        }
    }
}
