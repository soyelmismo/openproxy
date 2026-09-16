use super::recent::map_usage_row;

/// Minimal schema for the `usage` table, matching only the columns
/// that `map_usage_row` reads (indices 0..=34).
const TEST_USAGE_SCHEMA: &str = "CREATE TABLE usage (
    id INTEGER PRIMARY KEY,
    request_id TEXT NOT NULL,
    trace_id TEXT NOT NULL,
    provider_id TEXT NOT NULL,
    upstream_model_id TEXT NOT NULL,
    status_code INTEGER NOT NULL,
    total_ms INTEGER NOT NULL,
    prompt_tokens INTEGER,
    completion_tokens INTEGER,
    cost_usd REAL,
    connect_ms INTEGER,
    ttft_ms INTEGER,
    request_body_json TEXT,
    response_body_json TEXT,
    request_headers TEXT,
    response_headers TEXT,
    error_msg_redacted TEXT,
    error_msg TEXT,
    race_total INTEGER NOT NULL DEFAULT 0,
    race_attempts INTEGER NOT NULL DEFAULT 0,
    is_streaming INTEGER NOT NULL DEFAULT 0,
    stream_complete INTEGER NOT NULL DEFAULT 0,
    race_lost INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL,
    stop_reason TEXT,
    compression_savings_pct REAL,
    compression_techniques TEXT,
    client_response INTEGER NOT NULL DEFAULT 0,
    prompt_tokens_estimated INTEGER NOT NULL DEFAULT 0,
    completion_tokens_estimated INTEGER NOT NULL DEFAULT 0,
    endpoint_kind TEXT NOT NULL DEFAULT 'chat',
    proxy_url TEXT,
    proxy_status TEXT,
    is_proxy_rotated INTEGER NOT NULL DEFAULT 0,
    cached_tokens INTEGER,
    pii_redacted TEXT
)";

#[test]
fn map_usage_row_basic() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(TEST_USAGE_SCHEMA).unwrap();
    conn.execute(
        "INSERT INTO usage (id, request_id, trace_id, provider_id, upstream_model_id,
            status_code, total_ms, prompt_tokens, completion_tokens, cost_usd,
            connect_ms, ttft_ms, request_body_json, response_body_json,
            request_headers, response_headers, error_msg_redacted, error_msg,
            race_total, race_attempts, is_streaming, stream_complete, race_lost,
            created_at, stop_reason, compression_savings_pct, compression_techniques,
            client_response, prompt_tokens_estimated, completion_tokens_estimated,
            endpoint_kind, proxy_url, proxy_status, is_proxy_rotated, cached_tokens, pii_redacted)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                 ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
                 ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27,
                 ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36)",
        rusqlite::params![
            42i64,
            "req-abc",
            "trace-xyz",
            "openai",
            "gpt-4o",
            200i64,
            150i64,
            100i64,
            50i64,
            0.003f64,
            10i64,
            25i64,
            r#"{"model":"gpt-4o"}"#,
            r#"{"choices":[]}"#,
            r#"{"content-type":"application/json"}"#,
            r#"{"x-request-id":"abc"}"#,
            Some("redacted err"),
            None::<String>,
            2i64,
            1i64,
            1i64,
            1i64,
            0i64,
            "2026-01-01T00:00:00Z",
            Some("stop"),
            Some(12.5f64),
            Some("gzip"),
            1i64,
            0i64,
            0i64,
            "chat",
            None::<String>,
            None::<String>,
            0i64,
            Some(10i64),
            Some("email: 1"),
        ],
    )
    .unwrap();

    let row = conn
        .query_row("SELECT * FROM usage WHERE id = 42", [], map_usage_row)
        .unwrap();

    assert_eq!(row.id.0, 42);
    assert_eq!(row.request_id, "req-abc");
    assert_eq!(row.trace_id, "trace-xyz");
    assert_eq!(row.provider_id.as_str(), "openai");
    assert_eq!(row.upstream_model_id, "gpt-4o");
    assert_eq!(row.status_code, 200);
    assert_eq!(row.total_ms, 150);
    assert_eq!(row.prompt_tokens, Some(100));
    assert_eq!(row.completion_tokens, Some(50));
    assert!((row.cost_usd.unwrap() - 0.003).abs() < f64::EPSILON);
    assert_eq!(row.connect_ms, Some(10));
    assert_eq!(row.ttft_ms, Some(25));
    assert_eq!(row.stop_reason.as_deref(), Some("stop"));
    assert_eq!(row.compression_savings_pct, Some(12.5));
    assert_eq!(row.compression_techniques.as_deref(), Some("gzip"));
    assert_eq!(row.pii_redacted.as_deref(), Some("email: 1"));
    assert_eq!(row.proxy_url, None);
    assert_eq!(row.proxy_status, None);
    assert_eq!(row.cached_tokens, Some(10));
    // flags: IS_STREAMING(2) | STREAM_COMPLETE(4) | CLIENT_RESPONSE(8) = 14
    assert_eq!(row.flags, 14);
}

#[test]
fn map_usage_row_negative_total_ms_returns_error() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(TEST_USAGE_SCHEMA).unwrap();
    conn.execute_batch(
        "INSERT INTO usage (id, request_id, trace_id, provider_id, upstream_model_id,
            status_code, total_ms, prompt_tokens, completion_tokens, cost_usd,
            connect_ms, ttft_ms, request_body_json, response_body_json,
            request_headers, response_headers, error_msg_redacted, error_msg,
            race_total, race_attempts, is_streaming, stream_complete, race_lost,
            created_at, stop_reason, compression_savings_pct, compression_techniques,
            client_response, prompt_tokens_estimated, completion_tokens_estimated,
            endpoint_kind, proxy_url, proxy_status, is_proxy_rotated, cached_tokens, pii_redacted)
         VALUES (1, 'r', 't', 'p', 'm', 200, -1, NULL, NULL, NULL,
                 NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL,
                 0, 0, 0, 0, 0, '2026-01-01', NULL, NULL, NULL,
                 0, 0, 0, 'chat', NULL, NULL, 0, NULL, NULL)",
    )
    .unwrap();

    let result = conn.query_row("SELECT * FROM usage WHERE id = 1", [], map_usage_row);
    assert!(result.is_err(), "negative total_ms should fail");
}
