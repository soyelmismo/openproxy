use crate::pricing;
use once_cell::sync::Lazy;
use openproxy_types::ids::UsageId;
use openproxy_types::usage::{RecentUsageRow, UsageInput, publish_usage_row};
use rusqlite::{Connection, params};

pub fn compute(price: Option<pricing::Price>, input: &UsageInput) -> (f64, Option<f64>) {
    let cost = pricing::compute_cost(
        price,
        input.prompt_tokens.unwrap_or(0),
        input.completion_tokens.unwrap_or(0),
    );
    let tps = match (input.completion_tokens, input.ttft_ms) {
        (Some(c), Some(ttft)) if c > 0 && input.total_ms > ttft => {
            let denom = (input.total_ms - ttft) as f64;
            Some(c as f64 * 1000.0 / denom)
        }
        _ => None,
    };
    (cost, tps)
}

pub fn redact_error_msg(raw: &str) -> (String, String) {
    static RE_SK: Lazy<regex::Regex> =
        Lazy::new(|| regex::Regex::new(r"sk-[A-Za-z0-9_\-]{10,}").unwrap());
    static RE_XAPIKEY: Lazy<regex::Regex> =
        Lazy::new(|| regex::Regex::new(r"(?i)x-api-key:\s*\S+").unwrap());
    static RE_BEARER: Lazy<regex::Regex> =
        Lazy::new(|| regex::Regex::new(r"(?i)Authorization:\s*Bearer\s+\S+").unwrap());

    let mut sanitized = raw.to_string();
    if RE_SK.is_match(&sanitized) {
        sanitized = RE_SK.replace_all(&sanitized, "sk-[REDACTED]").into_owned();
    }
    if RE_XAPIKEY.is_match(&sanitized) {
        sanitized = RE_XAPIKEY
            .replace_all(&sanitized, "x-api-key: [REDACTED]")
            .into_owned();
    }
    if RE_BEARER.is_match(&sanitized) {
        sanitized = RE_BEARER
            .replace_all(&sanitized, "Authorization: Bearer [REDACTED]")
            .into_owned();
    }
    if sanitized.len() > 2048 {
        let mut idx = 2048;
        while idx > 0 && !sanitized.is_char_boundary(idx) {
            idx -= 1;
        }
        sanitized.truncate(idx);
        sanitized.push_str("...[truncated]");
    }
    (sanitized.clone(), sanitized)
}

