use super::{active_ids_for, fresh_pool, seed_provider_with_account, seed_three_models};
use crate::discovery_scheduler::*;
use crate::ids::{ModelId, ProviderId as CoreProviderId};
use crate::models::{self, DiscoveredModel, TargetFormat};
use openproxy_adapters::upstream::UpstreamClient;
use openproxy_db::secrets::MasterKey;
use std::sync::Arc;
use std::time::Duration;

#[test]
fn gate_f2_apply_auto_activation_include_keyword() {
    let (pool, _path) = fresh_pool();
    let conn = pool.open_connection().expect("open conn");
    let provider = CoreProviderId::new("acme");

    seed_three_models(&conn, &provider, &["gpt-4", "claude-3", "llama-3"]);

    let updated = models::apply_auto_activation(&conn, &provider, Some("gpt")).expect("apply");
    assert!(updated >= 1, "gpt-4 row should have been updated");

    let active = active_ids_for(&conn, &provider);
    assert!(active.contains(&"gpt-4".to_string()), "gpt-4 stays active");
    assert!(
        !active.contains(&"claude-3".to_string()),
        "claude-3 disabled"
    );
    assert!(!active.contains(&"llama-3".to_string()), "llama-3 disabled");
}

#[test]
fn gate_f2_apply_auto_activation_exclude_keyword() {
    let (pool, _path) = fresh_pool();
    let conn = pool.open_connection().expect("open conn");
    let provider = CoreProviderId::new("acme");

    seed_three_models(&conn, &provider, &["gpt-4", "gpt-legacy", "claude-3"]);

    let updated = models::apply_auto_activation(&conn, &provider, Some("legacy")).expect("apply");
    assert!(updated >= 1);

    let active = active_ids_for(&conn, &provider);
    assert!(
        active.contains(&"gpt-legacy".to_string()),
        "gpt-legacy stays active"
    );
    assert!(!active.contains(&"gpt-4".to_string()), "gpt-4 deactivated");
    assert!(
        !active.contains(&"claude-3".to_string()),
        "claude-3 deactivated"
    );
}

