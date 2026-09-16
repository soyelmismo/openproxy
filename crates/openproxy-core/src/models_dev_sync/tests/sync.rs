use super::{TEST_JSON, create_sync_table};
use crate::models_dev_sync::*;
use rusqlite::Connection;

#[test]
fn upsert_parses_nested_format_and_stores_pricing() {
    let conn = Connection::open_in_memory().unwrap();
    create_sync_table(&conn);

    let count = upsert_models_dev(TEST_JSON.as_bytes(), &conn).unwrap();
    assert_eq!(
        count, 6,
        "should upsert 6 rows (4 opencode+opencode-zen, 2 google+gemini)"
    );
}

#[test]
fn lookup_with_db_exact_match_returns_pricing() {
    let conn = Connection::open_in_memory().unwrap();
    create_sync_table(&conn);

    upsert_models_dev(TEST_JSON.as_bytes(), &conn).unwrap();

    let price = crate::pricing::lookup_with_db(&conn, "gemini", "gemini-2.5-pro");
    assert!(price.is_some(), "glm should have pricing");
    let p = price.unwrap();
    assert!((p.input_per_1m - 1.25).abs() < 1e-9);
    assert!((p.output_per_1m - 10.0).abs() < 1e-9);
}

#[test]
fn lookup_with_db_fuzzy_free_suffix_fallback() {
    let conn = Connection::open_in_memory().unwrap();
    create_sync_table(&conn);

    upsert_models_dev(TEST_JSON.as_bytes(), &conn).unwrap();

    let price = crate::pricing::lookup_with_db(&conn, "opencode-zen", "deepseek-v4-flash");
    assert!(
        price.is_some(),
        "deepseek-v4-flash should have pricing via exact match"
    );
    let p = price.unwrap();
    assert!(
        (p.input_per_1m - 0.14).abs() < 1e-9,
        "paid model should be $0.14, got {}",
        p.input_per_1m
    );

    let price = crate::pricing::lookup_with_db(&conn, "opencode-zen", "deepseek-v4-flash-free");
    assert!(price.is_some());
    let p = price.unwrap();
    assert!(
        (p.input_per_1m - 0.0).abs() < 1e-9,
        "free model should be $0, got {}",
        p.input_per_1m
    );

    let price =
        crate::pricing::lookup_with_db(&conn, "opencode-zen", "deepseek-v4-flash-free-trial");
    assert!(
        price.is_some(),
        "fuzzy fallback should strip -free-trial and match"
    );
    let p = price.unwrap();
    assert!((p.input_per_1m - 0.14).abs() < 1e-9);

    assert!(crate::pricing::lookup_with_db(&conn, "opencode-zen", "no-such-model").is_none());
}

#[test]
fn lookup_with_db_falls_back_to_static_table() {
    let conn = Connection::open_in_memory().unwrap();
    create_sync_table(&conn);

    let price = crate::pricing::lookup_with_db(&conn, "openrouter", "openai/gpt-4o");
    assert!(price.is_some(), "should fall back to static table");
    let p = price.unwrap();
    assert!((p.input_per_1m - 2.5).abs() < 1e-9);
}

#[test]
fn lookup_with_db_normalized_matches_date_suffix() {
    let conn = Connection::open_in_memory().unwrap();
    create_sync_table(&conn);

    let json = r#"{
        "anthropic": {
            "id": "anthropic",
            "models": {
                "claude-3-5-sonnet": {
                    "id": "claude-3-5-sonnet",
                    "tool_call": true,
                    "reasoning": false,
                    "structured_output": true,
                    "limit": { "context": 200000, "output": 8192 },
                    "cost": { "input": 3.0, "output": 15.0, "cache_read": 0.3 },
                    "family": "claude-3.5",
                    "status": "active"
                }
            }
        }
    }"#;
    upsert_models_dev(json.as_bytes(), &conn).unwrap();

    let price =
        crate::pricing::lookup_with_db(&conn, "openrouter", "anthropic/claude-3-5-sonnet-20241022");
    assert!(
        price.is_some(),
        "normalized lookup should match the date-suffixed model"
    );
    let p = price.unwrap();
    assert!(
        (p.input_per_1m - 3.0).abs() < 1e-9,
        "expected $3.0/1M from claude-3-5-sonnet, got {}",
        p.input_per_1m
    );
    assert!((p.output_per_1m - 15.0).abs() < 1e-9);
}

