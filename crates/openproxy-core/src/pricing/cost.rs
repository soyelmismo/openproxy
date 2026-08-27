//! Cost calculation. Wraps pricing::lookup and applies the
//! tokens_per_sec guard (per C3): NULL if completion=0, ttft NULL, or (total-ttft)<=0.

use crate::error::Result;
use crate::ids::UsageId;
use crate::pricing;
use rusqlite::{Connection, params};
use std::sync::LazyLock;

pub use openproxy_types::usage::UsageInput;

fn has_valid_tps_timing(completion: u32, total_ms: u64, ttft_ms: u64, status_code: u16) -> bool {
    completion > 0 && total_ms > ttft_ms && status_code < 400
}

fn compute_tokens_per_sec(input: &UsageInput) -> Option<f64> {
    let c = input.completion_tokens?;
    let ttft = input.ttft_ms?;
    if !has_valid_tps_timing(c, input.total_ms, ttft, input.status_code) {
        return None;
    }
    let denom = (input.total_ms - ttft) as f64;
    Some(f64::from(c) * 1000.0 / denom)
}

/// Computes (cost_usd, tokens_per_sec) from pricing + tokens + timing.
/// Per C3: tokens_per_sec is None if any guard fails.
pub fn compute(price: Option<pricing::Price>, input: &UsageInput) -> (f64, Option<f64>) {
    let cost = match input.status_code {
        0..400 => pricing::compute_cost_with_cache(
            price,
            input.prompt_tokens.unwrap_or(0),
            input.completion_tokens.unwrap_or(0),
            input.cached_tokens,
        ),
        _ => 0.0,
    };
    (cost, compute_tokens_per_sec(input))
}

fn apply_secret_redactions(text: String) -> String {
    static RE_SK: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"sk-[A-Za-z0-9_\-]{10,}").expect("valid regex"));
    static RE_XAPIKEY: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"(?i)x-api-key:\s*\S+").expect("valid regex"));
    static RE_BEARER: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)Authorization:\s*Bearer\s+\S+").expect("valid regex")
    });

    let patterns: &[(&LazyLock<regex::Regex>, &str)] = &[
        (&RE_SK, "sk-[REDACTED]"),
        (&RE_XAPIKEY, "x-api-key: [REDACTED]"),
        (&RE_BEARER, "Authorization: Bearer [REDACTED]"),
    ];

    patterns.iter().fold(text, |acc, (re, rep)| {
        if re.is_match(&acc) {
            re.replace_all(&acc, *rep).into_owned()
        } else {
            acc
        }
    })
}

fn truncate_error_message(mut text: String) -> String {
    if text.len() <= 2048 {
        return text;
    }
    let mut idx = 2048;
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    text.truncate(idx);
    text.push_str("...[truncated]");
    text
}

/// Sanitize error_msg:
/// - cap at 2KB
/// - redact patterns: sk-..., x-api-key: ..., Authorization: Bearer ***
///
/// Returns `(sanitized, redacted)`.
pub fn redact_error_msg(raw: &str) -> (String, String) {
    let sanitized = truncate_error_message(apply_secret_redactions(raw.to_string()));
    (sanitized.clone(), sanitized)
}

fn warn_missing_pricing(input: &UsageInput, price: Option<&pricing::Price>) {
    if price.is_none()
        && (input.prompt_tokens.is_some_and(|t| t > 0)
            || input.completion_tokens.is_some_and(|t| t > 0))
    {
        tracing::warn!(
            provider_id = %input.provider_id,
            upstream_model_id = %input.upstream_model_id,
            "no pricing data found; recording cost_usd = 0 (run models.dev sync or set pricing manually)"
        );
    }
}

fn sanitize_db_error_msg(error_msg: Option<&str>) -> (Option<String>, Option<String>) {
    match error_msg {
        Some(msg) => {
            let (sanitized, _) = redact_error_msg(msg);
            (Some(sanitized.clone()), Some(sanitized))
        }
        None => (None, None),
    }
}

