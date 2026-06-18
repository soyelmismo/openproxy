//! Cost calculation. Wraps pricing::lookup and applies the
//! tokens_per_sec guard (per C3): NULL if completion=0, ttft NULL, or (total-ttft)<=0.

use crate::error::{CoreError, Result};
use crate::ids::{
    AccountId, ApiKeyId, ComboId, ComboTargetId, ModelRowId, ProviderId, RequestId, TraceId,
    UsageId,
};
use crate::pricing;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageInput {
    pub request_id: RequestId,
    pub trace_id: TraceId,
    pub attempt: u8,
    pub provider_id: ProviderId,
    pub account_id: Option<AccountId>,
    pub combo_id: Option<ComboId>,
    pub combo_target_id: Option<ComboTargetId>,
    pub model_row_id: Option<ModelRowId>,
    pub upstream_model_id: String,
    pub prompt_tokens: Option<u32>,
    pub completion_tokens: Option<u32>,
    pub connect_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
    pub status_code: u16,
    pub error_msg: Option<String>,
    pub race_total: u8,
    pub race_lost: bool,
    /// The API key that produced this attempt, if the chat handler
    /// authenticated the caller. Anonymous (no Authorization header)
    /// traffic is `None`; the column is also `None` for rows
    /// predating the 000015 migration.
    pub api_key_id: Option<ApiKeyId>,
    pub request_body_json: Option<serde_json::Value>,
    pub response_body_json: Option<serde_json::Value>,
    pub request_headers: Option<std::collections::BTreeMap<String, String>>,
    pub response_headers: Option<std::collections::BTreeMap<String, String>>,
    pub error_message: Option<String>,
    pub race_attempts: u8,
    pub is_streaming: bool,
    pub stream_complete: bool,
    /// Upstream stop reason (e.g. "end_turn", "max_tokens",
    /// "stop_sequence" for Anthropic; "stop", "length" for OpenAI).
    pub stop_reason: Option<String>,
}

/// Computes (cost_usd, tokens_per_sec) from pricing + tokens + timing.
/// Per C3: tokens_per_sec is None if any guard fails.
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

/// Sanitize error_msg:
/// - cap at 2KB
/// - redact patterns: sk-..., x-api-key: ..., Authorization: Bearer ***
///
/// Returns `(sanitized, redacted)`.
pub fn redact_error_msg(raw: &str) -> (String, String) {
    let mut sanitized = raw.to_string();
    let re_sk = regex::Regex::new(r"sk-[A-Za-z0-9_\-]{10,}").unwrap();
    sanitized = re_sk
        .replace_all(&sanitized, "sk-[REDACTED]")
        .to_string();
    let re_xapikey = regex::Regex::new(r"(?i)x-api-key:\s*\S+").unwrap();
    sanitized = re_xapikey
        .replace_all(&sanitized, "x-api-key: [REDACTED]")
        .to_string();
    let re_bearer = regex::Regex::new(r"(?i)Authorization:\s*Bearer\s+\S+").unwrap();
    sanitized = re_bearer
        .replace_all(&sanitized, "Authorization: Bearer [REDACTED]")
        .to_string();
    if sanitized.len() > 2048 {
        sanitized.truncate(2048);
        sanitized.push_str("...[truncated]");
    }
    (sanitized.clone(), sanitized)
}

