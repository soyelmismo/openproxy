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
    // Un cancel del cliente (499) SÍ factura: el seller no aborta su upstream
    // cuando el buyer corta el stream, y reclama input completo + generación
    // completa por NeedAuth. El bloque de usage final no llegó, así que no hay
    // reparto de caché: todos los prompt tokens a input uncached + los tokens
    // entregados en vuelo (estimados del acumulador parcial). El resto de
    // errores sigue en 0: un upstream caído no se le cobra al usuario.
    let billable =
        (input.status_code >= 200 && input.status_code < 400) || input.status_code == 499;
    let cost = if billable {
        pricing::compute_cost_opt_with_cache(
            price,
            input.prompt_tokens.unwrap_or(0),
            input.completion_tokens.unwrap_or(0),
            if input.status_code == 499 {
                None
            } else {
                input.cached_tokens
            },
        )
    } else {
        Some(0.0)
    };
    let tps = match (input.completion_tokens, input.total_ms, input.status_code) {
        (Some(c), total_ms, status) if c > 0 && total_ms > 0 && status < 400 => {
            Some(f64::from(c) * 1000.0 / (total_ms as f64))
        }
        _ => None,
    };
    (cost, tps)
}

pub fn redact_error_msg(raw: &str) -> String {
    // Canonical redaction lives in openproxy_types::redaction: this copy used
    // to miss the `op_live_` pattern (the product's own key format) and the
    // JWT/Google-key/Bearer families, leaking them into usage.error_msg.
    openproxy_types::redaction::sanitize_error_message(raw)
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
    created_at: &str,
) -> openproxy_types::Result<i64> {
    let request_id = input.request_id.to_string();

    let mut api_key_id = input.api_key_id.map(|k| k.0);
    if let Some(id) = api_key_id {
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM api_keys WHERE id = ?1)",
                rusqlite::params![id],
                |row| row.get(0),
            )
            .map_err(crate::error::map_db_error)?;
        if !exists {
            api_key_id = None;
        }
    }

    let execute_insert = |key_id: Option<i64>| {
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
                was_winner, audio_ms, session_hash, max_chunk_gap_ms, avg_chunk_gap_ms\
             ) VALUES (\
                ?1,  ?2,  ?3,  ?4,  ?5,  ?6,  ?7,  ?8,  ?9,  ?10, \
                ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, \
                ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, \
                ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44, ?45, ?46, ?47, ?48\
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
                key_id,
                created_at,
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
                input.audio_ms.map(i64::from),
                input.session_hash.map(|h| h as i64),
                input.max_chunk_gap_ms.map(|g| g as i64),
                input.avg_chunk_gap_ms.map(|g| g as i64),
            ],
        )
    };

    // Missing keys were resolved above. Any other insert failure must retain
    // attribution and propagate: retrying as anonymous can bypass a trigger
    // failure and turn a prepaid response into unbilled usage.
    execute_insert(api_key_id).map_err(crate::error::map_db_error)?;
    Ok(conn.last_insert_rowid())
}

static WARNED_MISSING_PRICING: LazyLock<parking_lot::Mutex<std::collections::HashSet<u64>>> =
    LazyLock::new(|| parking_lot::Mutex::new(std::collections::HashSet::new()));

