#[cfg(test)]
mod tests {
    use crate::repository::*;
    use openproxy_types::ids::{ComboId, ModelRowId, ProviderId, AccountId};
    use crate::test_utils::fresh_pool;

    #[test]
    fn test_auto_populate_empty_combo() {
        let (pool, _conn, _path) = fresh_pool();
        let w = pool.writer();

        // Seed provider and models
        w.execute("INSERT INTO providers(id, name, base_url, auth_type, format, rate_limit_scope, active) VALUES ('openrouter', 'or', 'https://example.com', 'bearer', 'openai', 'account', 1)", []).unwrap();
        w.execute("INSERT INTO models(provider_id, model_id, target_format) VALUES ('openrouter', 'm1', 'openai')", []).unwrap();
        w.execute("INSERT INTO models(provider_id, model_id, target_format) VALUES ('openrouter', 'm2', 'openai')", []).unwrap();

        w.execute("INSERT INTO accounts(provider_id, api_key_type, name) VALUES ('openrouter', 'plaintext', 'test')", []).unwrap();
        
        w.execute("INSERT INTO combos(id, strategy, retries) VALUES (1, 'priority', 1)", []).unwrap();
        let combo_id = ComboId(1);

        // Auto-populate
        let added = auto_populate_empty_combo(&w, combo_id).unwrap();
        assert_eq!(added, 2);

        let targets = list_targets(&w, combo_id).unwrap();
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn test_expand_account_rotation_fallback() {
        let (pool, _conn, _path) = fresh_pool();
        let w = pool.writer();

        w.execute("INSERT INTO providers(id, name, base_url, auth_type, format, rate_limit_scope, active) VALUES ('p', 'p', 'https://example.com', 'bearer', 'openai', 'account', 1)", []).unwrap();
        w.execute("INSERT INTO models(provider_id, model_id, target_format) VALUES ('p', 'm', 'openai')", []).unwrap();
        let model_rowid: i64 = w.query_row("SELECT last_insert_rowid()", [], |r| r.get(0)).unwrap();

        w.execute("INSERT INTO accounts(provider_id, api_key_type, name) VALUES ('p', 'plaintext', 'sk-1')", []).unwrap();
        let account_id1 = AccountId(w.query_row("SELECT last_insert_rowid()", [], |r| r.get(0)).unwrap());
        w.execute("INSERT INTO accounts(provider_id, api_key_type, name) VALUES ('p', 'plaintext', 'sk-2')", []).unwrap();
        let account_id2 = AccountId(w.query_row("SELECT last_insert_rowid()", [], |r| r.get(0)).unwrap());
        w.execute("INSERT INTO accounts(provider_id, api_key_type, name) VALUES ('p', 'plaintext', 'sk-3')", []).unwrap();
        let account_id3 = AccountId(w.query_row("SELECT last_insert_rowid()", [], |r| r.get(0)).unwrap());

        w.execute("INSERT INTO combos(id, strategy, retries) VALUES (1, 'priority', 1)", []).unwrap();
        let combo_id = ComboId(1);
        
        w.execute("INSERT INTO combo_targets(combo_id, provider_id, model_row_id, priority_order) VALUES (1, 'p', ?1, 10)", [model_rowid]).unwrap();

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
