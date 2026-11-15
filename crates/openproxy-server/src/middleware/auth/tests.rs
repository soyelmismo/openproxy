use super::*;
use openproxy_adapters::adapters;
use openproxy_core::AppConfig;
use openproxy_db as core_db;
use openproxy_db::MasterKey;
use parking_lot::RwLock;
use std::sync::Arc;

async fn make_test_state() -> AppState {
    tokio::task::spawn_blocking(|| {
        let pool =
            core_db::DbPool::test_pool_with_prefix("openproxy-auth-test").expect("open pool");
        let db_pool = Arc::new(pool);
        let master_key = Arc::new(MasterKey::generate().unwrap());
        let adapters = Arc::new(RwLock::new(Arc::new(
            Vec::<adapters::ProviderAdapterEnum>::new(),
        )));
        let mut config = AppConfig::default();
        config.server.allow_anonymous = false;
        AppState::for_test(config, db_pool, master_key, adapters)
    })
    .await
    .expect("make_test_state join")
}

#[test]
fn test_last_used_needs_stamp_unit() {
    assert!(last_used_needs_stamp(None));
    assert!(last_used_needs_stamp(Some(&"malformed_date".to_string())));

    let old_time = (chrono::Utc::now()
        - chrono::Duration::seconds(core_api_keys::LAST_USED_THROTTLE_SECS + 30))
    .format("%Y-%m-%d %H:%M:%S")
    .to_string();
    assert!(last_used_needs_stamp(Some(&old_time)));

    let recent_time = (chrono::Utc::now()
        - chrono::Duration::seconds(core_api_keys::LAST_USED_THROTTLE_SECS.min(60) / 2))
    .format("%Y-%m-%d %H:%M:%S")
    .to_string();
    assert!(!last_used_needs_stamp(Some(&recent_time)));
}

#[tokio::test]
async fn test_verify_key_credentials_stamps_and_throttles() {
    let state = make_test_state().await;

    // Install an UPDATE trigger on `api_keys` to count real disk/SQL writes to `last_used_at`
    state
        .db_pool()
        .spawn_write(|conn| {
            conn.execute_batch(
                "CREATE TABLE test_touch_counter (count INTEGER NOT NULL);\
                 INSERT INTO test_touch_counter (count) VALUES (0);\
                 CREATE TRIGGER test_count_last_used_touch AFTER UPDATE OF last_used_at ON api_keys \
                 BEGIN \
                     UPDATE test_touch_counter SET count = count + 1; \
                 END;",
            )
            .map_err(openproxy_db::error::map_db_error)
        })
        .await
        .expect("setup touch counter trigger");

    let input = core_api_keys::CreateApiKeyInput {
        label: Some("test-key".into()),
        scopes: vec!["chat".into()],
        ..Default::default()
    };
    let (key_row, token) = state
        .db_pool()
        .spawn_write(move |conn| core_api_keys::create(conn, input, "test_admin"))
        .await
        .expect("create key");
    assert!(key_row.last_used_at.is_none());

    // Initial trigger count must be 0
    let initial_count: i64 = state
        .db_pool()
        .spawn_read(|conn| {
            conn.query_row("SELECT count FROM test_touch_counter", [], |row| row.get(0))
                .map_err(openproxy_db::error::map_db_error)
        })
        .await
        .expect("read initial count");
    assert_eq!(initial_count, 0);

    // First verification: stamps last_used_at via awaited spawn_write and caches it
    let verified = verify_key_credentials(&state, &token, "chat")
        .await
        .expect("first verification succeeds");
    assert_eq!(verified.id, key_row.id);

    // Verify the DB row has the new timestamp
    let key_id = key_row.id;
    let in_db = state
        .db_pool()
        .spawn_read(move |conn| {
            core_api_keys::get_by_id(conn, key_id)?
                .ok_or_else(|| CoreError::Internal("key not found in db".into()))
        })
        .await
        .expect("db read");
    assert!(
        in_db.last_used_at.is_some(),
        "DB row must have last_used_at updated"
    );

    // Verify trigger count is exactly 1
    let count_after_first: i64 = state
        .db_pool()
        .spawn_read(|conn| {
            conn.query_row("SELECT count FROM test_touch_counter", [], |row| row.get(0))
                .map_err(openproxy_db::error::map_db_error)
        })
        .await
        .expect("read count after first verify");
    assert_eq!(
        count_after_first, 1,
        "First verification must trigger exactly 1 write"
    );

    // Second verification immediately: hits cached key and throttle skips DB write
    let cached = verify_key_credentials(&state, &token, "chat")
        .await
        .expect("second verification succeeds");
    assert_eq!(cached.id, key_row.id);
    assert!(cached.last_used_at.is_some());

    // Verify trigger count is STILL 1, proving NO second write occurred!
    let count_after_second: i64 = state
        .db_pool()
        .spawn_read(|conn| {
            conn.query_row("SELECT count FROM test_touch_counter", [], |row| row.get(0))
                .map_err(openproxy_db::error::map_db_error)
        })
        .await
        .expect("read count after second verify");
    assert_eq!(
        count_after_second, 1,
        "Second verification inside throttle window must NOT perform any second write"
    );
}