#[test]
fn enrich_via_normalized_matches_date_suffix() {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE models (
             provider_id         TEXT NOT NULL,
             model_id            TEXT NOT NULL,
             context_length      INTEGER,
             max_output_tokens   INTEGER,
             capabilities_json   TEXT,
             family              TEXT,
             input_modalities_json TEXT,
             output_modalities_json TEXT,
             model_type          TEXT NOT NULL DEFAULT 'chat',
             custom              INTEGER NOT NULL DEFAULT 0,
             model_id_normalized TEXT,
             UNIQUE(provider_id, model_id)
         );",
    )
    .unwrap();
    create_sync_table(&conn);

    let json = r#"{
        "anthropic": {
            "id": "anthropic",
            "models": {
                "claude-3-5-sonnet": {
                    "id": "claude-3-5-sonnet",
                    "tool_call": true,
                    "reasoning": false,
                    "structured_output": true,
                    "limit": { "context": 200000, "output": 8192 },
                    "cost": { "input": 3.0, "output": 15.0, "cache_read": 0.3 },
                    "family": "claude-3.5",
                    "status": "active"
                }
            }
        }
    }"#;
    upsert_models_dev(json.as_bytes(), &conn).unwrap();

    let normalized =
        crate::model_normalize::normalize_model_id("anthropic/claude-3-5-sonnet-20241022");
    conn.execute(
        "INSERT INTO models (provider_id, model_id, context_length, custom, model_id_normalized) \
         VALUES ('openrouter', 'anthropic/claude-3-5-sonnet-20241022', 128000, 0, ?1)",
        rusqlite::params![&normalized],
    )
    .unwrap();

    let touched = enrich_models_from_sync(&conn).unwrap();
    assert!(touched >= 1, "enrichment should touch at least one row");

    let ctx: i64 = conn
        .query_row(
            "SELECT context_length FROM models \
             WHERE provider_id = 'openrouter' \
               AND model_id = 'anthropic/claude-3-5-sonnet-20241022'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        ctx, 200_000,
        "context_length should be refreshed to models.dev's 200000, got {ctx}"
    );
}

#[test]
fn test_resolved_provider_map_uses_adapter_metadata() {
    let map = &*RESOLVED_PROVIDER_MAP;

    let google_mapped = map.get("google").expect("google must be mapped");
    assert!(google_mapped.iter().any(|s| &**s == "gemini"));

    let minimax_mapped = map.get("minimax").expect("minimax must be mapped");
    assert!(minimax_mapped.iter().any(|s| &**s == "minimax"));
    assert!(minimax_mapped.iter().any(|s| &**s == "minimax-cn"));

    let openai_mapped = map.get("openai").expect("openai must be mapped");
    assert!(openai_mapped.iter().any(|s| &**s == "openrouter"));

    let anthropic_mapped = map.get("anthropic").expect("anthropic must be mapped");
    assert!(anthropic_mapped.iter().any(|s| &**s == "openrouter"));

    let meta_mapped = map.get("meta").expect("meta must be mapped");
    assert!(meta_mapped.iter().any(|s| &**s == "openrouter"));

    let nvidia_mapped = map.get("nvidia").expect("nvidia must be mapped");
    assert!(nvidia_mapped.iter().any(|s| &**s == "nvidia-nim"));

    let opencode_mapped = map.get("opencode").expect("opencode must be mapped");
    assert!(opencode_mapped.iter().any(|s| &**s == "opencode-zen"));

    let opencode_go_mapped = map.get("opencode-go").expect("opencode-go must be mapped");
    assert!(opencode_go_mapped.iter().any(|s| &**s == "opencode-go"));
}
