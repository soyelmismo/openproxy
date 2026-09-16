use crate::pricing;
use openproxy_types::ids::UsageId;
use openproxy_types::usage::{
    RecentUsageRow, USAGE_FLAG_CLIENT_RESPONSE, USAGE_FLAG_COMPLETION_ESTIMATED,
    USAGE_FLAG_IS_STREAMING, USAGE_FLAG_PROMPT_ESTIMATED, USAGE_FLAG_PROXY_ROTATED,
    USAGE_FLAG_RACE_LOST, USAGE_FLAG_STREAM_COMPLETE, UsageInput, publish_usage_row,
};
use rusqlite::{Connection, params};
use std::sync::LazyLock;

use crate::error::with_busy_retry;

pub fn compute(price: Option<pricing::Price>, input: &UsageInput) -> (Option<f64>, Option<f64>) {
    let cost = if input.status_code >= 200 && input.status_code < 400 {
        pricing::compute_cost_opt(
            price,
            input.prompt_tokens.unwrap_or(0),
            input.completion_tokens.unwrap_or(0),
        )
    } else {
        Some(0.0)
    };
    let tps = match (input.completion_tokens, input.ttft_ms) {
        (Some(c), Some(ttft)) if c > 0 && input.total_ms > ttft => {
            let denom = (input.total_ms - ttft) as f64;
            Some(f64::from(c) * 1000.0 / denom)
        }
        _ => None,
    };
    (cost, tps)
}

fn truncate_sanitized(sanitized: &str, max_len: usize) -> String {
    if sanitized.len() <= max_len {
        return sanitized.to_string();
    }
    let mut idx = max_len;
    while idx > 0 && !sanitized.is_char_boundary(idx) {
        idx -= 1;
    }
    let mut s = String::with_capacity(idx + 14);
    s.push_str(&sanitized[..idx]);
    s.push_str("...[truncated]");
    s
}

pub fn redact_error_msg(raw: &str) -> String {
    static RE_SK: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"sk-[A-Za-z0-9_\-]{10,}").expect("valid regex"));
    static RE_XAPIKEY: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)x-api-key:\s*\S+").expect("valid regex"));
    static RE_BEARER: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)Authorization:\s*Bearer\s+\S+").expect("valid regex")
    });

    let sanitized = RE_SK.replace_all(raw, "sk-[REDACTED]");
    let sanitized = RE_XAPIKEY.replace_all(&sanitized, "x-api-key: [REDACTED]");
    let sanitized = RE_BEARER.replace_all(&sanitized, "Authorization: Bearer [REDACTED]");

    truncate_sanitized(&sanitized, 2048)
}

fn prepare_usage_error_msg(error_msg: Option<&String>) -> Option<String> {
    error_msg.map(|msg| redact_error_msg(msg))
}

