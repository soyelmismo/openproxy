//! Recent usage queries and mapper for live feed and long polling.

use openproxy_types::Result;
use openproxy_types::ids::{ProviderId, UsageId};
use openproxy_types::usage::{
    RecentUsageRow, USAGE_FLAG_CLIENT_RESPONSE, USAGE_FLAG_COMPLETION_ESTIMATED,
    USAGE_FLAG_IS_STREAMING, USAGE_FLAG_PROMPT_ESTIMATED, USAGE_FLAG_PROXY_ROTATED,
    USAGE_FLAG_RACE_LOST, USAGE_FLAG_STREAM_COMPLETE,
};
use rusqlite::{Connection, Row, params};

use super::filter::collect_rows;

#[derive(Debug)]
struct SimpleErr(String);
impl std::fmt::Display for SimpleErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SimpleErr {}

/// Map a rusqlite Row to `RecentUsageRow`. Reads columns sequentially from
/// col_idx 0. Used by `recent()`, `recent_desc()`, and `row_for_broadcast_by_id()`.
pub(crate) fn map_usage_row(row: &Row<'_>) -> rusqlite::Result<RecentUsageRow> {
    let mut col_idx = 0;
    let id: i64 = row.get(col_idx)?;
    col_idx += 1;
    let request_id: String = row.get(col_idx)?;
    col_idx += 1;
    let trace_id: String = row.get(col_idx)?;
    col_idx += 1;
    let provider_id: String = row.get(col_idx)?;
    col_idx += 1;
    let upstream_model_id: String = row.get(col_idx)?;
    col_idx += 1;
    let status_code: i64 = row.get(col_idx)?;
    col_idx += 1;
    let total_ms: i64 = row.get(col_idx)?;
    col_idx += 1;
    let prompt_tokens: Option<i64> = row.get(col_idx)?;
    col_idx += 1;
    let completion_tokens: Option<i64> = row.get(col_idx)?;
    col_idx += 1;
    let cost_usd: Option<f64> = row.get(col_idx)?;
    col_idx += 1;
    let connect_ms: Option<i64> = row.get(col_idx)?;
    col_idx += 1;
    let ttft_ms: Option<i64> = row.get(col_idx)?;
    col_idx += 1;
    let request_body_json: Option<serde_json::Value> = row
        .get::<_, Option<String>>(col_idx)?
        .and_then(|s| serde_json::from_str(&s).ok());
    col_idx += 1;
    let response_body_json: Option<serde_json::Value> = row
        .get::<_, Option<String>>(col_idx)?
        .and_then(|s| serde_json::from_str(&s).ok());
    col_idx += 1;
    let request_headers: Option<String> = row.get(col_idx)?;
    col_idx += 1;
    let response_headers: Option<String> = row.get(col_idx)?;
    col_idx += 1;
    let error_msg_redacted: Option<String> = row.get(col_idx)?;
    col_idx += 1;
    let error_msg: Option<String> = row.get(col_idx)?;
    col_idx += 1;
    let race_total: i64 = row.get(col_idx)?;
    col_idx += 1;
    let race_attempts: i64 = row.get(col_idx)?;
    col_idx += 1;
    let is_streaming: i64 = row.get(col_idx)?;
    col_idx += 1;
    let stream_complete: i64 = row.get(col_idx)?;
    col_idx += 1;
    let race_lost: i64 = row.get(col_idx)?;
    col_idx += 1;
    let created_at: String = row.get(col_idx)?;
    col_idx += 1;
    let stop_reason: Option<String> = row.get(col_idx)?;
    col_idx += 1;
    let compression_savings_pct: Option<f64> = row.get(col_idx)?;
    col_idx += 1;
    let compression_techniques: Option<String> = row.get(col_idx)?;
    col_idx += 1;
    let client_response: i64 = row.get(col_idx)?;
    col_idx += 1;
    let prompt_tokens_estimated: i64 = row.get(col_idx)?;
    col_idx += 1;
    let completion_tokens_estimated: i64 = row.get(col_idx)?;
    col_idx += 1;
    let endpoint_kind_str: String = row.get(col_idx)?;
    col_idx += 1;
    let proxy_url: Option<String> = row.get(col_idx)?;
    col_idx += 1;
    let proxy_status: Option<String> = row.get(col_idx)?;
    col_idx += 1;
    let is_proxy_rotated: i64 = row.get(col_idx)?;
    col_idx += 1;
    let cached_tokens: Option<i64> = row.get(col_idx)?;
    col_idx += 1;
    let pii_redacted: Option<String> = row.get(col_idx)?;

    if !(0..=i64::from(u16::MAX)).contains(&status_code) {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Integer,
            Box::new(SimpleErr(format!(
                "status_code out of u16 range: {status_code}"
            ))),
        ));
    }
    if total_ms < 0 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            6,
            rusqlite::types::Type::Integer,
            Box::new(SimpleErr(format!(
                "total_ms unexpectedly negative: {total_ms}"
            ))),
        ));
    }
    let request_headers = request_headers.and_then(|s| serde_json::from_str(&s).ok());
    let response_headers = response_headers.and_then(|s| serde_json::from_str(&s).ok());
    let error_message = error_msg_redacted.or(error_msg);
    let prompt_tokens = prompt_tokens.and_then(|v| u32::try_from(v).ok());
    let completion_tokens = completion_tokens.and_then(|v| u32::try_from(v).ok());
    let race_total_u8 = u8::try_from(race_total).ok();
    let race_attempts_u8 = u8::try_from(race_attempts).ok();
    let mut flags = 0u8;
    if race_lost != 0 {
        flags |= USAGE_FLAG_RACE_LOST;
    }
    if is_streaming != 0 {
        flags |= USAGE_FLAG_IS_STREAMING;
    }
    if stream_complete != 0 {
        flags |= USAGE_FLAG_STREAM_COMPLETE;
    }
    if client_response != 0 {
        flags |= USAGE_FLAG_CLIENT_RESPONSE;
    }
    if prompt_tokens_estimated != 0 {
        flags |= USAGE_FLAG_PROMPT_ESTIMATED;
    }
    if completion_tokens_estimated != 0 {
        flags |= USAGE_FLAG_COMPLETION_ESTIMATED;
    }
    if is_proxy_rotated != 0 {
        flags |= USAGE_FLAG_PROXY_ROTATED;
    }
    let endpoint_kind = endpoint_kind_str.parse().unwrap_or_default();
    let cached_tokens = cached_tokens.and_then(|v| u32::try_from(v).ok());
    Ok(RecentUsageRow {
        id: UsageId(id),
        request_id,
        trace_id,
        provider_id: ProviderId::new(provider_id),
        upstream_model_id,
        status_code: status_code as u16,
        total_ms: total_ms as u64,
        prompt_tokens,
        completion_tokens,
        cached_tokens,
        cost_usd,
        connect_ms: connect_ms.map(|v| v as u64),
        ttft_ms: ttft_ms.map(|v| v as u64),
        request_body_json,
        response_body_json,
        request_headers,
        response_headers,
        error_message,
        race_total: race_total_u8,
        race_attempts: race_attempts_u8,
        stop_reason,
        compression_savings_pct,
        compression_techniques,
        pii_redacted,
        proxy_url,
        proxy_status,
        endpoint_kind,
        created_at,
        flags,
    })
}

