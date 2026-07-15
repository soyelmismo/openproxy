#[cfg(test)]
mod tests {
    use openproxy_pipeline::repository::*;
    use openproxy_types::combos::Strategy;
    use openproxy_types::providers::{AuthType, ProviderFormat, RateLimitScope};
    use openproxy_types::ids::{ComboId, ModelRowId, ProviderId};
    use crate::combos;
    use crate::providers;
    use openproxy_pipeline::test_utils::fresh_pool;

    #[test]
    fn test_auto_populate_empty_combo() {
        let (pool, _conn, _path) = fresh_pool();
        let w = pool.writer();

        // Seed provider and models
        providers::create(
            &w,
            providers::NewProvider {
                id: &ProviderId::new("openrouter"),
                name: "or",
                base_url: "https://example.com",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: RateLimitScope::Account,
            },
        ).unwrap();
        w.execute("INSERT INTO models(provider_id, model_id, target_format) VALUES ('openrouter', 'm1', 'openai')", []).unwrap();
        w.execute("INSERT INTO models(provider_id, model_id, target_format) VALUES ('openrouter', 'm2', 'openai')", []).unwrap();

        let combo_id = combos::create_combo(&w, "c", Strategy::Priority, 1).unwrap();

        // Auto-populate
        let p: i32 = w.query_row("SELECT count(*) FROM providers WHERE active=1", [], |r| r.get(0)).unwrap(); println!("active providers: {}", p);
        let added = auto_populate_empty_combo(&w, combo_id).unwrap();
        assert_eq!(added, 1);

        let targets = list_targets(&w, combo_id).unwrap();
        assert_eq!(targets.len(), 1);
    }

    #[test]
    fn test_expand_account_rotation_fallback() {
        let (pool, _conn, _path) = fresh_pool();
        let w = pool.writer();
        let mk = openproxy_db::secrets::MasterKey::generate();
        
        providers::create(&w, providers::NewProvider {
            id: &ProviderId::new("p"),
            name: "p",
            base_url: "https://example.com",
            auth_type: AuthType::Bearer,
            format: ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: RateLimitScope::Account,
        }).unwrap();
        w.execute("INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm', 'openai')", []).unwrap();
        let model_rowid: i64 = w.query_row("SELECT last_insert_rowid()", [], |r| r.get(0)).unwrap();
        
        let account_id1 = crate::accounts::create(&w, &ProviderId::new("p"), Some("sk-1"), &mk, None, 10, None).unwrap();
        let account_id2 = crate::accounts::create(&w, &ProviderId::new("p"), Some("sk-2"), &mk, None, 20, None).unwrap();
        let account_id3 = crate::accounts::create(&w, &ProviderId::new("p"), Some("sk-3"), &mk, None, 30, None).unwrap();
        
        let combo_id = combos::create_combo(&w, "c", Strategy::Priority, 1).unwrap();
        combos::add_target(&w, combos::AddTargetInput {
            combo_id,
            provider_id: ProviderId::new("p"),
            account_id: None, // No account -> Expand all
            model_row_id: Some(ModelRowId(model_rowid)),
            sub_combo_id: None,
            priority_order: 10,
        }).unwrap();
        
        let mut targets = list_targets(&w, combo_id).unwrap();
        assert_eq!(targets.len(), 1);
        
        // Expand
        targets = expand_account_rotation(&w, targets).unwrap();
        
        // Should expand to all 3 accounts, ordered by priority
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].account_id, Some(account_id1));
        assert_eq!(targets[1].account_id, Some(account_id2));
        assert_eq!(targets[2].account_id, Some(account_id3));
        assert_eq!(targets[0].priority_order, 10);
        assert_eq!(targets[1].priority_order, 10);
        assert_eq!(targets[2].priority_order, 10);
    }
}