fn insert_usage_record(
    conn: &Connection,
    input: &UsageInput,
    cost_usd: Option<f64>,
    tps: Option<f64>,
    error_msg_for_db: Option<&str>,
    error_msg_redacted_for_db: Option<&str>,
) -> openproxy_types::Result<i64> {
    let request_id = input.request_id.to_string();

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
            endpoint_kind, proxy_url, proxy_status, is_proxy_rotated, cached_tokens, pii_redacted, \
            was_winner\
         ) VALUES (\
            ?1,  ?2,  ?3,  ?4,  ?5,  ?6,  ?7,  ?8,  ?9,  ?10, \
            ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, \
            ?21, ?22, ?23, datetime('now'), ?24, ?25, ?26, ?27, ?28, ?29, \
            ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43\
         )",
        params![
            request_id,
            input.trace_id.as_str(),
            i64::from(input.attempt),
            input.provider_id.as_str(),
            input.account_id.map(|a| a.0),
            input.combo_id.map(|c| c.0),
            input.model_row_id.map(|m| m.0),
            input.upstream_model_id,
            input.combo_target_id.map(|c| c.0),
            input.prompt_tokens.map(i64::from),
            input.completion_tokens.map(i64::from),
            cost_usd,
            input.connect_ms.map(|c| c as i64),
            input.ttft_ms.map(|t| t as i64),
            input.total_ms as i64,
            tps,
            i64::from(input.status_code),
            error_msg_for_db,
            error_msg_redacted_for_db,
            i64::from(input.race_total),
            i64::from(input.race_attempts),
            i64::from(input.has_flag(USAGE_FLAG_RACE_LOST)),
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
                .and_then(|h| { serde_json::to_string(h).ok() }),
            input
                .response_headers
                .as_ref()
                .and_then(|h| { serde_json::to_string(h).ok() }),
            input.error_message,
            i64::from(input.has_flag(USAGE_FLAG_IS_STREAMING)),
            i64::from(input.has_flag(USAGE_FLAG_STREAM_COMPLETE)),
            input.stop_reason,
            input.compression_savings_pct,
            input.compression_techniques,
            i64::from(input.has_flag(USAGE_FLAG_CLIENT_RESPONSE)),
            i64::from(input.has_flag(USAGE_FLAG_PROMPT_ESTIMATED)),
            i64::from(input.has_flag(USAGE_FLAG_COMPLETION_ESTIMATED)),
            input.endpoint_kind.as_str(),
            input.proxy_url,
            input.proxy_status,
            i64::from(input.has_flag(USAGE_FLAG_PROXY_ROTATED)),
            input.cached_tokens.map(i64::from),
            input.pii_redacted,
            i64::from(input.has_flag(USAGE_FLAG_CLIENT_RESPONSE)),
        ],
    )
    .map_err(crate::error::map_db_error)?;

    Ok(conn.last_insert_rowid())
}

pub fn record(conn: &Connection, input: &UsageInput) -> openproxy_types::Result<UsageId> {
    let price = pricing::lookup_with_db(conn, input.provider_id.as_str(), &input.upstream_model_id);
    if price.is_none()
        && (input.prompt_tokens.unwrap_or(0) > 0 || input.completion_tokens.unwrap_or(0) > 0)
    {
        tracing::warn!(
            provider_id = %input.provider_id,
            upstream_model_id = %input.upstream_model_id,
            "no pricing data found; recording cost_usd = NULL (run models.dev sync or set pricing manually)"
        );
    }
    let (cost_usd, tps) = compute(price, input);
    let error_msg_for_db = prepare_usage_error_msg(input.error_msg.as_ref());

    let rowid = insert_usage_record(
        conn,
        input,
        cost_usd,
        tps,
        error_msg_for_db.as_deref(),
        error_msg_for_db.as_deref(),
    )?;

    let row = RecentUsageRow {
        id: UsageId(rowid),
        request_id: input.request_id.to_string(),
        trace_id: input.trace_id.clone(),
        provider_id: input.provider_id.clone(),
        upstream_model_id: input.upstream_model_id.clone(),
        status_code: input.status_code,
        total_ms: input.total_ms,
        prompt_tokens: input.prompt_tokens,
        completion_tokens: input.completion_tokens,
        cached_tokens: input.cached_tokens,
        cost_usd,
        created_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        connect_ms: input.connect_ms,
        ttft_ms: input.ttft_ms,
        request_body_json: None,
        response_body_json: None,
        request_headers: None,
        response_headers: None,
        error_message: error_msg_for_db,
        race_total: Some(input.race_total),
        race_attempts: Some(input.race_attempts),
        stop_reason: input.stop_reason.as_deref().map(str::to_owned),
        compression_savings_pct: input.compression_savings_pct,
        compression_techniques: input.compression_techniques.as_deref().map(str::to_owned),
        pii_redacted: input.pii_redacted.clone(),
        proxy_url: input.proxy_url.as_deref().map(str::to_owned),
        proxy_status: input.proxy_status.as_deref().map(str::to_owned),
        flags: input.flags,
        endpoint_kind: input.endpoint_kind,
    };
    publish_usage_row(row);

    Ok(UsageId(rowid))
}

