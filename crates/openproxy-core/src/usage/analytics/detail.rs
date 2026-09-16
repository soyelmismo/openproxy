//! Detail view for individual usage rows and retention pruning.

use std::collections::BTreeMap;

use openproxy_types::Result;
use openproxy_types::endpoint::EndpointKind;
use openproxy_types::ids::{
    AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, UsageId,
};
use openproxy_types::usage::{
    USAGE_FLAG_CLIENT_RESPONSE, USAGE_FLAG_COMPLETION_ESTIMATED, USAGE_FLAG_IS_STREAMING,
    USAGE_FLAG_PROMPT_ESTIMATED, USAGE_FLAG_PROXY_ROTATED, USAGE_FLAG_RACE_LOST,
    USAGE_FLAG_STREAM_COMPLETE,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug)]
struct SimpleErr(String);
impl std::fmt::Display for SimpleErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for SimpleErr {}

/// Full `usage` row projection for live-log detail views.
#[derive(Debug, Clone)]
pub struct UsageDetailRow {
    pub id: UsageId,
    pub request_id: String,
    pub trace_id: String,
    pub attempt: i64,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub combo_id: Option<ComboId>,
    pub combo_target_id: Option<ComboTargetId>,
    pub model_row_id: Option<ModelRowId>,
    pub upstream_model_id: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub connect_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub total_ms: i64,
    pub tokens_per_sec: Option<f64>,
    pub status_code: u16,
    pub error_msg: Option<String>,
    pub error_msg_redacted: Option<String>,
    pub request_body_json: Option<Value>,
    pub response_body_json: Option<Value>,
    pub request_headers: Option<BTreeMap<String, String>>,
    pub response_headers: Option<BTreeMap<String, String>>,
    pub error_message: Option<String>,
    pub race_total: i64,
    pub race_attempts: i64,
    pub api_key_id: Option<ApiKeyId>,
    pub proxy_url: Option<String>,
    pub proxy_status: Option<String>,
    pub endpoint_kind: EndpointKind,
    pub pii_redacted: Option<String>,
    pub created_at: String,
    pub flags: u8,
}

impl UsageDetailRow {
    #[inline]
    pub fn has_flag(&self, flag: u8) -> bool {
        self.flags & flag != 0
    }

    #[inline]
    pub fn set_flag(&mut self, flag: u8) {
        self.flags |= flag;
    }
}

#[derive(Serialize, Deserialize)]
struct UsageDetailRowSerde {
    pub id: UsageId,
    pub request_id: String,
    pub trace_id: String,
    pub attempt: i64,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub combo_id: Option<ComboId>,
    pub combo_target_id: Option<ComboTargetId>,
    pub model_row_id: Option<ModelRowId>,
    pub upstream_model_id: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub connect_ms: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub total_ms: i64,
    pub tokens_per_sec: Option<f64>,
    pub status_code: u16,
    pub error_msg: Option<String>,
    pub error_msg_redacted: Option<String>,
    pub request_body_json: Option<Value>,
    pub response_body_json: Option<Value>,
    pub request_headers: Option<BTreeMap<String, String>>,
    pub response_headers: Option<BTreeMap<String, String>>,
    pub error_message: Option<String>,
    pub race_total: i64,
    pub race_attempts: i64,
    pub race_lost: bool,
    pub is_streaming: bool,
    pub stream_complete: bool,
    pub api_key_id: Option<ApiKeyId>,
    pub client_response: bool,
    pub prompt_tokens_estimated: bool,
    pub completion_tokens_estimated: bool,
    pub proxy_url: Option<String>,
    pub proxy_status: Option<String>,
    pub is_proxy_rotated: bool,
    pub endpoint_kind: EndpointKind,
    pub pii_redacted: Option<String>,
    pub created_at: String,
}

