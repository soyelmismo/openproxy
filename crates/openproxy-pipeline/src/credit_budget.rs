//! Strict monetary bound for prepaid chat requests, enforced **before** the
//! upstream is touched.
//!
//! The per-key credit gate serializes requests and refuses a balance that is
//! already exhausted, but a single request used to be admitted as long as
//! `balance > 0`: its cost is only known after the response, and the debit was
//! clamped to whatever was left (`debit = min(cost, balance)`). A prepaid key
//! could therefore consume more than it owned on every request.
//!
//! This module computes a conservative *maximum* cost for one candidate before
//! dispatch and rejects it with `402 InsufficientCredits` when that maximum does
//! not fit the authoritative balance. The rules are deliberately one-sided:
//!
//! - **Input** is bounded by the exact formatted payload size in bytes. A BPE
//!   token is a byte sequence of length >= 1, and JSON escaping only adds bytes,
//!   so `payload_bytes` is an upper bound on the prompt tokens the upstream can
//!   bill. `chars / 4` is never used: it is an estimate, not a bound.
//! - **Output** comes from the client's explicit `max_tokens` /
//!   `max_completion_tokens`, or the model/combo declared ceiling when the
//!   client stayed silent. `n` / `best_of` multiply that ceiling. When the
//!   model declares no ceiling and output is not free, the request has no
//!   verifiable bound and is rejected (fail closed). A reasoning request also
//!   requires the model-declared ceiling because `max_tokens` does not
//!   necessarily bound hidden reasoning tokens.
//! - **Cached** input is priced at `max(input_rate, cached_rate)`: no
//!   percentage is ever derived, and a cached rate that is higher than the
//!   input rate still cannot make the bound smaller than the real cost.
//! - **Unknown modalities and unknown prices** are rejected. Image/audio/video
//!   parts bill in units this module cannot bound from token rates, and a
//!   missing or non-finite rate has no verifiable cost at all.
//! - Free keys (no ledger rows) never enter this path and keep full
//!   concurrency.
//!
//! Streaming adds a second requirement: a crash between the first client-visible
//! chunk and the usage row must not look like "nothing was delivered". After
//! each delivered frame, [`CreditStreamGuard`] checkpoint the text bytes already
//! handed to the client into `credit_reservations`; the reconciliation policy in
//! `openproxy_db::reservations` charges that **delivered-cost checkpoint**, never
//! the full admitted maximum and never output the model did not produce. The
//! real usage row, when it lands, remains the authoritative charge; the
//! checkpoint only covers a crash where that row was lost.

use crate::context::ResolvedTarget;
use crate::{Pipeline, PipelineRequest};
use openproxy_db::pricing::{self, Price, PriceKind};
use openproxy_types::OpenAIRequest;
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::error::CoreError;
use openproxy_types::models::Model;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// One USD is one million credit units, matching the credits ledger.
pub const UNITS_PER_USD: f64 = 1_000_000.0;

/// Attempt number of the single admission the gate performs per client
/// request. The dispatcher may retry upstream targets, but those retries share
/// this one durable intent.
const ADMISSION_ATTEMPT: u8 = 1;

/// Conservative upper bound on what one dispatch may cost.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CreditQuote {
    /// Maximum cost in micro-USDC units, rounded **up** and with one unit of
    /// headroom so f64 rounding can never understate the ledger's rounded debit.
    pub units: i64,
    /// Prompt-token upper bound (payload bytes).
    pub prompt_tokens: u64,
    /// Completion-token upper bound (explicit/model ceiling times `n`).
    pub output_tokens: u64,
    /// Cost of the prompt alone in micro-USDC units (used as the floor of a
    /// delivered-bytes streaming checkpoint).
    pub input_units: i64,
    /// Output rate in USD per million tokens, kept so a streaming checkpoint
    /// can price only the bytes already handed to the client.
    pub output_per_1m: f64,
}

impl CreditQuote {
    #[must_use]
    pub fn usd(self) -> f64 {
        self.units as f64 / UNITS_PER_USD
    }
}