pub fn record(conn: &Connection, input: &UsageInput) -> openproxy_types::Result<UsageId> {
    let price = pricing::lookup_with_db(conn, input.provider_id.as_str(), &input.upstream_model_id);
    if price.is_none()
        && (input.prompt_tokens.unwrap_or(0) > 0 || input.completion_tokens.unwrap_or(0) > 0)
    {
        tracing::warn!(
            provider_id = %input.provider_id,
            upstream_model_id = %input.upstream_model_id,
            "no pricing data found; recording cost_usd = 0 (run models.dev sync or set pricing manually)"
        );
    }
    let (cost_usd, tps) = compute(price, input);
    let (error_msg_for_db, error_msg_redacted_for_db) = match &input.error_msg {
        Some(msg) => {
            let (sanitized, _redacted) = redact_error_msg(msg);
            (Some(sanitized.clone()), Some(sanitized))
        }
        None => (None, None),
    };

    let request_id = input.request_id.to_string();
    let trace_id = input.trace_id.clone();

    conn.execute(
        "INSERT INTO usage (\
            request_id, trace_id, attempt, provider_id, account_id, combo_id, \
            model_row_id, upstream_model_id, combo_target_id, prompt_tokens, \
            completion_tokens, cost_usd, connect_ms, ttft_ms, total_ms, \
            tokens_per_sec, status_code, error_msg, error_msg_redacted, \
            race_total, race_attempts, race_lost, api_key_id, created_at, \
            request_body_json, response_body_json, request_headers, \
            response_headers, error_message, is_streaming, stream_complete, \
            stop_reason, compression_savings_pct, compression_techniques, \
            client_response, prompt_tokens_estimated, completion_tokens_estimated, \
            endpoint_kind, proxy_url, proxy_status, is_proxy_rotated\
         ) VALUES (\
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
            ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, \
            ?21, ?22, ?23, datetime('now'), ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, \
            ?37, ?38, ?39, ?40\
         )",
        params![
            request_id,
            trace_id,
            input.attempt as i64,
            input.provider_id.as_str(),
            input.account_id.map(|a| a.0),
            input.combo_id.map(|c| c.0),
            input.model_row_id.map(|m| m.0),
            input.upstream_model_id,
            input.combo_target_id.map(|c| c.0),
            input.prompt_tokens.map(|p| p as i64),
            input.completion_tokens.map(|c| c as i64),
            cost_usd,
            input.connect_ms.map(|c| c as i64),
            input.ttft_ms.map(|t| t as i64),
            input.total_ms as i64,
            tps,
            input.status_code as i64,
            error_msg_for_db,
            error_msg_redacted_for_db,
            input.race_total as i64,
            input.race_attempts as i64,
            input.race_lost as i64,
            input.api_key_id.map(|k| k.0),
            input
                .request_body_json
                .as_ref()
                .and_then(|j| std::str::from_utf8(j).ok()),
            input
                .response_body_json
                .as_ref()
                .and_then(|j| serde_json::to_string(j).ok()),
            input
                .request_headers
                .as_ref()
                .and_then(|h| serde_json::to_string(h).ok()),
            input
                .response_headers
                .as_ref()
                .and_then(|h| serde_json::to_string(h).ok()),
            error_msg_redacted_for_db.clone(),
            input.is_streaming as i64,
            input.stream_complete as i64,
            input.stop_reason,
            input.compression_savings_pct,
            input.compression_techniques,
            input.client_response as i64,
            input.prompt_tokens_estimated as i64,
            input.completion_tokens_estimated as i64,
            input.endpoint_kind.as_str(),
            input.proxy_url,
            input.proxy_status,
            input.is_proxy_rotated as i64,
        ],
    )
    .map_err(crate::error::map_db_error)?;

    let rowid = conn.last_insert_rowid();

    let row = RecentUsageRow {
        id: UsageId(rowid),
        request_id,
        trace_id,
        provider_id: input.provider_id.clone(),
        upstream_model_id: input.upstream_model_id.clone(),
        status_code: input.status_code,
        total_ms: input.total_ms,
        prompt_tokens: input.prompt_tokens,
        completion_tokens: input.completion_tokens,
        cost_usd: Some(cost_usd),
        race_lost: input.race_lost,
        created_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        connect_ms: input.connect_ms,
        ttft_ms: input.ttft_ms,
        request_body_json: None,
        response_body_json: None,
        request_headers: None,
        response_headers: None,
        error_message: error_msg_redacted_for_db.clone(),
        race_total: Some(input.race_total),
        race_attempts: Some(input.race_attempts),
        is_streaming: input.is_streaming,
        stream_complete: input.stream_complete,
        stop_reason: input.stop_reason.clone(),
        compression_savings_pct: input.compression_savings_pct,
        compression_techniques: input.compression_techniques.clone(),
        client_response: input.client_response,
        prompt_tokens_estimated: input.prompt_tokens_estimated,
        completion_tokens_estimated: input.completion_tokens_estimated,
        proxy_url: input.proxy_url.clone(),
        proxy_status: input.proxy_status.clone(),
        is_proxy_rotated: input.is_proxy_rotated,
        endpoint_kind: input.endpoint_kind,
    };
    publish_usage_row(row);

    Ok(UsageId(rowid))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::DbPool;
    use openproxy_types::endpoint::EndpointKind;
    use openproxy_types::ids::{ProviderId, RequestId};
    use openproxy_types::usage::UsageInput;
    use std::sync::atomic::AtomicU64;

    fn tempdir() -> std::path::PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("openproxy-cost-test-{}-{}-{}", pid, nanos, n));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        dir
    }

    #[test]
    fn test_compute_cost_and_tps() {
        let price = crate::pricing::Price {
            input_per_1m: 1.0,
            output_per_1m: 2.0,
            kind: "chat".to_string(),
        };

        let mut input = UsageInput {
            request_id: RequestId::new(),
            trace_id: "trace-123".to_string(),
            attempt: 1,
            provider_id: ProviderId::new("test-provider"),
            account_id: None,
            combo_id: None,
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id: "test-model".to_string(),
            prompt_tokens: Some(100),
            completion_tokens: Some(200),
            connect_ms: None,
            ttft_ms: Some(100),
            total_ms: 1100,
            status_code: 200,
            error_msg: None,
            race_total: 1,
            race_lost: false,
            api_key_id: None,
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            error_message: None,
            race_attempts: 1,
            is_streaming: true,
            stream_complete: true,
            stop_reason: None,
            compression_savings_pct: None,
            compression_techniques: None,
            client_response: true,
            prompt_tokens_estimated: false,
            completion_tokens_estimated: false,
            endpoint_kind: EndpointKind::Chat,
            proxy_url: None,
            proxy_status: None,
            is_proxy_rotated: false,
        };

        let (cost, tps) = compute(Some(price), &input);
        assert_eq!(cost, 0.0005); // 100 * 1.0/1M + 200 * 2.0/1M = 0.0001 + 0.0004 = 0.0005
        assert_eq!(tps, Some(200.0)); // 200 tokens / (1100ms - 100ms) * 1000 = 200.0

        // Test with None TTFT
        input.ttft_ms = None;
        let (_, tps2) = compute(Some(crate::pricing::Price::default()), &input);
        assert_eq!(tps2, None);

        // Test with 0 completion tokens
        input.ttft_ms = Some(100);
        input.completion_tokens = Some(0);
        let (_, tps3) = compute(Some(crate::pricing::Price::default()), &input);
        assert_eq!(tps3, None);
    }

    #[test]
    fn test_redact_error_msg() {
        let raw_msg = "Error connecting to sk-1234567890abcdef and x-api-key: my-secret-key and Authorization: Bearer my-bearer-token.";
        let (for_db, redacted) = redact_error_msg(raw_msg);

        assert!(!for_db.contains("sk-1234567890abcdef"));
        assert!(for_db.contains("sk-[REDACTED]"));

        assert!(!for_db.contains("my-secret-key"));
        assert!(for_db.contains("x-api-key: [REDACTED]"));

        assert!(!for_db.contains("my-bearer-token"));
        assert!(for_db.contains("Authorization: Bearer [REDACTED]"));

        assert_eq!(for_db, redacted);

        // Test truncation
        let long_msg = "a".repeat(2100);
        let (for_db2, _) = redact_error_msg(&long_msg);
        assert!(for_db2.ends_with("...[truncated]"));
        assert!(for_db2.len() <= 2048 + 14); // 2048 + length of "...[truncated]"
    }

    #[test]
    fn test_record() {
        let dir = tempdir();
        let path = dir.join("test.db");
        let pool = DbPool::open(&path).expect("open pool");

        let mut conn = pool.writer();
        crate::migrations::run(&mut conn).expect("run migrations");

        let input = UsageInput {
            request_id: RequestId::new(),
            trace_id: "trace-123".to_string(),
            attempt: 1,
            provider_id: ProviderId::new("test-provider"),
            account_id: None,
            combo_id: None,
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id: "test-model".to_string(),
            prompt_tokens: Some(100),
            completion_tokens: Some(200),
            connect_ms: Some(10),
            ttft_ms: Some(100),
            total_ms: 1100,
            status_code: 200,
            error_msg: Some("test error sk-1234567890abcdef".to_string()),
            race_total: 1,
            race_lost: false,
            api_key_id: None,
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            error_message: None,
            race_attempts: 1,
            is_streaming: true,
            stream_complete: true,
            stop_reason: None,
            compression_savings_pct: None,
            compression_techniques: None,
            client_response: true,
            prompt_tokens_estimated: false,
            completion_tokens_estimated: false,
            endpoint_kind: EndpointKind::Chat,
            proxy_url: None,
            proxy_status: None,
            is_proxy_rotated: false,
        };

        let result = record(&conn, &input);
        assert!(result.is_ok(), "record failed: {:?}", result.err());

        let rowid = result.unwrap();

        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM usage WHERE id = ?1",
                rusqlite::params![rowid.0],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(count, 1);

        let error_msg_redacted: String = conn
            .query_row(
                "SELECT error_msg_redacted FROM usage WHERE id = ?1",
                rusqlite::params![rowid.0],
                |row| row.get(0),
            )
            .expect("query");
        assert_eq!(error_msg_redacted, "test error sk-[REDACTED]");
    }
}