/// Hot-path wrapper around [`record`] that retries on transient
/// `SQLITE_BUSY` from other writers (vacuum, backfill, concurrent
/// inserts from a different process).
///
/// The hot-path callers all guard the writer with
/// `try_writer_for(100ms)` to avoid starving chat requests behind a
/// long admin query. Once that lock is held, the actual `INSERT` can
/// still surface `SQLITE_BUSY` if the SQLite engine itself can't
/// acquire the file-level write lock in time (e.g. another process
/// is running vacuum). Without this wrapper the row is silently
/// dropped, which loses a usage record. With it, we retry with
/// 50ms+100ms backoff (150ms total ceiling), still well under the
/// lock-hold window, and convert a transient `SQLITE_BUSY` into a
/// successful insert.
pub fn record_with_retry(
    conn: &Connection,
    input: &UsageInput,
) -> openproxy_types::Result<UsageId> {
    with_busy_retry("cost::record", || record(conn, input))
}

fn update_backfill_price(
    conn: &Connection,
    provider_id: &str,
    upstream_model_id: &str,
    price: Option<pricing::Price>,
) -> openproxy_types::Result<usize> {
    match price {
        Some(p) => conn
            .execute(
                "UPDATE usage \
                 SET cost_usd = (COALESCE(?1, 0.0) * prompt_tokens / 1000000.0) + \
                                (COALESCE(?2, 0.0) * COALESCE(completion_tokens, 0) / 1000000.0) \
                 WHERE provider_id = ?3 AND upstream_model_id = ?4 \
                   AND prompt_tokens > 0 AND (cost_usd = 0.0 OR cost_usd IS NULL)",
                params![
                    p.input_per_1m,
                    p.output_per_1m,
                    provider_id,
                    upstream_model_id
                ],
            )
            .map_err(crate::error::map_db_error),
        None => conn
            .execute(
                "UPDATE usage \
                 SET cost_usd = NULL \
                 WHERE provider_id = ?1 AND upstream_model_id = ?2 \
                   AND prompt_tokens > 0 AND cost_usd = 0.0",
                params![provider_id, upstream_model_id],
            )
            .map_err(crate::error::map_db_error),
    }
}

pub fn backfill_usage_pricing(conn: &Connection) -> openproxy_types::Result<usize> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT provider_id, upstream_model_id \
             FROM usage \
             WHERE prompt_tokens > 0 AND (cost_usd = 0.0 OR cost_usd IS NULL)",
        )
        .map_err(crate::error::map_db_error)?;

    let pairs: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(crate::error::map_db_error)?
        .filter_map(std::result::Result::ok)
        .collect();

    let mut total_updated = 0;
    for (provider_id, upstream_model_id) in pairs {
        let price = pricing::lookup_with_db(conn, &provider_id, &upstream_model_id);
        total_updated += update_backfill_price(conn, &provider_id, &upstream_model_id, price)?;
    }
    Ok(total_updated)
}