struct Rates {
    input_per_1m: f64,
    output_per_1m: f64,
    cached_per_1m: f64,
}

fn unverifiable(reason: impl Into<String>) -> CoreError {
    CoreError::Internal(format!(
        "prepaid budget: cannot verify the maximum cost: {}",
        reason.into()
    ))
}

fn finite_rate(name: &str, rate: f64) -> Result<f64, CoreError> {
    if rate.is_finite() && rate >= 0.0 {
        Ok(rate)
    } else {
        Err(unverifiable(format!(
            "{name} rate is not a finite non-negative USD value ({rate})"
        )))
    }
}

fn rates(price: Price) -> Result<Rates, CoreError> {
    match price.kind {
        // Token math below does not describe how these bill. A chat request
        // routed to them has no verifiable token bound.
        PriceKind::Audio | PriceKind::Image | PriceKind::Video => {
            return Err(unverifiable(format!(
                "price kind {:?} is not billable by chat token rates",
                price.kind
            )));
        }
        PriceKind::Chat | PriceKind::Other => {}
    }
    let input_per_1m = finite_rate("input", price.input_per_1m)?;
    let output_per_1m = finite_rate("output", price.output_per_1m)?;
    let cached_per_1m = finite_rate("cached", price.cached_per_1m.unwrap_or(input_per_1m))?;
    Ok(Rates {
        input_per_1m,
        output_per_1m,
        cached_per_1m,
    })
}

fn json_u64(value: Option<&serde_json::Value>) -> Option<u64> {
    let value = value?;
    value
        .as_u64()
        .or_else(|| value.as_i64().filter(|v| *v >= 0).map(|v| v as u64))
        .or_else(|| {
            value
                .as_f64()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .map(|v| v.floor() as u64)
        })
}

/// The client's explicit per-completion output ceiling, across every dialect
/// the chat endpoint accepts.
fn explicit_output_limit(req: &OpenAIRequest) -> Option<u64> {
    req.max_tokens
        .map(u64::from)
        .or_else(|| json_u64(req.extra.get("max_completion_tokens")))
        .or_else(|| json_u64(req.extra.get("max_output_tokens")))
        .or_else(|| json_u64(req.extra.get("max_tokens")))
}

/// Number of choices the upstream may bill in one response.
fn completion_multiplier(req: &OpenAIRequest) -> Result<u64, CoreError> {
    let n = json_u64(req.extra.get("n")).unwrap_or(1).max(1);
    let best_of = json_u64(req.extra.get("best_of")).unwrap_or(1).max(1);
    let num_choices = json_u64(req.extra.get("num_choices")).unwrap_or(1).max(1);
    n.checked_mul(best_of)
        .and_then(|v| v.checked_mul(num_choices))
        .filter(|v| *v > 0)
        .ok_or_else(|| unverifiable("choice multiplier overflows"))
}

fn requests_reasoning(req: &OpenAIRequest, target: &ComboTarget) -> bool {
    if target.thinking_effort.is_some() {
        return true;
    }
    [
        "reasoning_effort",
        "reasoning",
        "thinking",
        "thinking_budget",
        "thinking_effort",
        "enable_thinking",
    ]
    .iter()
    .any(|key| req.extra.contains_key(*key))
}