impl Serialize for UsageDetailRow {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let shadow = UsageDetailRowSerde {
            id: self.id,
            request_id: self.request_id.clone(),
            trace_id: self.trace_id.clone(),
            attempt: self.attempt,
            provider_id: self.provider_id.clone(),
            account_id: self.account_id,
            combo_id: self.combo_id,
            combo_target_id: self.combo_target_id,
            model_row_id: self.model_row_id,
            upstream_model_id: self.upstream_model_id.clone(),
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            connect_ms: self.connect_ms,
            ttft_ms: self.ttft_ms,
            total_ms: self.total_ms,
            tokens_per_sec: self.tokens_per_sec,
            status_code: self.status_code,
            error_msg: self.error_msg.clone(),
            error_msg_redacted: self.error_msg_redacted.clone(),
            request_body_json: self.request_body_json.clone(),
            response_body_json: self.response_body_json.clone(),
            request_headers: self.request_headers.clone(),
            response_headers: self.response_headers.clone(),
            error_message: self.error_message.clone(),
            race_total: self.race_total,
            race_attempts: self.race_attempts,
            race_lost: self.has_flag(USAGE_FLAG_RACE_LOST),
            is_streaming: self.has_flag(USAGE_FLAG_IS_STREAMING),
            stream_complete: self.has_flag(USAGE_FLAG_STREAM_COMPLETE),
            api_key_id: self.api_key_id,
            client_response: self.has_flag(USAGE_FLAG_CLIENT_RESPONSE),
            prompt_tokens_estimated: self.has_flag(USAGE_FLAG_PROMPT_ESTIMATED),
            completion_tokens_estimated: self.has_flag(USAGE_FLAG_COMPLETION_ESTIMATED),
            proxy_url: self.proxy_url.clone(),
            proxy_status: self.proxy_status.clone(),
            is_proxy_rotated: self.has_flag(USAGE_FLAG_PROXY_ROTATED),
            endpoint_kind: self.endpoint_kind,
            pii_redacted: self.pii_redacted.clone(),
            created_at: self.created_at.clone(),
        };
        shadow.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for UsageDetailRow {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let shadow = UsageDetailRowSerde::deserialize(deserializer)?;
        let mut flags = 0u8;
        if shadow.race_lost {
            flags |= USAGE_FLAG_RACE_LOST;
        }
        if shadow.is_streaming {
            flags |= USAGE_FLAG_IS_STREAMING;
        }
        if shadow.stream_complete {
            flags |= USAGE_FLAG_STREAM_COMPLETE;
        }
        if shadow.client_response {
            flags |= USAGE_FLAG_CLIENT_RESPONSE;
        }
        if shadow.prompt_tokens_estimated {
            flags |= USAGE_FLAG_PROMPT_ESTIMATED;
        }
        if shadow.completion_tokens_estimated {
            flags |= USAGE_FLAG_COMPLETION_ESTIMATED;
        }
        if shadow.is_proxy_rotated {
            flags |= USAGE_FLAG_PROXY_ROTATED;
        }
        Ok(UsageDetailRow {
            id: shadow.id,
            request_id: shadow.request_id,
            trace_id: shadow.trace_id,
            attempt: shadow.attempt,
            provider_id: shadow.provider_id,
            account_id: shadow.account_id,
            combo_id: shadow.combo_id,
            combo_target_id: shadow.combo_target_id,
            model_row_id: shadow.model_row_id,
            upstream_model_id: shadow.upstream_model_id,
            prompt_tokens: shadow.prompt_tokens,
            completion_tokens: shadow.completion_tokens,
            connect_ms: shadow.connect_ms,
            ttft_ms: shadow.ttft_ms,
            total_ms: shadow.total_ms,
            tokens_per_sec: shadow.tokens_per_sec,
            status_code: shadow.status_code,
            error_msg: shadow.error_msg,
            error_msg_redacted: shadow.error_msg_redacted,
            request_body_json: shadow.request_body_json,
            response_body_json: shadow.response_body_json,
            request_headers: shadow.request_headers,
            response_headers: shadow.response_headers,
            error_message: shadow.error_message,
            race_total: shadow.race_total,
            race_attempts: shadow.race_attempts,
            api_key_id: shadow.api_key_id,
            proxy_url: shadow.proxy_url,
            proxy_status: shadow.proxy_status,
            endpoint_kind: shadow.endpoint_kind,
            pii_redacted: shadow.pii_redacted,
            created_at: shadow.created_at,
            flags,
        })
    }
}

fn parse_json_from_row<T: serde::de::DeserializeOwned>(raw: Option<String>) -> Option<T> {
    raw.and_then(|s| serde_json::from_str(&s).ok())
}