fn map_recent_usage_from_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RecentUsageRow> {
    let id: i64 = r.get(0)?;
    let req_id: String = r.get(1)?;
    let trace_id: String = r.get(2)?;
    let provider_id_str: String = r.get(3)?;
    let upstream_model_id: String = r.get(4)?;
    let status_code: u16 = r.get(5)?;
    let total_ms: i64 = r.get(6)?;
    let prompt_tokens: Option<u32> = r.get(7)?;
    let completion_tokens: Option<u32> = r.get(8)?;
    let cost_usd: Option<f64> = r.get(9)?;
    let connect_ms: Option<i64> = r.get(10)?;
    let ttft_ms: Option<i64> = r.get(11)?;
    let error_message: Option<String> = r.get(12)?;
    let race_total: Option<u8> = r.get(13)?;
    let race_attempts: Option<u8> = r.get(14)?;
    let is_streaming: i64 = r.get(15)?;
    let stream_complete: i64 = r.get(16)?;
    let race_lost: i64 = r.get(17)?;
    let created_at: String = r.get(18)?;
    let stop_reason: Option<String> = r.get(19)?;
    let compression_savings_pct: Option<f64> = r.get(20)?;
    let compression_techniques: Option<String> = r.get(21)?;
    let client_response: i64 = r.get(22)?;
    let prompt_tokens_estimated: i64 = r.get(23)?;
    let completion_tokens_estimated: i64 = r.get(24)?;
    let endpoint_kind_str: String = r.get(25)?;
    let proxy_url: Option<String> = r.get(26)?;
    let proxy_status: Option<String> = r.get(27)?;
    let is_proxy_rotated: i64 = r.get(28)?;
    let cached_tokens: Option<u32> = r.get(29)?;
    let pii_redacted: Option<String> = r.get(30)?;

    let flags = ((race_lost != 0) as u8 * USAGE_FLAG_RACE_LOST)
        | ((is_streaming != 0) as u8 * USAGE_FLAG_IS_STREAMING)
        | ((stream_complete != 0) as u8 * USAGE_FLAG_STREAM_COMPLETE)
        | ((client_response != 0) as u8 * USAGE_FLAG_CLIENT_RESPONSE)
        | ((prompt_tokens_estimated != 0) as u8 * USAGE_FLAG_PROMPT_ESTIMATED)
        | ((completion_tokens_estimated != 0) as u8 * USAGE_FLAG_COMPLETION_ESTIMATED)
        | ((is_proxy_rotated != 0) as u8 * USAGE_FLAG_PROXY_ROTATED);

    Ok(RecentUsageRow {
        id: UsageId(id),
        request_id: req_id,
        trace_id,
        provider_id: openproxy_types::ids::ProviderId::new(provider_id_str),
        upstream_model_id,
        status_code,
        total_ms: total_ms.max(0) as u64,
        prompt_tokens,
        completion_tokens,
        cached_tokens,
        cost_usd,
        created_at,
        connect_ms: connect_ms.map(|c| c.max(0) as u64),
        ttft_ms: ttft_ms.map(|t| t.max(0) as u64),
        request_body_json: None,
        response_body_json: None,
        request_headers: None,
        response_headers: None,
        error_message,
        race_total,
        race_attempts,
        stop_reason,
        compression_savings_pct,
        compression_techniques,
        pii_redacted,
        proxy_url,
        proxy_status,
        flags,
        endpoint_kind: endpoint_kind_str.parse().unwrap_or_default(),
    })
}

const RECENT_USAGE_COLS: &str = "id, request_id, trace_id, provider_id, upstream_model_id, \
    status_code, total_ms, prompt_tokens, completion_tokens, cost_usd, connect_ms, ttft_ms, \
    error_msg, race_total, race_attempts, is_streaming, stream_complete, race_lost, created_at, \
    stop_reason, compression_savings_pct, compression_techniques, client_response, \
    prompt_tokens_estimated, completion_tokens_estimated, endpoint_kind, proxy_url, \
    proxy_status, is_proxy_rotated, cached_tokens, pii_redacted";

pub fn mark_client_response(conn: &Connection, row_id: UsageId) -> openproxy_types::Result<()> {
    let affected = conn
        .execute(
            "UPDATE usage SET was_winner = 1, client_response = 1 WHERE id = ?1",
            params![row_id.0],
        )
        .map_err(crate::error::map_db_error_ctx("mark_client_response"))?;

    if affected > 0
        && let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT {RECENT_USAGE_COLS} FROM usage WHERE id = ?1"
        ))
        && let Ok(row) = stmt.query_row(params![row_id.0], map_recent_usage_from_row)
    {
        publish_usage_row(row);
    }
    Ok(())
}