#[tokio::test]
async fn test_verify_key_credentials_scope_and_inactive() {
    let state = make_test_state().await;

    let input = core_api_keys::CreateApiKeyInput {
        label: Some("scope-test".into()),
        scopes: vec!["chat".into()],
        ..Default::default()
    };
    let (key_row, token) = state
        .db_pool()
        .spawn_write(move |conn| core_api_keys::create(conn, input, "test_admin"))
        .await
        .expect("create test key");

    // Missing required scope
    let err = verify_key_credentials(&state, &token, "manage")
        .await
        .unwrap_err();
    match err.0 {
        CoreError::Auth(msg) => assert!(msg.contains("lacks required scope")),
        other => panic!("expected auth error, got: {other:?}"),
    }

    // Revoke the key via spawn_write
    let key_id = key_row.id;
    state
        .db_pool()
        .spawn_write(move |conn| core_api_keys::revoke(conn, key_id))
        .await
        .expect("revoke key");

    // Invalidate cache
    state.invalidate_api_key_cache(None);

    // Inactive key rejected
    let err = verify_key_credentials(&state, &token, "chat")
        .await
        .unwrap_err();
    match err.0 {
        CoreError::Auth(msg) => assert!(msg.contains("revoked or inactive")),
        other => panic!("expected auth error, got: {other:?}"),
    }
}

#[tokio::test]
async fn test_verify_key_credentials_survives_shutdown() {
    let state = make_test_state().await;
    let input = core_api_keys::CreateApiKeyInput {
        label: Some("shutdown-test".into()),
        scopes: vec!["chat".into()],
        ..Default::default()
    };
    let (_key_row, token) = state
        .db_pool()
        .spawn_write(move |conn| core_api_keys::create(conn, input, "test_admin"))
        .await
        .expect("create test key");

    state
        .shutdown_usage_worker()
        .await
        .expect("shutdown usage worker");

    let verified = verify_key_credentials(&state, &token, "chat")
        .await
        .expect("verification succeeds during shutdown");

    let key_id = verified.id;
    let db_key = state
        .db_pool()
        .spawn_read(move |conn| {
            core_api_keys::get_by_id(conn, key_id)?
                .ok_or_else(|| CoreError::Internal("key not found in db".into()))
        })
        .await
        .expect("fetch db key");
    assert!(
        db_key.last_used_at.is_some(),
        "DB row must have last_used_at persisted even after shutdown"
    );
}

#[test]
fn test_sanitize_tool_calls_disambiguates_duplicate_ids() {
    use openproxy_types::OpenAIMessage;
    use serde_json::json;

    let mut messages = vec![
        OpenAIMessage {
            role: "assistant".to_string(),
            content: Some(json!("Calling tools in parallel")),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![
                json!({"id": "call_0", "type": "function", "function": {"name": "terminal", "arguments": "{}"}}),
                json!({"id": "call_0", "type": "function", "function": {"name": "web_search", "arguments": "{}"}}),
                json!({"id": "call_0", "type": "function", "function": {"name": "terminal", "arguments": "{}"}}),
            ]),
            extra: serde_json::Map::new(),
        },
        OpenAIMessage {
            role: "tool".to_string(),
            content: Some(json!("res 1")),
            name: None,
            tool_call_id: Some("call_0".to_string()),
            tool_calls: None,
            extra: serde_json::Map::new(),
        },
        OpenAIMessage {
            role: "tool".to_string(),
            content: Some(json!("res 2")),
            name: None,
            tool_call_id: Some("call_0".to_string()),
            tool_calls: None,
            extra: serde_json::Map::new(),
        },
        OpenAIMessage {
            role: "tool".to_string(),
            content: Some(json!("res 3")),
            name: None,
            tool_call_id: Some("call_0".to_string()),
            tool_calls: None,
            extra: serde_json::Map::new(),
        },
        OpenAIMessage {
            role: "user".to_string(),
            content: Some(json!("next prompt")),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extra: serde_json::Map::new(),
        },
    ];

    sanitize_tool_calls(&mut messages);

    // Assistant tool_calls should have unique IDs
    let assistant_calls = messages[0].tool_calls.as_ref().unwrap();
    assert_eq!(assistant_calls.len(), 3);
    assert_eq!(assistant_calls[0]["id"], "call_0");
    assert_eq!(assistant_calls[1]["id"], "call_0_1");
    assert_eq!(assistant_calls[2]["id"], "call_0_2");

    // Tool messages should have updated matching IDs in 1-to-1 order
    assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_0"));
    assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_0_1"));
    assert_eq!(messages[3].tool_call_id.as_deref(), Some("call_0_2"));
}