#[test]
fn gate_f2_apply_auto_activation_no_config_is_passthrough() {
    let (pool, _path) = fresh_pool();
    let conn = pool.open_connection().expect("open conn");
    let provider = CoreProviderId::new("acme");

    seed_three_models(&conn, &provider, &["gpt-4", "claude-3", "llama-3"]);

    models::apply_auto_activation(&conn, &provider, None).expect("apply");

    let active = active_ids_for(&conn, &provider);
    assert_eq!(
        active.len(),
        3,
        "all three rows stay active when no keyword is set; got {active:?}"
    );
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn gate_f2_discovery_scheduler_invokes_auto_activation() {
    let (pool, _path) = fresh_pool();
    let mk = MasterKey::generate().unwrap();

    seed_provider_with_account(&pool, &mk, "openrouter");
    {
        let w = pool.writer();
        w.execute(
            "UPDATE providers SET auto_activate_keyword = ?1 WHERE id = ?2",
            rusqlite::params!["gpt", "openrouter"],
        )
        .expect("set keyword");
    }

    let models = vec![
        DiscoveredModel {
            model_id: ModelId::new("gpt-4"),
            display_name: Some("gpt-4".into()),
            target_format: TargetFormat::Openai,
            context_length: None,
            max_output_tokens: None,
            input_modalities: None,
            output_modalities: None,
            model_type: None,
            family: None,
            capabilities: None,
        },
        DiscoveredModel {
            model_id: ModelId::new("claude-3"),
            display_name: Some("claude-3".into()),
            target_format: TargetFormat::Openai,
            context_length: None,
            max_output_tokens: None,
            input_modalities: None,
            output_modalities: None,
            model_type: None,
            family: None,
            capabilities: None,
        },
        DiscoveredModel {
            model_id: ModelId::new("llama-3"),
            display_name: Some("llama-3".into()),
            target_format: TargetFormat::Openai,
            context_length: None,
            max_output_tokens: None,
            input_modalities: None,
            output_modalities: None,
            model_type: None,
            family: None,
            capabilities: None,
        },
    ];
    let (adapter, _counter) =
        openproxy_adapters::adapters::MockAdapter::with_discovery("openrouter", models);
    let adapters: Arc<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>> = Arc::new(vec![
        openproxy_adapters::adapters::ProviderAdapterEnum::Mock(Box::new(adapter)),
    ]);

    let sched = start(
        Arc::clone(&pool),
        Arc::new(mk),
        adapters,
        UpstreamClient::new(),
        DiscoverySchedulerConfig {
            interval_secs: 2,
            initial_stagger_secs: 1,
        },
    );

    for _ in 0..6 {
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }

    let mut active = vec![];
    for _ in 0..100 {
        active = {
            let c = pool.open_connection().expect("open conn");
            models::list_active(&c, &CoreProviderId::new("openrouter"))
                .expect("list_active")
                .into_iter()
                .map(|m| m.model_id.as_str().to_string())
                .collect::<Vec<_>>()
        };
        if active.contains(&"gpt-4".to_string())
            && !active.contains(&"claude-3".to_string())
            && !active.contains(&"llama-3".to_string())
        {
            break;
        }
        tokio::time::advance(Duration::from_secs(1)).await;
        tokio::time::sleep(Duration::from_millis(10)).await;
        tokio::task::yield_now().await;
    }

    assert!(
        active.contains(&"gpt-4".to_string()),
        "gpt-4 must remain active after auto-activation; got {active:?}"
    );
    assert!(
        !active.contains(&"claude-3".to_string()),
        "claude-3 must be deactivated by the auto-activation rule; got {active:?}"
    );
    assert!(
        !active.contains(&"llama-3".to_string()),
        "llama-3 must be deactivated by the auto-activation rule; got {active:?}"
    );

    sched.cancel();
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn gate_f2_discovery_scheduler_skips_auto_activation_on_failure() {
    let (pool, _path) = fresh_pool();
    let mk = MasterKey::generate().unwrap();
    seed_provider_with_account(&pool, &mk, "openrouter");

    {
        let w = pool.writer();
        w.execute(
            "UPDATE providers SET auto_activate_keyword = ?1 WHERE id = ?2",
            rusqlite::params!["gpt", "openrouter"],
        )
        .expect("set keyword");
    }

    {
        let conn = pool.open_connection().expect("open conn");
        models::upsert_many(
            &conn,
            &CoreProviderId::new("openrouter"),
            &[DiscoveredModel {
                model_id: ModelId::new("gpt-old"),
                display_name: Some("gpt-old".into()),
                target_format: TargetFormat::Openai,
                context_length: None,
                max_output_tokens: None,
                input_modalities: None,
                output_modalities: None,
                model_type: None,
                family: None,
                capabilities: None,
            }],
            Duration::from_hours(1),
        )
        .expect("seed gpt-old");
        models::set_active(
            &conn,
            models::find_active_by_provider_and_name(
                &conn,
                &CoreProviderId::new("openrouter"),
                "gpt-old",
            )
            .expect("find")
            .expect("present")
            .row_id,
            false,
        )
        .expect("disable");
    }

    let adapter = openproxy_adapters::adapters::ProviderAdapterEnum::Mock(Box::new(
        openproxy_adapters::adapters::MockAdapter::failing_discovery("openrouter"),
    ));
    let adapters: Arc<Vec<openproxy_adapters::adapters::ProviderAdapterEnum>> =
        Arc::new(vec![adapter]);

    let sched = start(
        Arc::clone(&pool),
        Arc::new(mk),
        adapters,
        UpstreamClient::new(),
        DiscoverySchedulerConfig {
            interval_secs: 2,
            initial_stagger_secs: 1,
        },
    );

    for _ in 0..6 {
        tokio::time::advance(Duration::from_secs(1)).await;
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }

    let still_inactive = {
        let c = pool.open_connection().expect("open conn");
        models::list_all(&c)
            .expect("list_all")
            .into_iter()
            .find(|m| m.model_id.as_str() == "gpt-old")
            .map(|m| m.active)
    };
    assert_eq!(
        still_inactive,
        Some(false),
        "gpt-old must remain inactive after a failed refresh (auto-activation hook skipped)"
    );

    sched.cancel();
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}