/// Reject content whose cost cannot be expressed as input/output tokens.
fn ensure_text_only(req: &OpenAIRequest) -> Result<(), CoreError> {
    for message in &req.messages {
        let Some(content) = message.content.as_ref() else {
            continue;
        };
        match content {
            serde_json::Value::String(_) | serde_json::Value::Null => {}
            serde_json::Value::Array(parts) => {
                for part in parts {
                    ensure_text_part(part)?;
                }
            }
            other => {
                return Err(unverifiable(format!(
                    "message content is neither text nor a parts array: {other}"
                )));
            }
        }
    }
    for key in req.extra.keys() {
        if matches!(
            key.as_str(),
            "audio" | "input_audio" | "image" | "images" | "video" | "videos" | "file" | "files"
        ) {
            return Err(unverifiable(format!(
                "request carries an `{key}` field with modality-specific pricing"
            )));
        }
        if key == "modalities" {
            let text_only = req
                .extra
                .get(key)
                .and_then(serde_json::Value::as_array)
                .is_some_and(|values| {
                    values
                        .iter()
                        .all(|v| v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("text")))
                });
            if !text_only {
                return Err(unverifiable(
                    "request asks for non-text output modalities".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn ensure_text_part(part: &serde_json::Value) -> Result<(), CoreError> {
    let media = part
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|kind| {
            matches!(
                kind,
                "image_url"
                    | "input_image"
                    | "image"
                    | "input_audio"
                    | "audio"
                    | "input_video"
                    | "video_url"
                    | "video"
                    | "file"
                    | "input_file"
                    | "document"
            )
        })
        || part.get("image_url").is_some()
        || part.get("video_url").is_some()
        || part.get("input_audio").is_some()
        || part.get("file").is_some();
    if media {
        return Err(unverifiable(
            "request contains an image/audio/video/file part".to_string(),
        ));
    }
    if let Some(kind) = part.get("type").and_then(serde_json::Value::as_str)
        && !matches!(kind, "text" | "input_text" | "thinking" | "reasoning")
    {
        return Err(unverifiable(format!("unknown content part type `{kind}`")));
    }
    Ok(())
}

/// Build the conservative quote for one candidate.
///
/// `payload_bytes` must be the length of the exact formatted body sent upstream
/// (after prefix injection, wrapping and provider parameter stripping), not the
/// client's original JSON.
pub fn quote(
    req: &PipelineRequest,
    combo: &Combo,
    target: &ComboTarget,
    model: &Model,
    payload_bytes: usize,
    price: Option<Price>,
) -> Result<CreditQuote, CoreError> {
    ensure_text_only(&req.openai_request)?;
    let price = price.ok_or_else(|| {
        unverifiable(format!(
            "no price for provider={} model={}",
            target.provider_id.as_str(),
            model.model_id.as_str()
        ))
    })?;
    let rates = rates(price)?;

    let policy = crate::policy::EffectiveTargetPolicy::resolve(Some(combo), target, model);
    let declared_output = policy.max_output_tokens.map(u64::from);
    let explicit_output = explicit_output_limit(&req.openai_request);
    let reasoning = requests_reasoning(&req.openai_request, target);

    let per_choice_output = if rates.output_per_1m == 0.0 {
        // Output is free at this rate; no ceiling is needed to bound its cost.
        0
    } else if reasoning {
        // `max_tokens` is not guaranteed to bound hidden reasoning tokens;
        // only the model-declared ceiling is a verifiable total.
        let declared = declared_output.ok_or_else(|| {
            unverifiable("reasoning request has no model-declared output ceiling")
        })?;
        explicit_output.map_or(declared, |explicit| explicit.max(declared))
    } else {
        explicit_output.or(declared_output).ok_or_else(|| {
            unverifiable(
                "request has no explicit max_tokens and the model declares no output ceiling",
            )
        })?
    };
    let output_tokens = per_choice_output
        .checked_mul(completion_multiplier(&req.openai_request)?)
        .ok_or_else(|| unverifiable("output token bound overflows"))?;

    let prompt_tokens = u64::try_from(payload_bytes).unwrap_or(u64::MAX);
    let input_rate = rates.input_per_1m.max(rates.cached_per_1m);
    let input_cost_usd = input_rate * prompt_tokens as f64 / UNITS_PER_USD;
    let output_cost_usd = rates.output_per_1m * output_tokens as f64 / UNITS_PER_USD;
    let cost_usd = input_cost_usd + output_cost_usd;
    if !cost_usd.is_finite() || cost_usd < 0.0 {
        return Err(unverifiable(format!(
            "computed maximum cost is not finite ({cost_usd})"
        )));
    }
    let rounded_up = (cost_usd * UNITS_PER_USD).ceil();
    if !rounded_up.is_finite() || rounded_up > i64::MAX as f64 {
        return Err(unverifiable(
            "maximum cost exceeds the ledger's integer range".to_string(),
        ));
    }
    let input_units = if input_cost_usd > 0.0 {
        ((input_cost_usd * UNITS_PER_USD).ceil() as i64).saturating_add(1)
    } else {
        0
    };
    let units = if cost_usd > 0.0 {
        (rounded_up as i64).saturating_add(1)
    } else {
        0
    };
    Ok(CreditQuote {
        units,
        prompt_tokens,
        output_tokens,
        input_units,
        output_per_1m: rates.output_per_1m,
    })
}

struct CreditState {
    governed: bool,
    balance_units: i64,
    price: Option<Price>,
}

fn load_credit_state(
    conn: &rusqlite::Connection,
    key_id: i64,
    provider: &str,
    model_id: &str,
    combo_id: i64,
) -> openproxy_types::Result<CreditState> {
    if !openproxy_db::credits_ledger::is_credit_based(conn, key_id)? {
        return Ok(CreditState {
            governed: false,
            balance_units: 0,
            price: None,
        });
    }
    let balance_units = openproxy_db::credits_ledger::balance_units(conn, key_id)?;
    let price = pricing::lookup_for_request(conn, provider, model_id, Some(combo_id));
    Ok(CreditState {
        governed: true,
        balance_units,
        price,
    })
}

fn deny_if_over_budget(quote: &CreditQuote, balance_units: i64) -> Result<(), CoreError> {
    if quote.units > balance_units {
        return Err(CoreError::InsufficientCredits {
            balance_usd: balance_units as f64 / UNITS_PER_USD,
        });
    }
    Ok(())
}

/// Pre-dispatch affordability check for one resolved candidate.
///
/// `Ok(None)` means the key is not governed by prepaid credit (free/unlimited)
/// and must not be serialized or bounded. `Ok(Some(quote))` means the maximum
/// cost fits the authoritative balance. Every other outcome is an error and the
/// caller must not dispatch.
///
/// The balance read is authoritative because the caller invokes this while the
/// key's admission lane is already held (`Pipeline::run` /
/// `execute_test_target`); the journal was drained by admission.
pub async fn check_prepaid_dispatch(
    pipeline: &Pipeline,
    req: &PipelineRequest,
    combo: &Combo,
    target: &ComboTarget,
    model: &Model,
    payload_bytes: usize,
) -> Result<Option<CreditQuote>, CoreError> {
    let Some(key_id) = req.api_key_id else {
        return Ok(None);
    };
    let key = key_id.0;
    let provider = target.provider_id.as_str().to_string();
    let model_id = model.model_id.as_str().to_string();
    let combo_id = combo.id.0;
    let state = pipeline
        .read_db(move |conn| load_credit_state(conn, key, &provider, &model_id, combo_id))
        .await?;
    if !state.governed {
        return Ok(None);
    }
    let quote = quote(req, combo, target, model, payload_bytes, state.price)?;
    deny_if_over_budget(&quote, state.balance_units)?;
    Ok(Some(quote))
}

/// Re-check affordability and return the guard that durably records the quote
/// and the first client-visible chunk.
///
/// Called by the streaming dispatcher after a 2xx response and before anything
/// reaches the client. The DB write itself is deferred to the first actual
/// chunk (`StreamSink`), so a stream that fails while empty does not fabricate
/// delivery evidence.
pub async fn stream_guard(
    conn: Arc<parking_lot::Mutex<rusqlite::Connection>>,
    req: &PipelineRequest,
    combo: &Combo,
    target: &ComboTarget,
    model: &Model,
    payload_bytes: usize,
) -> Result<Option<Arc<CreditStreamGuard>>, CoreError> {
    let Some(key_id) = req.api_key_id else {
        return Ok(None);
    };
    let key = key_id.0;
    let provider = target.provider_id.as_str().to_string();
    let model_id = model.model_id.as_str().to_string();
    let combo_id = combo.id.0;
    let reader = Arc::clone(&conn);
    let state = tokio::task::spawn_blocking(move || {
        let conn = reader.lock();
        load_credit_state(&conn, key, &provider, &model_id, combo_id)
    })
    .await
    .map_err(|error| {
        CoreError::Internal(format!(
            "prepaid budget: credit state task failed to run: {error}"
        ))
    })??;
    if !state.governed {
        return Ok(None);
    }
    let quote = quote(req, combo, target, model, payload_bytes, state.price)?;
    deny_if_over_budget(&quote, state.balance_units)?;
    Ok(Some(Arc::new(CreditStreamGuard {
        conn,
        api_key_id: key,
        request_id: req.request_id.to_string(),
        // The gate's admission is a single one per client request and always
        // writes attempt 1, even when the dispatcher retries a target before
        // delivering a chunk.
        attempt: ADMISSION_ATTEMPT,
        max_units: quote.units,
        input_units: quote.input_units,
        output_per_1m: quote.output_per_1m,
        delivered_bytes: AtomicU64::new(0),
        checkpointed_bytes: AtomicU64::new(0),
    })))
}

/// Minimum newly delivered text before the next durable checkpoint. The first
/// chunk always checkpoints, so even a tiny partial response has durable cost
/// evidence; the threshold only coalesces the rest.
const CHECKPOINT_EVERY_BYTES: u64 = 512;

/// Sum of the text bytes the client actually received in one sink frame.
///
/// Token count can never exceed UTF-8 byte count, so this is a conservative
/// upper bound on the output tokens produced so far (reasoning and tool
/// arguments included). SSE framing and JSON keys are excluded; an unparseable
/// frame falls back to its wire length, which is still an upper bound.
fn delivered_text_bytes(frame: &[u8]) -> u64 {
    let Ok(text) = std::str::from_utf8(frame) else {
        return frame.len() as u64;
    };
    let mut total = 0u64;
    let mut parsed = false;
    for line in text.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(payload) else {
            continue;
        };
        parsed = true;
        let Some(choices) = value.get("choices").and_then(serde_json::Value::as_array) else {
            continue;
        };
        for choice in choices {
            for container in ["delta", "message"] {
                let Some(object) = choice.get(container) else {
                    continue;
                };
                for field in ["content", "reasoning_content", "reasoning"] {
                    if let Some(text) = object.get(field).and_then(serde_json::Value::as_str) {
                        total = total.saturating_add(text.len() as u64);
                    }
                }
                if let Some(calls) = object
                    .get("tool_calls")
                    .and_then(serde_json::Value::as_array)
                {
                    for call in calls {
                        if let Some(arguments) = call
                            .pointer("/function/arguments")
                            .and_then(serde_json::Value::as_str)
                        {
                            total = total.saturating_add(arguments.len() as u64);
                        }
                    }
                }
            }
        }
    }
    if parsed { total } else { frame.len() as u64 }
}

/// Durable, incremental cost evidence for a prepaid stream.
///
/// The admission intent starts with the balance-wide ceiling. After each
/// checkpointed chunk this guard records, in `generated_units`, the cost of the
/// text already handed to the client, capped by the admitted maximum. A crash
/// therefore charges for produced output only — never for output tokens the
/// model never generated. The checkpoint is written after the chunk was
/// enqueued, so a marker cannot invent a delivery that did not happen.
pub struct CreditStreamGuard {
    conn: Arc<parking_lot::Mutex<rusqlite::Connection>>,
    api_key_id: i64,
    request_id: String,
    attempt: u8,
    /// Full admitted maximum, used only to cap the delivered-cost estimate.
    max_units: i64,
    /// Prompt cost: always owed once anything was delivered.
    input_units: i64,
    output_per_1m: f64,
    delivered_bytes: AtomicU64,
    checkpointed_bytes: AtomicU64,
}

impl std::fmt::Debug for CreditStreamGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CreditStreamGuard")
            .field("api_key_id", &self.api_key_id)
            .field("attempt", &self.attempt)
            .field("max_units", &self.max_units)
            .field(
                "delivered_bytes",
                &self.delivered_bytes.load(Ordering::Relaxed),
            )
            .field(
                "checkpointed_bytes",
                &self.checkpointed_bytes.load(Ordering::Relaxed),
            )
            .finish()
    }
}

impl CreditStreamGuard {
    /// Cost currently recorded for the delivered bytes, in micro-USDC units.
    #[must_use]
    pub fn reserved_units(&self) -> i64 {
        self.checkpoint_units(self.checkpointed_bytes.load(Ordering::Relaxed))
    }

    /// Conservative cost of `delivered_bytes` of produced text. The prompt is
    /// charged once; output bytes are an upper bound on output tokens; the
    /// result is capped by the admitted maximum.
    fn checkpoint_units(&self, delivered_bytes: u64) -> i64 {
        if delivered_bytes == 0 {
            return 0;
        }
        let output_units = (self.output_per_1m * delivered_bytes as f64 / UNITS_PER_USD).ceil();
        let units = self
            .input_units
            .saturating_add(if output_units.is_finite() && output_units > 0.0 {
                output_units as i64
            } else {
                0
            })
            .saturating_add(1)
            .min(self.max_units);
        units.max(1)
    }

    /// Account one frame **after** it was enqueued for the client, checkpointing
    /// when enough new text accumulated or on the first delivered frame.
    pub async fn record_delivered_frame(&self, frame: &[u8]) -> Result<(), CoreError> {
        let delta = delivered_text_bytes(frame);
        if delta == 0 {
            return Ok(());
        }
        let total = self
            .delivered_bytes
            .fetch_add(delta, Ordering::AcqRel)
            .saturating_add(delta);
        let checkpointed = self.checkpointed_bytes.load(Ordering::Acquire);
        if checkpointed != 0 && total.saturating_sub(checkpointed) < CHECKPOINT_EVERY_BYTES {
            return Ok(());
        }
        self.persist_checkpoint(total).await
    }

    /// Persist the last bytes delivered before the stream is reported complete.
    pub async fn flush_delivery(&self) -> Result<(), CoreError> {
        let total = self.delivered_bytes.load(Ordering::Acquire);
        if total == 0 || total <= self.checkpointed_bytes.load(Ordering::Acquire) {
            return Ok(());
        }
        self.persist_checkpoint(total).await
    }

    async fn persist_checkpoint(&self, delivered_bytes: u64) -> Result<(), CoreError> {
        let conn = Arc::clone(&self.conn);
        let key = self.api_key_id;
        let request_id = self.request_id.clone();
        let attempt = self.attempt;
        let units = self.checkpoint_units(delivered_bytes);
        let result = tokio::task::spawn_blocking(move || {
            let conn = conn.lock();
            let updated =
                openproxy_db::reservations::checkpoint(&conn, key, &request_id, attempt, units)?;
            if !updated {
                return Err(CoreError::Internal(
                    "prepaid budget: the request intent is no longer open".into(),
                ));
            }
            Ok::<_, CoreError>(())
        })
        .await;
        match result {
            Ok(Ok(())) => {
                self.checkpointed_bytes
                    .store(delivered_bytes, Ordering::Release);
                Ok(())
            }
            Ok(Err(error)) => Err(openproxy_db::reservations::hold_blocked(&error)),
            Err(error) => Err(CoreError::Internal(format!(
                "prepaid budget: delivery evidence task failed to run: {error}"
            ))),
        }
    }
}

/// Convenience wrapper for callers holding a [`ResolvedTarget`].
pub async fn check_resolved(
    pipeline: &Pipeline,
    req: &PipelineRequest,
    combo: &Combo,
    resolved: &ResolvedTarget,
    payload_bytes: usize,
) -> Result<Option<CreditQuote>, CoreError> {
    check_prepaid_dispatch(
        pipeline,
        req,
        combo,
        &resolved.target,
        &resolved.model,
        payload_bytes,
    )
    .await
}

#[cfg(test)]
#[path = "credit_budget_tests.rs"]
mod tests;

// Credit-backed lifecycle fixtures: payments-only.
#[cfg(all(test, feature = "payments"))]
#[path = "credit_budget_lifecycle_tests.rs"]
mod lifecycle_tests;