pub fn recent(conn: &Connection, since_id: i64, limit: u32) -> Result<Vec<RecentUsageRow>> {
    let limit_param: i64 = i64::from(limit);
    let mut stmt = conn
        .prepare(
            "SELECT id, request_id, trace_id, provider_id, upstream_model_id, \
                    status_code, total_ms, prompt_tokens, completion_tokens, \
                    cost_usd, connect_ms, ttft_ms, request_body_json, response_body_json, \
                    request_headers, response_headers, error_msg_redacted, error_msg, \
                    race_total, race_attempts, is_streaming, stream_complete, \
                    race_lost, created_at, stop_reason, \
                    compression_savings_pct, compression_techniques, \
                    client_response, prompt_tokens_estimated, completion_tokens_estimated, \
                    endpoint_kind, proxy_url, proxy_status, is_proxy_rotated, cached_tokens, pii_redacted \
             FROM usage \
             WHERE id > ?1 \
             ORDER BY id ASC \
             LIMIT ?2",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let rows = stmt
        .query_map(params![since_id, limit_param], map_usage_row)
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "recent")
}

pub fn recent_desc(conn: &Connection, limit: u32) -> Result<Vec<RecentUsageRow>> {
    let limit_param: i64 = i64::from(limit);
    let mut stmt = conn
        .prepare(
            "SELECT id, request_id, trace_id, provider_id, upstream_model_id, \
                    status_code, total_ms, prompt_tokens, completion_tokens, \
                    cost_usd, connect_ms, ttft_ms, request_body_json, response_body_json, \
                    request_headers, response_headers, error_msg_redacted, error_msg, \
                    race_total, race_attempts, is_streaming, stream_complete, \
                    race_lost, created_at, stop_reason, \
                    compression_savings_pct, compression_techniques, \
                    client_response, prompt_tokens_estimated, completion_tokens_estimated, \
                    endpoint_kind, proxy_url, proxy_status, is_proxy_rotated, cached_tokens, pii_redacted \
             FROM usage \
             ORDER BY id DESC \
             LIMIT ?1",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let rows = stmt
        .query_map(params![limit_param], map_usage_row)
        .map_err(openproxy_db::error::map_db_error)?;

    collect_rows(rows, "recent_desc")
}

pub fn row_for_broadcast_by_id(conn: &Connection, id: i64) -> Result<Option<RecentUsageRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, request_id, trace_id, provider_id, upstream_model_id, \
                    status_code, total_ms, prompt_tokens, completion_tokens, \
                    cost_usd, connect_ms, ttft_ms, request_body_json, response_body_json, \
                    request_headers, response_headers, error_msg_redacted, error_msg, \
                    race_total, race_attempts, is_streaming, stream_complete, \
                    race_lost, created_at, stop_reason, \
                    compression_savings_pct, compression_techniques, \
                    client_response, prompt_tokens_estimated, completion_tokens_estimated, \
                    endpoint_kind, proxy_url, proxy_status, is_proxy_rotated, cached_tokens, pii_redacted \
             FROM usage \
             WHERE id = ?1",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let mut rows = stmt
        .query_map(params![id], map_usage_row)
        .map_err(openproxy_db::error::map_db_error)?;

    match rows.next() {
        Some(Ok(row)) => Ok(Some(row)),
        Some(Err(e)) => Err(openproxy_db::error::map_db_error(e)),
        None => Ok(None),
    }
}