fn warn_missing_pricing_once(provider_id: &str, upstream_model_id: &str) {
    use std::hash::{DefaultHasher, Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    provider_id.hash(&mut hasher);
    upstream_model_id.hash(&mut hasher);
    let key = hasher.finish();

    let mut warned = WARNED_MISSING_PRICING.lock();
    if warned.len() > 1024 {
        warned.clear();
    }
    if warned.insert(key) {
        tracing::warn!(
            provider_id = %provider_id,
            upstream_model_id = %upstream_model_id,
            "no pricing data found; recording cost_usd = NULL (run models.dev sync or set pricing manually)"
        );
    }
}

/// The `usage.created_at` column format (UTC, whole seconds), matching what
/// [`pricing::parse_usage_timestamp`] reads back.
fn usage_created_at(billed_at: chrono::DateTime<chrono::Utc>) -> String {
    billed_at.format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Persists a usage row and returns `(UsageId, RecentUsageRow)` so callers can release
/// the database connection lock before broadcasting the row, eliminating lock contention (AGENTS.md §4.3).
pub fn record_row(
    conn: &Connection,
    input: &UsageInput,
) -> openproxy_types::Result<(UsageId, RecentUsageRow)> {
    record_row_at(conn, input, chrono::Utc::now())
}

/// [`record_row`], billed at an explicit instant.
///
/// One snapshot per request: `billed_at` is truncated to the second *before* the
/// band is resolved, and that same truncated instant is what gets written to
/// `usage.created_at`. Re-pricing the row from its own timestamp therefore
/// reproduces exactly this band — no request is ever charged a tariff its own
/// stored timestamp contradicts, even when the wall clock crosses a window
/// boundary between pricing and insert.
pub fn record_row_at(
    conn: &Connection,
    input: &UsageInput,
    billed_at: chrono::DateTime<chrono::Utc>,
) -> openproxy_types::Result<(UsageId, RecentUsageRow)> {
    use chrono::Timelike;
    let billed_at = billed_at.with_nanosecond(0).unwrap_or(billed_at);
    let created_at = usage_created_at(billed_at);
    // A combo with its own price bills at that price; otherwise the target
    // model's rate applies. Never a blend of both. A combo whose schedule is
    // unreadable yields no price at all, which records `cost_usd = NULL`
    // instead of charging a tariff nobody agreed to.
    let price = pricing::lookup_for_request_at(
        conn,
        input.provider_id.as_str(),
        &input.upstream_model_id,
        input.combo_id.map(|c| c.0),
        billed_at,
    );
    if price.is_none()
        && (input.prompt_tokens.unwrap_or(0) > 0 || input.completion_tokens.unwrap_or(0) > 0)
    {
        warn_missing_pricing_once(input.provider_id.as_str(), &input.upstream_model_id);
    }
    let (cost_usd, tps) = {
        // STT bills per hour of audio, and its duration lives in `audio_ms`
        // rather than the token columns. Route it through the `prompt_tokens`
        // slot of the cost math (milliseconds) so `PriceKind::Audio` sees a
        // billable quantity instead of pricing the row at zero.
        let cost_input;
        let billable = match (input.audio_ms, price.as_ref().map(|p| p.kind)) {
            (Some(ms), Some(pricing::PriceKind::Audio)) => {
                cost_input = UsageInput {
                    prompt_tokens: Some(ms),
                    completion_tokens: Some(0),
                    ..input.clone()
                };
                &cost_input
            }
            (None, Some(pricing::PriceKind::Audio)) => {
                cost_input = UsageInput {
                    prompt_tokens: Some(0),
                    completion_tokens: Some(0),
                    ..input.clone()
                };
                &cost_input
            }
            _ => input,
        };
        compute(price, billable)
    };
    let error_msg_for_db = prepare_usage_error_msg(input.error_msg.as_ref());

    let (rowid, charged_units) = crate::credits_billing::transaction(conn, |conn| {
        let id = insert_usage_record(
            conn,
            input,
            cost_usd,
            tps,
            error_msg_for_db.as_deref(),
            error_msg_for_db.as_deref(),
            &created_at,
        )?;
        // Same transaction: the row can never carry a debit marker without the
        // amount that debit actually moved (`None` = no debit was attempted).
        #[cfg(feature = "payments")]
        let charged = crate::credits_billing::debit_usage(conn, id)?;
        #[cfg(not(feature = "payments"))]
        let charged = crate::credits_billing::debit_usage(conn, id);
        Ok((id, charged))
    })?;

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
        created_at: billed_at.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
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
        api_key_id: input.api_key_id.map(|k| k.0),
        audio_ms: input.audio_ms,
        session_hash: input.session_hash,
        charged_units,
        credits_debited: charged_units.is_some(),
    };

    Ok((UsageId(rowid), row))
}

#[inline]
fn broadcast_usage_row(row: RecentUsageRow) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(async move {
            publish_usage_row(row);
        });
    } else {
        publish_usage_row(row);
    }
}