fn build_recent_usage_row(
    rowid: i64,
    input: &UsageInput,
    request_id: String,
    trace_id: String,
    cost_usd: f64,
    error_msg: Option<String>,
) -> openproxy_types::usage::RecentUsageRow {
    openproxy_types::usage::RecentUsageRow {
        id: UsageId(rowid),
        request_id,
        trace_id,
        provider_id: input.provider_id.clone(),
        upstream_model_id: input.upstream_model_id.clone(),
        status_code: input.status_code,
        total_ms: input.total_ms,
        prompt_tokens: input.prompt_tokens,
        completion_tokens: input.completion_tokens,
        cached_tokens: None,
        cost_usd: Some(cost_usd),
        race_lost: input.race_lost,
        created_at: chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        connect_ms: input.connect_ms,
        ttft_ms: input.ttft_ms,
        // PERF: set heavy fields to None for the broadcast row.
        // The dashboard never receives these fields — they're
        // fetched on demand via GET /admin/usage/:id. Cloning
        // them here was pure waste (up to MB of JSON per request).
        request_body_json: None,
        response_body_json: None,
        request_headers: None,
        response_headers: None,
        error_message: error_msg,
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
    }
}

fn serialize_json_field<T: serde::Serialize>(val: Option<&T>) -> Option<String> {
    val.and_then(|j| serde_json::to_string(j).ok())
}

fn execute_usage_insert(
    conn: &Connection,
    input: &UsageInput,
    cost_usd: f64,
    tps: Option<f64>,
    error_msg_db: (Option<String>, Option<String>),
) -> Result<i64> {
    let (err_raw, err_redacted) = error_msg_db;
    let req_body = input
        .request_body_json
        .as_ref()
        .and_then(|j| std::str::from_utf8(j).ok());
    let resp_body = serialize_json_field(input.response_body_json.as_ref());
    let req_headers = serialize_json_field(input.request_headers.as_ref());
    let resp_headers = serialize_json_field(input.response_headers.as_ref());

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
            input.request_id.to_string(),
            input.trace_id,
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
            err_raw,
            err_redacted,
            i64::from(input.race_total),
            i64::from(input.race_attempts),
            i64::from(input.race_lost),
            input.api_key_id.map(|k| k.0),
            req_body,
            resp_body,
            req_headers,
            resp_headers,
            err_redacted.as_deref(),
            i64::from(input.is_streaming),
            i64::from(input.stream_complete),
            input.stop_reason,
            input.compression_savings_pct,
            input.compression_techniques,
            i64::from(input.client_response),
            i64::from(input.prompt_tokens_estimated),
            i64::from(input.completion_tokens_estimated),
            input.endpoint_kind.as_str(),
            input.proxy_url,
            input.proxy_status,
            i64::from(input.is_proxy_rotated),
        ],
    )
    .map_err(openproxy_db::error::map_db_error)?;

    Ok(conn.last_insert_rowid())
}

/// Insert a usage row. Returns the new UsageId.
pub fn record(conn: &Connection, input: &UsageInput) -> Result<UsageId> {
    let price =
        pricing::lookup_with_db(conn, input.provider_id.as_str(), &input.upstream_model_id);
    warn_missing_pricing(input, price.as_ref());
    let (cost_usd, tps) = compute(price, input);
    let db_err_msgs = sanitize_db_error_msg(input.error_msg.as_deref());
    let error_msg_redacted = db_err_msgs.1.clone();

    let rowid = execute_usage_insert(conn, input, cost_usd, tps, db_err_msgs)?;
    let row = build_recent_usage_row(
        rowid,
        input,
        input.request_id.to_string(),
        input.trace_id.clone(),
        cost_usd,
        error_msg_redacted,
    );
    openproxy_types::usage::publish_usage_row(row);

    Ok(UsageId(rowid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{ProviderId, RequestId, TraceId};

    fn make_input() -> UsageInput {
        UsageInput {
            proxy_url: None,
            proxy_status: None,
            is_proxy_rotated: false,
            request_id: RequestId::new(),
            trace_id: TraceId::new().to_string(),
            attempt: 1,
            provider_id: ProviderId::new("openrouter"),
            account_id: None,
            combo_id: None,
            combo_target_id: None,
            model_row_id: None,
            upstream_model_id: "openai/gpt-4o".to_string(),
            prompt_tokens: Some(1000),
            completion_tokens: Some(500),
            cached_tokens: None,
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
            compression_savings_pct: None,
            compression_techniques: None,
            client_response: false,
            prompt_tokens_estimated: false,
            completion_tokens_estimated: false,
            endpoint_kind: openproxy_types::endpoint::EndpointKind::Chat,
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