fn row_to_usage_detail(row: &Row<'_>) -> rusqlite::Result<UsageDetailRow> {
    let id: i64 = row.get(0)?;
    let request_id: String = row.get(1)?;
    let trace_id: String = row.get(2)?;
    let attempt: i64 = row.get(3)?;
    let provider_id: String = row.get(4)?;
    let account_id: Option<i64> = row.get(5)?;
    let combo_id: Option<i64> = row.get(6)?;
    let combo_target_id: Option<i64> = row.get(7)?;
    let model_row_id: Option<i64> = row.get(8)?;
    let upstream_model_id: String = row.get(9)?;
    let prompt_tokens: Option<i64> = row.get(10)?;
    let completion_tokens: Option<i64> = row.get(11)?;
    let connect_ms: Option<i64> = row.get(12)?;
    let ttft_ms: Option<i64> = row.get(13)?;
    let total_ms: i64 = row.get(14)?;
    let tokens_per_sec: Option<f64> = row.get(15)?;
    let status_code: i64 = row.get(16)?;
    let error_msg: Option<String> = row.get(17)?;
    let error_msg_redacted: Option<String> = row.get(18)?;
    let race_total: i64 = row.get(19)?;
    let race_attempts: i64 = row.get(20)?;
    let race_lost: i64 = row.get(21)?;
    let api_key_id: Option<i64> = row.get(22)?;
    let created_at: String = row.get(23)?;
    let is_streaming: i64 = row.get(24)?;
    let stream_complete: i64 = row.get(25)?;
    let mut col_idx = 26;
    let request_body_json = parse_json_from_row(row.get(col_idx)?);
    col_idx += 1;
    let response_body_json = parse_json_from_row(row.get(col_idx)?);
    col_idx += 1;
    let request_headers = parse_json_from_row(row.get(col_idx)?);
    col_idx += 1;
    let response_headers = parse_json_from_row(row.get(col_idx)?);
    col_idx += 1;
    let error_message: Option<String> = row.get(col_idx)?;
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
    let pii_redacted: Option<String> = row.get(col_idx).unwrap_or(None);

    if !(0..=i64::from(u16::MAX)).contains(&status_code) {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            16,
            rusqlite::types::Type::Integer,
            Box::new(SimpleErr(format!(
                "status_code out of u16 range: {status_code}"
            ))),
        ));
    }
    if total_ms < 0 {
        return Err(rusqlite::Error::FromSqlConversionFailure(
            14,
            rusqlite::types::Type::Integer,
            Box::new(SimpleErr(format!(
                "total_ms unexpectedly negative: {total_ms}"
            ))),
        ));
    }
    let endpoint_kind = endpoint_kind_str.parse().unwrap_or_default();

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

    Ok(UsageDetailRow {
        id: UsageId(id),
        request_id,
        trace_id,
        attempt,
        provider_id: ProviderId::new(provider_id),
        account_id: account_id.map(AccountId),
        combo_id: combo_id.map(ComboId),
        combo_target_id: combo_target_id.map(ComboTargetId),
        model_row_id: model_row_id.map(ModelRowId),
        upstream_model_id,
        prompt_tokens,
        completion_tokens,
        connect_ms,
        ttft_ms,
        total_ms,
        tokens_per_sec,
        status_code: status_code as u16,
        error_msg,
        error_msg_redacted,
        request_body_json,
        response_body_json,
        request_headers,
        response_headers,
        race_total,
        race_attempts,
        created_at,
        api_key_id: api_key_id.map(ApiKeyId),
        error_message,
        proxy_url,
        proxy_status,
        endpoint_kind,
        pii_redacted,
        flags,
    })
}

pub fn detail_by_id(conn: &Connection, id: i64) -> Result<Option<UsageDetailRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, request_id, trace_id, attempt, provider_id, account_id, \
                    combo_id, combo_target_id, model_row_id, upstream_model_id, \
                    prompt_tokens, completion_tokens, connect_ms, ttft_ms, \
                    total_ms, tokens_per_sec, status_code, error_msg, \
                    error_msg_redacted, race_total, race_attempts, race_lost, \
                    api_key_id, created_at, is_streaming, stream_complete, \
                    request_body_json, response_body_json, request_headers, \
                    response_headers, error_message, client_response, \
                    prompt_tokens_estimated, completion_tokens_estimated, \
                    endpoint_kind, proxy_url, proxy_status, is_proxy_rotated, \
                    pii_redacted \
             FROM usage \
             WHERE id = ?1",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let row = stmt
        .query_row(params![id], row_to_usage_detail)
        .optional()
        .map_err(openproxy_db::error::map_db_error)?;

    Ok(row)
}

pub fn detail_by_trace_id(conn: &Connection, trace_id: &str) -> Result<Option<UsageDetailRow>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, request_id, trace_id, attempt, provider_id, account_id, \
                    combo_id, combo_target_id, model_row_id, upstream_model_id, \
                    prompt_tokens, completion_tokens, connect_ms, ttft_ms, \
                    total_ms, tokens_per_sec, status_code, error_msg, \
                    error_msg_redacted, race_total, race_attempts, race_lost, \
                    api_key_id, created_at, is_streaming, stream_complete, \
                    request_body_json, response_body_json, request_headers, \
                    response_headers, error_message, client_response, \
                    prompt_tokens_estimated, completion_tokens_estimated, \
                    endpoint_kind, proxy_url, proxy_status, is_proxy_rotated, \
                    pii_redacted \
             FROM usage \
             WHERE trace_id = ?1 \
             ORDER BY client_response DESC, id DESC \
             LIMIT 1",
        )
        .map_err(openproxy_db::error::map_db_error)?;

    let row = stmt
        .query_row(params![trace_id], row_to_usage_detail)
        .optional()
        .map_err(openproxy_db::error::map_db_error)?;

    Ok(row)
}

pub fn prune_expired_recording_bodies(conn: &Connection, ttl_secs: i64) -> Result<usize> {
    let ttl_secs = ttl_secs.max(0);
    let n = conn
        .execute(
            "UPDATE usage \
             SET request_body_json = NULL, \
                 response_body_json = NULL, \
                 request_headers = NULL, \
                 response_headers = NULL \
             WHERE datetime(created_at) <= datetime(?1, ?2)",
            params![
                chrono::Utc::now().to_rfc3339(),
                format!("-{} seconds", ttl_secs)
            ],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(n)
}

pub fn prune_expired_usage_rows(conn: &Connection, ttl_days: i64) -> Result<usize> {
    let ttl_days = ttl_days.max(0);
    let n = conn
        .execute(
            "DELETE FROM usage \
             WHERE datetime(created_at) <= datetime(?1, ?2)",
            params![
                chrono::Utc::now().to_rfc3339(),
                format!("-{} days", ttl_days)
            ],
        )
        .map_err(openproxy_db::error::map_db_error)?;
    Ok(n)
}