pub fn record(conn: &Connection, input: &UsageInput) -> openproxy_types::Result<UsageId> {
    let (usage_id, row) = record_row(conn, input)?;
    broadcast_usage_row(row);
    Ok(usage_id)
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
    use pricing::PriceKind;
    // Rows that ran through a priced combo are billed by the combo pass; the
    // model fallback must never overwrite them with the target model's rate.
    // A combo with a schedule is priced by time of day and is excluded here for
    // the same reason: the target model's flat rate is not its bill.
    const NOT_A_PRICED_COMBO: &str = "AND (combo_id IS NULL OR combo_id NOT IN \
         (SELECT id FROM combos WHERE (pricing_input_per_1m IS NOT NULL \
            AND pricing_output_per_1m IS NOT NULL) OR pricing_schedule IS NOT NULL))";
    match price {
        // Audio rows bill on `audio_ms` at an hourly rate; the token formula
        // below would price them at zero forever.
        Some(p) if p.kind == PriceKind::Audio => {
            let per_hour = p.audio_per_hour.unwrap_or(p.input_per_1m);
            conn.execute(
                &format!(
                    "UPDATE usage \
                     SET cost_usd = (?1 * audio_ms / 3600000.0) \
                     WHERE provider_id = ?2 AND upstream_model_id = ?3 \
                       AND audio_ms > 0 AND (cost_usd = 0.0 OR cost_usd IS NULL) \
                       {NOT_A_PRICED_COMBO}"
                ),
                params![per_hour, provider_id, upstream_model_id],
            )
            .map_err(crate::error::map_db_error)
        }
        Some(p) => conn
            .execute(
                &format!(
                    "UPDATE usage \
                     SET cost_usd = (COALESCE(?1, 0.0) * prompt_tokens / 1000000.0) + \
                                    (COALESCE(?2, 0.0) * COALESCE(completion_tokens, 0) / 1000000.0) \
                     WHERE provider_id = ?3 AND upstream_model_id = ?4 \
                       AND prompt_tokens > 0 AND (cost_usd = 0.0 OR cost_usd IS NULL) \
                       {NOT_A_PRICED_COMBO}"
                ),
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
                &format!(
                    "UPDATE usage \
                     SET cost_usd = NULL \
                     WHERE provider_id = ?1 AND upstream_model_id = ?2 \
                       AND prompt_tokens > 0 AND cost_usd = 0.0 \
                       {NOT_A_PRICED_COMBO}"
                ),
                params![provider_id, upstream_model_id],
            )
            .map_err(crate::error::map_db_error),
    }
}

/// Backfill rows routed through a combo that carries its own price.
fn update_backfill_combo_price(
    conn: &Connection,
    combo_id: i64,
    price: pricing::Price,
) -> openproxy_types::Result<usize> {
    if price.kind == pricing::PriceKind::Audio {
        let per_hour = price.audio_per_hour.unwrap_or(price.input_per_1m);
        return conn
            .execute(
                "UPDATE usage SET cost_usd = (?1 * audio_ms / 3600000.0) \
                 WHERE combo_id = ?2 AND audio_ms > 0 \
                   AND (cost_usd = 0.0 OR cost_usd IS NULL)",
                params![per_hour, combo_id],
            )
            .map_err(crate::error::map_db_error);
    }
    conn.execute(
        "UPDATE usage \
         SET cost_usd = (COALESCE(?1, 0.0) * prompt_tokens / 1000000.0) + \
                        (COALESCE(?2, 0.0) * COALESCE(completion_tokens, 0) / 1000000.0) \
         WHERE combo_id = ?3 AND prompt_tokens > 0 \
           AND (cost_usd = 0.0 OR cost_usd IS NULL)",
        params![price.input_per_1m, price.output_per_1m, combo_id],
    )
    .map_err(crate::error::map_db_error)
}

