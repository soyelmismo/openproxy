use super::*;
use openproxy_types::CoreError;

const EXPECTED_COLUMNS: [&str; 36] = [
    "id",
    "request_id",
    "trace_id",
    "provider_id",
    "upstream_model_id",
    "status_code",
    "total_ms",
    "prompt_tokens",
    "completion_tokens",
    "cost_usd",
    "connect_ms",
    "ttft_ms",
    "request_body_json",
    "response_body_json",
    "request_headers",
    "response_headers",
    "error_msg_redacted",
    "error_msg",
    "race_total",
    "race_attempts",
    "is_streaming",
    "stream_complete",
    "race_lost",
    "created_at",
    "stop_reason",
    "compression_savings_pct",
    "compression_techniques",
    "client_response",
    "prompt_tokens_estimated",
    "completion_tokens_estimated",
    "endpoint_kind",
    "proxy_url",
    "proxy_status",
    "is_proxy_rotated",
    "cached_tokens",
    "pii_redacted",
];

#[test]
fn test_projection_column_contract() {
    let conn = openproxy_db::testing::open_in_memory();
    let queries = [
        recent_usage_select!("WHERE id > ?1 ORDER BY id ASC LIMIT ?2"),
        recent_usage_select!("ORDER BY id DESC LIMIT ?1"),
        recent_usage_select!("WHERE id = ?1"),
    ];
    for sql in queries {
        let stmt = conn.prepare(sql).unwrap();
        assert_eq!(stmt.column_count(), 36);
        assert_eq!(stmt.column_names(), EXPECTED_COLUMNS);
    }
}

#[test]
fn test_recent_queries_ordering_and_equivalence() {
    let conn = openproxy_db::testing::open_in_memory();
    let insert_sql = "INSERT INTO usage (
        id, request_id, trace_id, provider_id, upstream_model_id, status_code, total_ms,
        prompt_tokens, completion_tokens, cost_usd, connect_ms, ttft_ms, request_body_json,
        response_body_json, request_headers, response_headers, error_msg_redacted, error_msg,
        race_total, race_attempts, is_streaming, stream_complete, race_lost, created_at,
        stop_reason, compression_savings_pct, compression_techniques, client_response,
        prompt_tokens_estimated, completion_tokens_estimated, endpoint_kind, proxy_url,
        proxy_status, is_proxy_rotated, cached_tokens, pii_redacted
    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18,
              ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36)";

    for id in [3i64, 1, 2] {
        let headers = serde_json::json!({"h": id.to_string()}).to_string();
        conn.execute(
            insert_sql,
            rusqlite::params![
                id,
                format!("req-{id}"),
                format!("tr-{id}"),
                "test-provider",
                format!("model-{id}"),
                200i64,
                id * 100,
                Some(100i64),
                Some(50i64),
                Some(0.002f64),
                Some(10i64),
                Some(20i64),
                Some(r#"{"in":1}"#),
                Some(r#"{"out":2}"#),
                headers,
                headers,
                None::<String>,
                None::<String>,
                1i64,
                1i64,
                1i64,
                1i64,
                0i64,
                format!("2026-10-01T12:00:0{id}Z"),
                Some("stop"),
                Some(id as f64 * 10.0),
                Some("gzip"),
                1i64,
                0i64,
                0i64,
                "chat",
                None::<String>,
                None::<String>,
                0i64,
                Some(id * 10),
                Some("pii-ok")
            ],
        )
        .unwrap();
    }

    let since1_lim1 = recent(&conn, 1, 1).unwrap();
    assert_eq!(since1_lim1.len(), 1);
    assert_eq!(since1_lim1[0].id.0, 2);

    let all_asc = recent(&conn, 0, 10).unwrap();
    let asc_ids: Vec<i64> = all_asc.iter().map(|r| r.id.0).collect();
    assert_eq!(asc_ids, vec![1, 2, 3]);

    let desc_lim2 = recent_desc(&conn, 2).unwrap();
    let desc_ids: Vec<i64> = desc_lim2.iter().map(|r| r.id.0).collect();
    assert_eq!(desc_ids, vec![3, 2]);

    let row_by_id2 = row_for_broadcast_by_id(&conn, 2)
        .unwrap()
        .expect("id 2 exists");
    let json_by_id = serde_json::to_value(&row_by_id2).unwrap();
    let json_recent = serde_json::to_value(&all_asc[1]).unwrap();
    let json_desc = serde_json::to_value(&desc_lim2[1]).unwrap();
    assert_eq!(json_by_id, json_recent);
    assert_eq!(json_by_id, json_desc);
    assert_eq!(row_by_id2.cached_tokens, Some(20));
    assert_eq!(
        row_by_id2.request_headers,
        Some(std::collections::BTreeMap::from([(
            "h".to_string(),
            "2".to_string()
        )]))
    );
    assert_eq!(row_by_id2.flags, 14);
    assert_eq!(row_by_id2.compression_savings_pct, Some(20.0));
    assert_eq!(row_by_id2.pii_redacted.as_deref(), Some("pii-ok"));

    assert!(row_for_broadcast_by_id(&conn, 999).unwrap().is_none());
    assert!(recent(&conn, 0, 0).unwrap().is_empty());
    assert!(recent_desc(&conn, 0).unwrap().is_empty());
}

#[test]
fn test_missing_table_error_contract() {
    let empty_conn = rusqlite::Connection::open_in_memory().unwrap();

    for error in [
        recent(&empty_conn, 0, 10).unwrap_err(),
        recent_desc(&empty_conn, 10).unwrap_err(),
        row_for_broadcast_by_id(&empty_conn, 1).unwrap_err(),
    ] {
        let CoreError::Database { message, source } = error else {
            panic!("expected CoreError::Database, got {error:?}");
        };
        let source = source.expect("original SQL source");
        let sqlite = source.downcast_ref::<rusqlite::Error>().unwrap();
        assert_eq!(message, sqlite.to_string());
    }
}
