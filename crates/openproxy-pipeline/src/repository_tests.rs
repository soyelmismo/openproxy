#[cfg(test)]
mod tests {
    use super::super::repository::*;
    use openproxy_db::combos::{self};
    use openproxy_db::providers::{self};
    use openproxy_types::{ProviderId, ProviderFormat, AuthType, RateLimitScope};
    use openproxy_types::combos::Strategy;
    use openproxy_types::ids::{ComboId, ModelRowId};
    use crate::test_utils::fresh_pool;

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
        let added = auto_populate_empty_combo(&w, combo_id).unwrap();
        assert_eq!(added, 2);

        let targets = list_targets(&w, combo_id).unwrap();
        assert_eq!(targets.len(), 2);
    }
}