/// Re-price one scheduled combo's zero-cost rows at the band each was billed in.
///
/// A time-of-day price has no single rate: the band is a function of the instant
/// the row was billed at, and `usage.created_at` is that instant. Rows whose
/// timestamp cannot be read — and every row of a combo whose persisted schedule
/// no longer parses — are skipped, never re-priced at today's band. Rewriting
/// history with a tariff that never applied is worse than leaving it at zero.
type ScheduledUsageRow = (i64, Option<String>, Option<u32>, Option<u32>, Option<u32>);

fn backfill_scheduled_combo(conn: &Connection, combo_id: i64) -> openproxy_types::Result<usize> {
    let rows: Vec<ScheduledUsageRow> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, created_at, prompt_tokens, completion_tokens, cached_tokens \
                 FROM usage \
                 WHERE combo_id = ?1 AND prompt_tokens > 0 \
                   AND (cost_usd = 0.0 OR cost_usd IS NULL)",
            )
            .map_err(crate::error::map_db_error)?;
        let mapped = stmt
            .query_map(params![combo_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .map_err(crate::error::map_db_error)?;
        mapped.filter_map(std::result::Result::ok).collect()
    };
    let mut updated = 0;
    for (usage_id, created_at, prompt_tokens, completion_tokens, cached_tokens) in rows {
        let billed_at = created_at
            .as_deref()
            .and_then(pricing::parse_usage_timestamp);
        let Some(price) = pricing::lookup_for_usage_at(conn, "", "", Some(combo_id), billed_at)
        else {
            continue;
        };
        let Some(cost) = pricing::compute_cost_opt_with_cache(
            Some(price),
            prompt_tokens.unwrap_or(0),
            completion_tokens.unwrap_or(0),
            cached_tokens,
        ) else {
            continue;
        };
        conn.execute(
            "UPDATE usage SET cost_usd = ?1 WHERE id = ?2",
            params![cost, usage_id],
        )
        .map_err(crate::error::map_db_error)?;
        updated += 1;
    }
    Ok(updated)
}

pub fn backfill_usage_pricing(conn: &Connection) -> openproxy_types::Result<usize> {
    // 1. Combo prices first: a combo that carries its own rate bills every row
    //    it routed, whatever the target models cost. A scheduled combo has no
    //    single rate, so its rows are re-priced one by one at their own instant.
    let combo_prices: Vec<(i64, Option<pricing::Price>, bool)> = {
        let mut stmt = conn
            .prepare(
                "SELECT id, pricing_input_per_1m, pricing_output_per_1m, pricing_schedule \
                 FROM combos \
                 WHERE (pricing_input_per_1m IS NOT NULL \
                        AND pricing_output_per_1m IS NOT NULL) \
                    OR pricing_schedule IS NOT NULL",
            )
            .map_err(crate::error::map_db_error)?;
        let rows = stmt
            .query_map([], |r| {
                let input: Option<f64> = r.get(1)?;
                let output: Option<f64> = r.get(2)?;
                let scheduled: Option<String> = r.get(3)?;
                let flat = match (input, output) {
                    (Some(input_per_1m), Some(output_per_1m)) => Some(pricing::Price {
                        input_per_1m,
                        output_per_1m,
                        ..Default::default()
                    }),
                    _ => None,
                };
                Ok((r.get::<_, i64>(0)?, flat, scheduled.is_some()))
            })
            .map_err(crate::error::map_db_error)?;
        rows.filter_map(std::result::Result::ok).collect()
    };
    let mut total_updated = 0;
    for (combo_id, flat, scheduled) in combo_prices {
        if scheduled {
            total_updated += backfill_scheduled_combo(conn, combo_id)?;
        } else if let Some(price) = flat {
            total_updated += update_backfill_combo_price(conn, combo_id, price)?;
        }
    }

    // 2. Everything else falls back to the target model's price.
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT provider_id, upstream_model_id \
             FROM usage \
             WHERE (prompt_tokens > 0 OR audio_ms > 0) \
               AND (cost_usd = 0.0 OR cost_usd IS NULL) \
               AND (combo_id IS NULL OR combo_id NOT IN \
                    (SELECT id FROM combos WHERE (pricing_input_per_1m IS NOT NULL \
                       AND pricing_output_per_1m IS NOT NULL) OR pricing_schedule IS NOT NULL))",
        )
        .map_err(crate::error::map_db_error)?;

    let pairs: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(crate::error::map_db_error)?
        .filter_map(std::result::Result::ok)
        .collect();

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
    let api_key_id: Option<i64> = r.get(31)?;
    let audio_ms: Option<u32> = r.get(32)?;
    let session_hash: Option<i64> = r.get(33)?;
    let charged_units: Option<i64> = r.get(34)?;
    let credits_debited: i64 = r.get(35)?;

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
        api_key_id,
        audio_ms,
        session_hash: session_hash.map(|v| v as u64),
        charged_units,
        credits_debited: credits_debited != 0,
    })
}