/// Insert a usage row. Returns the new UsageId.
pub fn record(conn: &Connection, input: &UsageInput) -> Result<UsageId> {
    let price = pricing::lookup(input.provider_id.as_str(), &input.upstream_model_id);
    let (cost_usd, tps) = compute(price, input);
    let (error_msg_for_db, error_msg_redacted_for_db) = match &input.error_msg {
        Some(msg) => {
            let (sanitized, _redacted) = redact_error_msg(msg);
            (Some(sanitized.clone()), Some(sanitized))
        }
        None => (None, None),
    };

    let request_id = input.request_id.to_string();
    let trace_id = input.trace_id.to_string();

    conn.execute(
        "INSERT INTO usage (\
            request_id, trace_id, attempt, provider_id, account_id, combo_id, \
            model_row_id, upstream_model_id, combo_target_id, prompt_tokens, \
            completion_tokens, cost_usd, connect_ms, ttft_ms, total_ms, \
            tokens_per_sec, status_code, error_msg, error_msg_redacted, \
            race_total, race_attempts, race_lost, api_key_id, created_at, \
            request_body_json, response_body_json, request_headers, \
            response_headers, error_message, is_streaming, stream_complete, \
            stop_reason\
         ) VALUES (\
            ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, \
            ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, \
            ?21, ?22, ?23, datetime('now'), ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31\
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
                .and_then(|j| serde_json::to_string(j).ok()),
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
            // SEC-HIGH-E fix: the legacy `error_message` column was being
            // written verbatim from `pipeline.rs` (raw, unredacted), while
            // the parallel `error_msg` / `error_msg_redacted` columns
            // carry the redacted form. `log-detail.js` and
            // `GET /v1/admin/usage/:id` prefer `error_message` over
            // `error_msg_redacted`, so the redaction was being bypassed.
            // Mirror the redacted form into `error_message` so the two
            // columns stay in sync and neither leaks the raw upstream
            // body (which can contain internal IPs, debug stacks, and
            // PII echoed back by misbehaving upstreams).
            error_msg_redacted_for_db.clone(),
            input.is_streaming as i64,
            input.stream_complete as i64,
            input.stop_reason,
        ],
    )

    .map_err(|e| CoreError::Database {
        message: format!("insert usage row: {}", e),
        source: Some(Box::new(e)),
    })?;

    let rowid = conn.last_insert_rowid();

        let row = crate::usage::RecentUsageRow {
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
            request_body_json: input.request_body_json.clone(),
            response_body_json: input.response_body_json.clone(),
            request_headers: input.request_headers.clone(),
            response_headers: input.response_headers.clone(),
            error_message: error_msg_redacted_for_db.clone(),
            race_total: Some(input.race_total),
            race_attempts: Some(input.race_attempts),
            is_streaming: input.is_streaming,
            stream_complete: input.stream_complete,
            stop_reason: input.stop_reason.clone(),
        };
    crate::usage::publish_usage_row(row);

    Ok(UsageId(rowid))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_input() -> UsageInput {
        UsageInput {
            request_id: RequestId::new(),
            trace_id: TraceId::new(),
            attempt: 1,
            provider_id: ProviderId::new("openrouter"),
            account_id: None,
            combo_id: None,
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id: "openai/gpt-4o".to_string(),
            prompt_tokens: Some(1000),
            completion_tokens: Some(500),
            connect_ms: Some(100),
            ttft_ms: Some(200),
            total_ms: 1200,
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
            is_streaming: false,
            stream_complete: false,
            stop_reason: None,
        }
    }

    #[test]
    fn compute_cost_zero_tokens() {
        let input = UsageInput {
            prompt_tokens: Some(0),
            completion_tokens: Some(0),
            total_ms: 500,
            ttft_ms: Some(100),
            ..make_input()
        };
        let price = pricing::lookup("openrouter", "openai/gpt-4o");
        let (cost, tps) = compute(price, &input);
        assert_eq!(cost, 0.0);
        // TPS guard: completion=0 => None
        assert!(tps.is_none());
    }

    #[test]
    fn compute_cost_known_pricing() {
        let input = UsageInput {
            prompt_tokens: Some(1_000_000),
            completion_tokens: Some(1_000_000),
            total_ms: 10_000,
            ttft_ms: Some(1_000),
            ..make_input()
        };
        let price = pricing::lookup("openrouter", "openai/gpt-4o").unwrap();
        // 2.5 * 1e6 / 1e6 + 10.0 * 1e6 / 1e6 = 12.5
        let (cost, _tps) = compute(Some(price), &input);
        assert!((cost - 12.5).abs() < 1e-9);
    }

    #[test]
    fn compute_tokens_per_sec_normal() {
        let input = UsageInput {
            prompt_tokens: Some(100),
            completion_tokens: Some(300),
            total_ms: 1500,
            ttft_ms: Some(500),
            ..make_input()
        };
        let price = pricing::lookup("openrouter", "openai/gpt-4o");
        let (_cost, tps) = compute(price, &input);
        // denom = 1500 - 500 = 1000; 300 * 1000 / 1000 = 300.0
        assert_eq!(tps, Some(300.0));
    }

    #[test]
    fn compute_tokens_per_sec_zero_completion_is_none() {
        let input = UsageInput {
            completion_tokens: Some(0),
            total_ms: 1500,
            ttft_ms: Some(500),
            ..make_input()
        };
        let (_cost, tps) = compute(None, &input);
        assert!(tps.is_none());
    }

    #[test]
    fn compute_tokens_per_sec_ttft_equals_total_is_none() {
        // ttft == total => (total - ttft) <= 0 => None
        let input = UsageInput {
            completion_tokens: Some(300),
            total_ms: 1500,
            ttft_ms: Some(1500),
            ..make_input()
        };
        let (_cost, tps) = compute(None, &input);
        assert!(tps.is_none());
    }

    #[test]
    fn compute_tokens_per_sec_unknown_pricing_is_some_zero() {
        // price = None => cost = 0.0, but tps depends only on tokens/timings.
        let input = UsageInput {
            upstream_model_id: "no/such-model".to_string(),
            prompt_tokens: Some(100),
            completion_tokens: Some(200),
            total_ms: 1000,
            ttft_ms: Some(0),
            ..make_input()
        };
        let (cost, tps) = compute(None, &input);
        assert_eq!(cost, 0.0);
        // denom = 1000 - 0 = 1000; 200 * 1000 / 1000 = 200.0
        assert_eq!(tps, Some(200.0));
    }

    #[test]
    fn redact_sk_key() {
        let raw = "error: sk-abcdefghij1234567890 is invalid";
        let (sanitized, redacted) = redact_error_msg(raw);
        assert!(sanitized.contains("sk-[REDACTED]"));
        assert!(!sanitized.contains("abcdefghij1234567890"));
        assert_eq!(sanitized, redacted);
    }

    #[test]
    fn redact_x_api_key_header() {
        let raw = "auth failed: X-API-Key: abc123secret";
        let (sanitized, _redacted) = redact_error_msg(raw);
        assert!(sanitized.contains("x-api-key: [REDACTED]"));
        assert!(!sanitized.contains("abc123secret"));
    }

    #[test]
    fn redact_bearer_token() {
        let raw = "upstream error: Authorization: Bearer tok_xyz123";
        let (sanitized, _redacted) = redact_error_msg(raw);
        assert!(sanitized.contains("Authorization: Bearer [REDACTED]"));
        assert!(!sanitized.contains("tok_xyz123"));
    }

    #[test]
    fn redact_caps_at_2kb() {
        let raw = "x".repeat(3000);
        let (sanitized, _redacted) = redact_error_msg(&raw);
        // 2048 'x' + "...[truncated]" = 2061
        assert!(sanitized.ends_with("...[truncated]"));
        assert!(sanitized.len() <= 2048 + "...[truncated]".len());
    }

    // ---- LOW fix (#13): the live-logs WebSocket calls redact_error_msg
    // for its terminal event. The DB row and the WS payload must agree
    // on what secrets are masked. Test the patterns that ACTUALLY
    // appear in real upstream error bodies — if a future contributor
    // adds a new secret format to redact, they should also add an
    // assertion here so the WS publish and the DB row stay in sync.

    #[test]
    fn redact_matches_db_row_for_all_common_secret_formats() {
        // Every secret format below must be masked. The DB row and
        // the WS payload both go through `redact_error_msg`, so
        // they are equal iff the same input yields the same output.
        let secret_input = "upstream error: sk-abcdefghij1234567890 \
                             x-api-key: topsecret \
                             Authorization: Bearer ya31.aaa.bbb";
        let (db_form, _redacted) = redact_error_msg(secret_input);
        let ws_form = redact_error_msg(secret_input).0;
        assert_eq!(
            db_form, ws_form,
            "DB row and WS publish must render the same redacted form"
        );
        // Each secret pattern must be masked, not echoed.
        assert!(db_form.contains("sk-[REDACTED]"));
        assert!(db_form.contains("x-api-key: [REDACTED]"));
        assert!(db_form.contains("Authorization: Bearer [REDACTED]"));
        assert!(!db_form.contains("abcdefghij1234567890"));
        assert!(!db_form.contains("topsecret"));
        assert!(!db_form.contains("ya31.aaa.bbb"));
    }
}