pub fn mark_winner_usage_row(
    conn: &Connection,
    request_id: &str,
    attempt: u8,
    target_id: openproxy_types::ids::ComboTargetId,
) -> openproxy_types::Result<()> {
    let affected = conn
        .execute(
            "UPDATE usage SET was_winner = 1, client_response = 1 WHERE request_id = ?1 AND attempt = ?2 AND combo_target_id = ?3",
            params![request_id, attempt, target_id.0],
        )
        .map_err(crate::error::map_db_error_ctx("mark_winner_usage_row"))?;

    if affected > 0
        && let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT {RECENT_USAGE_COLS} FROM usage WHERE request_id = ?1 AND attempt = ?2 AND combo_target_id = ?3 ORDER BY id DESC LIMIT 1"
        ))
        && let Ok(row) = stmt.query_row(params![request_id, attempt, target_id.0], map_recent_usage_from_row)
    {
        publish_usage_row(row);
    }
    Ok(())
}

pub fn record_no_healthy_targets_row(
    conn: &Connection,
    request_id: &str,
    trace_id: &str,
    combo_id: openproxy_types::ids::ComboId,
    elapsed: u64,
    created_str: &str,
    error_msg: &str,
) -> openproxy_types::Result<()> {
    conn.execute(
        "INSERT INTO usage(request_id, trace_id, combo_id, total_ms, created_at, status_code, error_msg, error_message, was_winner, client_response, prompt_tokens, completion_tokens, provider_id, upstream_model_id, attempt, race_total, race_lost) \
         VALUES (?1, ?2, ?3, ?4, ?5, 502, ?6, ?6, 1, 0, 0, 0, 'virtual', 'none', 1, 1, 0)",
        params![request_id, trace_id, combo_id.0, elapsed as i64, created_str, error_msg],
    )
    .map(|_| ())
    .map_err(crate::error::map_db_error_ctx("insert no_healthy_targets usage"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::DbPool;
    use openproxy_types::endpoint::EndpointKind;
    use openproxy_types::ids::{ProviderId, RequestId};
    use openproxy_types::usage::UsageInput;

    fn test_input(
        prompt: u32,
        completion: u32,
        ttft: Option<u64>,
        total: u64,
        flags: u8,
    ) -> UsageInput {
        UsageInput {
            request_id: RequestId::new(),
            trace_id: "trace-test".to_string(),
            attempt: 1,
            provider_id: ProviderId::new("test"),
            account_id: None,
            combo_id: None,
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id: "test-model".to_string(),
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            connect_ms: None,
            ttft_ms: ttft,
            total_ms: total,
            status_code: 200,
            error_msg: None,
            race_total: 1,
            api_key_id: None,
            request_body_json: None,
            response_body_json: None,
            request_headers: None,
            response_headers: None,
            error_message: None,
            race_attempts: 1,
            stop_reason: None,
            compression_savings_pct: None,
            compression_techniques: None,
            pii_redacted: None,
            endpoint_kind: EndpointKind::Chat,
            proxy_url: None,
            proxy_status: None,
            cached_tokens: None,
            flags,
        }
    }

    #[test]
    fn test_compute_cost_and_tps() {
        let price = crate::pricing::Price {
            input_per_1m: 1.0,
            output_per_1m: 2.0,
            kind: crate::pricing::PriceKind::Chat,
        };
        let mut input = test_input(
            100,
            200,
            Some(100),
            1100,
            USAGE_FLAG_IS_STREAMING | USAGE_FLAG_STREAM_COMPLETE | USAGE_FLAG_CLIENT_RESPONSE,
        );
        let (cost, tps) = compute(Some(price), &input);
        assert_eq!(cost, Some(0.0005));
        assert_eq!(tps, Some(200.0));

        input.ttft_ms = None;
        assert_eq!(
            compute(Some(crate::pricing::Price::default()), &input).1,
            None
        );

        input.ttft_ms = Some(100);
        input.completion_tokens = Some(0);
        assert_eq!(
            compute(Some(crate::pricing::Price::default()), &input).1,
            None
        );
    }

    #[test]
    fn test_redact_error_msg() {
        let raw_msg = "Error connecting to sk-1234567890abcdef and x-api-key: my-secret-key and Authorization: Bearer my-bearer-token.";
        let redacted = redact_error_msg(raw_msg);
        assert!(!redacted.contains("sk-1234567890abcdef") && redacted.contains("sk-[REDACTED]"));
        assert!(!redacted.contains("my-secret-key") && redacted.contains("x-api-key: [REDACTED]"));
        assert!(
            !redacted.contains("my-bearer-token")
                && redacted.contains("Authorization: Bearer [REDACTED]")
        );
        let long_msg = "a".repeat(2100);
        let redacted2 = redact_error_msg(&long_msg);
        assert!(redacted2.ends_with("...[truncated]") && redacted2.len() <= 2048 + 14);
    }

    #[test]
    fn test_record() {
        let pool = DbPool::test_pool_with_prefix("openproxy-cost-test").expect("open pool");
        let conn = pool.writer();
        let mut input = test_input(
            100,
            200,
            Some(100),
            1100,
            USAGE_FLAG_IS_STREAMING | USAGE_FLAG_STREAM_COMPLETE | USAGE_FLAG_CLIENT_RESPONSE,
        );
        input.connect_ms = Some(10);
        input.error_msg = Some("test error sk-1234567890abcdef".to_string());
        let rowid = record(&conn, &input).expect("record failed");
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM usage WHERE id = ?1",
                rusqlite::params![rowid.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        let error_msg_redacted: String = conn
            .query_row(
                "SELECT error_msg_redacted FROM usage WHERE id = ?1",
                rusqlite::params![rowid.0],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(error_msg_redacted, "test error sk-[REDACTED]");
    }

    #[test]
    fn record_with_retry_succeeds_first_attempt_on_quiescent_db() {
        let pool = DbPool::test_pool_with_prefix("openproxy-cost-retry").expect("open pool");
        let input = test_input(10, 20, None, 100, USAGE_FLAG_CLIENT_RESPONSE);
        assert!(record_with_retry(&pool.writer(), &input).is_ok());
    }

    #[test]
    fn test_client_response_and_winner_lifecycle() {
        let pool = DbPool::test_pool_with_prefix("openproxy-cost-client-resp").expect("open pool");
        let conn = pool.writer();
        let target_id = openproxy_types::ids::ComboTargetId(42);

        let mut input_winner = test_input(10, 20, None, 100, USAGE_FLAG_CLIENT_RESPONSE);
        input_winner.combo_target_id = Some(target_id);
        let rowid1 = record(&conn, &input_winner).expect("insert winner");
        let (cr1, ww1): (i64, i64) = conn
            .query_row(
                "SELECT client_response, was_winner FROM usage WHERE id = ?1",
                rusqlite::params![rowid1.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cr1, 1);
        assert_eq!(ww1, 1);

        let mut input_fail = input_winner.clone();
        input_fail.request_id = RequestId::new();
        input_fail.flags = 0;
        let rowid2 = record(&conn, &input_fail).expect("insert non-winner");
        let (cr2, ww2): (i64, i64) = conn
            .query_row(
                "SELECT client_response, was_winner FROM usage WHERE id = ?1",
                rusqlite::params![rowid2.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cr2, 0);
        assert_eq!(ww2, 0);

        mark_winner_usage_row(&conn, &input_fail.request_id.to_string(), 1, target_id)
            .expect("mark_winner_usage_row");
        let (cr2_after, ww2_after): (i64, i64) = conn
            .query_row(
                "SELECT client_response, was_winner FROM usage WHERE id = ?1",
                rusqlite::params![rowid2.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cr2_after, 1);
        assert_eq!(ww2_after, 1);

        let mut input_3 = input_winner;
        input_3.request_id = RequestId::new();
        input_3.flags = 0;
        let rowid3 = record(&conn, &input_3).expect("insert row 3");
        mark_client_response(&conn, rowid3).expect("mark_client_response");
        let (cr3, ww3): (i64, i64) = conn
            .query_row(
                "SELECT client_response, was_winner FROM usage WHERE id = ?1",
                rusqlite::params![rowid3.0],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(cr3, 1);
        assert_eq!(ww3, 1);
    }
}