const RECENT_USAGE_COLS: &str = "id, request_id, trace_id, provider_id, upstream_model_id, \
    status_code, total_ms, prompt_tokens, completion_tokens, cost_usd, connect_ms, ttft_ms, \
    error_msg, race_total, race_attempts, is_streaming, stream_complete, race_lost, created_at, \
    stop_reason, compression_savings_pct, compression_techniques, client_response, \
    prompt_tokens_estimated, completion_tokens_estimated, endpoint_kind, proxy_url, \
    proxy_status, is_proxy_rotated, cached_tokens, pii_redacted, api_key_id, audio_ms, session_hash, \
    charged_units, credits_debited";

pub fn mark_client_response(conn: &Connection, row_id: UsageId) -> openproxy_types::Result<()> {
    crate::credits_billing::transaction(conn, |conn| {
        conn.execute(
            "UPDATE usage SET was_winner = 1, client_response = 1 WHERE id = ?1",
            params![row_id.0],
        )
        .map_err(crate::error::map_db_error_ctx("mark_client_response"))?;
        // The updated row (with its charged amount) is re-read and broadcast
        // below, so the debit result itself is not needed here.
        #[cfg(feature = "payments")]
        let _charged = crate::credits_billing::debit_usage(conn, row_id.0)?;
        #[cfg(not(feature = "payments"))]
        let _charged = crate::credits_billing::debit_usage(conn, row_id.0);
        Ok(())
    })?;

    if let Ok(mut stmt) = conn.prepare(&format!(
        "SELECT {RECENT_USAGE_COLS} FROM usage WHERE id = ?1"
    )) && let Ok(row) = stmt.query_row(params![row_id.0], map_recent_usage_from_row)
    {
        broadcast_usage_row(row);
    }
    Ok(())
}

pub fn mark_winner_usage_row(
    conn: &Connection,
    request_id: &str,
    attempt: u8,
    target_id: openproxy_types::ids::ComboTargetId,
) -> openproxy_types::Result<()> {
    crate::credits_billing::transaction(conn, |conn| {
        let mut stmt = conn
            .prepare(
                "UPDATE usage SET was_winner = 1, client_response = 1 \
             WHERE request_id = ?1 AND attempt = ?2 AND combo_target_id = ?3 RETURNING id",
            )
            .map_err(crate::error::map_db_error_ctx("mark_winner_usage_row"))?;
        let ids = stmt
            .query_map(params![request_id, attempt, target_id.0], |r| {
                r.get::<_, i64>(0)
            })
            .map_err(crate::error::map_db_error)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(crate::error::map_db_error)?;
        drop(stmt);
        for id in ids {
            #[cfg(feature = "payments")]
            let _charged = crate::credits_billing::debit_usage(conn, id)?;
            #[cfg(not(feature = "payments"))]
            let _charged = crate::credits_billing::debit_usage(conn, id);
        }
        Ok(())
    })?;

    if let Ok(mut stmt) = conn.prepare(&format!(
            "SELECT {RECENT_USAGE_COLS} FROM usage WHERE request_id = ?1 AND attempt = ?2 AND combo_target_id = ?3 ORDER BY id DESC LIMIT 1"
        ))
        && let Ok(row) = stmt.query_row(params![request_id, attempt, target_id.0], map_recent_usage_from_row)
    {
        broadcast_usage_row(row);
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
mod tests;
