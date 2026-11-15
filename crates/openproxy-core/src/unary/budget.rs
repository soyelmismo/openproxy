//! Pre-dispatch cost bounds for the non-chat executors.
//!
//! [`UnaryCreditLease`](super::UnaryCreditLease) serializes one prepaid key's
//! requests, but serialization alone does not bound a *single* request: the
//! cost was only known after the upstream answered, and
//! `credits_billing::debit_usage` clamps the debit to whatever balance is left.
//! A request priced above the remaining balance therefore dispatched anyway and
//! spent the balance down to zero — a free request for the customer.
//!
//! This module prices the request **before** it is dispatched, from quantities
//! derived from the client's own input, and never from the upstream answer:
//!
//! - tokens: the byte length of the textual payload. Byte-level BPE (cl100k,
//!   and every byte-fallback tokenizer) has a token for each byte, so the byte
//!   count is a provable upper bound, not a heuristic. Pre-tokenized arrays
//!   carry their exact count instead.
//! - images: the requested image count (`n`), which is also the quantity a
//!   per-image tariff multiplies.
//! - audio: the container duration probed from the submitted bytes *before*
//!   dispatch. An unreadable container has no billable quantity, so a prepaid
//!   request is refused instead of being dispatched unbilled.
//!
//! What is deliberately **not** bounded, and is therefore refused instead of
//! guessed:
//!
//! - media parts of an embeddings request (image/audio/video/document URLs).
//!   Their token cost is a property of the *remote* resource, not of the URL
//!   string that references it, so no client-side quantity can cap it.
//! - a completion slot with a non-zero output tariff and no ceiling on record
//!   (the model row's `max_output_tokens`, or the shared model-id catalog).
//! - an image endpoint without a per-image ([`PriceKind::Image`]) tariff: a
//!   token rate applied to an image count is a fiction, not a price.
//!
//! Every non-prepaid key never reaches this module: free/unlimited keys keep
//! their current behaviour and their concurrency, and missing pricing never
//! blocks them.
//!
//! # Settlement
//!
//! [`BudgetQuote::apply_quote`] fills the quantities the upstream omitted from
//! the bound that admission priced, so an upstream that reports no `usage`
//! block does not become a free request. It does **not** clamp a reported count
//! down to the bound: a response that cost more than admission authorized is a
//! real overrun, and hiding it in the ledger would only make it invisible. The
//! reported quantity is kept as evidence (with an error log) and the ledger's
//! own debit clamp is the last line of defence.

use openproxy_db::DbPool;
use openproxy_db::pricing::{self, PriceKind};
use openproxy_types::{
    CoreError, Result,
    embeddings::{EmbeddingInput, EmbeddingItem, EmbeddingPart, EmbeddingRequest},
    ids::ComboId,
    systemone::SystemOneRequest,
};

use super::UnaryTarget;

/// Micro-USDC units in one USD: the ledger's own unit
/// (`openproxy_db::credits_ledger`).
pub const UNITS_PER_USD: i64 = 1_000_000;

/// Framing overhead billed per input item on top of its raw payload.
const ITEM_FRAMING_TOKENS: u32 = 8;

/// Providers whose models execute inside this process (the native `laya` and
/// `embeddinggemma` engines). Used only to validate a [`HostLocal`] claim —
/// never as a pricing decision by itself.
const HOST_LOCAL_ENGINES: &[&str] = &["laya", "embeddinggemma"];

/// Proof that a target is served on this host, so no metered upstream is
/// contacted and no upstream cost is incurred.
///
/// There is no `HostLocal::new`: the only constructor takes the decision the
/// dispatcher itself makes (the predicate that skips the adapter and calls the
/// local engine) and re-checks it. A provider *name* alone is not evidence —
/// an operator can point `laya` at a remote base URL, and a request that says
/// "local" while the adapter would still be called must never be admitted for
/// free.
#[derive(Clone, Copy, Debug)]
pub struct HostLocal<'a> {
    provider: &'a str,
}

impl<'a> HostLocal<'a> {
    /// Confirm a host-local dispatch decision for `target`.
    ///
    /// `served_on_host` must be the result of the same predicate the executor
    /// uses to short-circuit the adapter (engine compiled in, engine enabled,
    /// and this exact target selected). Returns `None` when the request is not
    /// host-local, or when the provider is not a known local engine — the
    /// caller then goes through the normal priced path (and fails closed).
    #[must_use]
    pub fn confirm(target: &'a UnaryTarget, served_on_host: bool) -> Option<Self> {
        if !served_on_host {
            return None;
        }
        let provider = target.provider.as_str();
        HOST_LOCAL_ENGINES
            .contains(&provider)
            .then_some(Self { provider })
    }

    #[must_use]
    pub fn provider(&self) -> &'a str {
        self.provider
    }
}

/// The quantities one request can be billed for, bounded from client input.
#[derive(Clone, Copy, Debug)]
pub enum UnaryCharge<'a> {
    /// An embeddings request: every text/token array it carries.
    Embeddings(&'a EmbeddingRequest),
    /// A SystemOne request: its state plus its questions.
    SystemOne(&'a SystemOneRequest),
    /// Speech-to-text: the probed container duration. `None` means the probe
    /// could not read the container, which is not billable.
    Audio { duration_ms: Option<u32> },
    /// Image generation/edit/variation: the requested image count. `None`
    /// means the request carries no parseable count.
    Image { images: Option<u32> },
}

/// Upper bound of the billable quantities of one request, with their price.
#[derive(Clone, Copy, Debug)]
pub struct BudgetQuote {
    units: i64,
    prompt_tokens: Option<u32>,
    completion_tokens: Option<u32>,
    audio_ms: Option<u32>,
}

impl BudgetQuote {
    /// The most this request can cost, in micro-USDC.
    #[must_use]
    pub const fn units(&self) -> i64 {
        self.units
    }

    /// Token quantity to persist when the upstream omits `usage`.
    #[must_use]
    pub const fn prompt_tokens(&self) -> Option<u32> {
        self.prompt_tokens
    }

    /// Token quantity to persist when the upstream omits `usage`.
    #[must_use]
    pub const fn completion_tokens(&self) -> Option<u32> {
        self.completion_tokens
    }

    /// Audio duration to persist when the upstream omits `usage`.
    #[must_use]
    pub const fn audio_ms(&self) -> Option<u32> {
        self.audio_ms
    }

    /// Fill the quantities the upstream omitted, and report (without hiding)
    /// the ones that exceeded what admission priced.
    ///
    /// - absent or zero: the bound is written, because the request certainly
    ///   cost at least the quantity admission authorized — an upstream that
    ///   omits `usage` is not a free request.
    /// - above the bound: the reported value is kept. This is a real overrun;
    ///   writing the bound instead would understate the charge and erase the
    ///   evidence, and the ledger's own clamp is what protects the balance.
    pub fn apply_quote(&self, args: &mut super::UnaryUsageArgs<'_>) {
        if let Some(bound) = self.audio_ms {
            let reported = args.audio_ms;
            args.audio_ms = Some(fill_missing(reported, bound));
            report_overrun("audio_ms", reported, bound);
            return;
        }

        if let Some(bound) = self.prompt_tokens {
            let reported = args.prompt_tokens;
            args.prompt_tokens = Some(fill_missing(reported, bound));
            report_overrun("prompt_tokens", reported, bound);
        }
        if let Some(bound) = self.completion_tokens {
            let reported = args.completion_tokens;
            args.completion_tokens = Some(fill_missing(reported, bound));
            report_overrun("completion_tokens", reported, bound);
        }
    }
}

/// A reported count is only usable when it is neither absent nor zero
/// (protocols use both for "omitted"); anything else is the client's own
/// quantity, which is what the row must record.
const fn fill_missing(reported: Option<u32>, bound: u32) -> u32 {
    match reported {
        Some(value) if value > 0 => value,
        _ => bound,
    }
}

fn report_overrun(quantity: &str, reported: Option<u32>, bound: u32) {
    if let Some(value) = reported.filter(|value| *value > bound) {
        tracing::error!(
            quantity = %quantity,
            reported = value,
            admitted_bound = bound,
            "unary usage exceeded the admitted bound; keeping the reported quantity as evidence"
        );
    }
}

/// Price one request before dispatch and return the bound it was admitted
/// under.
///
/// The price is the **conservative** one: for a combo with a peak/off-peak
/// schedule it is the element-wise maximum of both bands, because the request
/// may start in one band and be billed in the other. The usage row is later
/// billed at the band of its own instant, which is never above this bound.
///
/// `local` is a [`HostLocal`] proof: the request will be served by an engine in
/// this process and cannot reach a metered upstream. Such a request still gets
/// its quantities bounded (for reporting), but no price is required — unless
/// one exists, in which case it is applied like any other.
///
/// # Errors
/// - [`CoreError::ServiceUnavailable`] when a prepaid request has no price, a
///   price whose unit cannot bill this endpoint (an hourly audio tariff on a
///   token endpoint, a token tariff on audio, a token tariff on images), a
///   media part whose remote cost cannot be bounded, or a priced output slot
///   with no ceiling on record.
/// - [`CoreError::Validation`] when the billable quantity cannot be derived
///   (unreadable audio container, missing image count).
/// - [`CoreError::Internal`] when the pricing store cannot be read or the cost
///   math does not produce a usable number. A failed lookup is never treated
///   as "no price".
pub async fn quote(
    db_pool: &DbPool,
    target: &UnaryTarget,
    charge: UnaryCharge<'_>,
    local: Option<HostLocal<'_>>,
) -> Result<BudgetQuote> {
    let quantities = Quantities::from_charge(charge)?;
    let provider = target.provider.as_str().to_string();
    let model = target.upstream_model.clone();
    let combo_id = target.combo_id.map(|id: ComboId| id.0);
    let model_row_id = target.model_row_id;
    let local = validate_local(local, &provider)?;

    db_pool
        .spawn_read(move |conn| {
            // A lookup that fails must not look like "unpriced": `pricing`
            // swallows database errors into `None`, which is indistinguishable
            // from a genuinely absent price. Probe the store first so a broken
            // read is an explicit failure instead of a free (or refused)
            // request based on missing evidence.
            pricing_store_readable(conn)?;

            let price =
                match pricing::lookup_for_request_conservative(conn, &provider, &model, combo_id) {
                    Some(price) => price,
                    None if local => {
                        // Host-local execution has no metered upstream cost, so
                        // there is nothing to bill; the quantities are still
                        // recorded so the row is not empty.
                        tracing::debug!(
                            provider = %provider,
                            model = %model,
                            "host-local target has no price: admitted at zero units"
                        );
                        return Ok(quantities.free_quote());
                    }
                    None => return Err(unpriced(&provider, &model)),
                };

            quantities.check_price_kind(price.kind, &provider, &model)?;
            let prompt_slot = quantities.prompt_slot(&provider, &model)?;
            let completion_slot = quantities.completion_slot(
                conn,
                model_row_id,
                &model,
                price.output_per_1m > 0.0,
                &provider,
            )?;

            let usd = pricing::compute_cost_opt_with_cache(
                Some(price),
                prompt_slot,
                completion_slot,
                None,
            )
            .ok_or_else(|| {
                CoreError::Internal(format!(
                    "cost math produced nothing for a priced request ({provider}/{model}); \
                     refusing to dispatch at an assumed price"
                ))
            })?;

            Ok(quantities.quote(usd_to_units_ceil(usd)?, completion_slot))
        })
        .await
}

/// Reject a [`HostLocal`] claim that does not match the target it is used for.
fn validate_local(local: Option<HostLocal<'_>>, provider: &str) -> Result<bool> {
    match local {
        None => Ok(false),
        Some(local) if local.provider() == provider => Ok(true),
        Some(local) => Err(CoreError::Internal(format!(
            "host-local execution was claimed for provider '{}' but the target is '{provider}': \
             refusing to price a metered upstream at zero",
            local.provider()
        ))),
    }
}

/// Quantities resolved from client input, before any pricing is known.
#[derive(Clone, Copy, Debug)]
enum Quantities {
    Tokens {
        prompt: u32,
        /// How the completion slot is priced: not at all (embeddings), or by
        /// the model's own ceiling (SystemOne).
        completion: Completion,
    },
    Audio {
        duration_ms: Option<u32>,
    },
    Images {
        count: Option<u32>,
    },
}

/// How the output side of a token-billed request is bounded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Completion {
    /// The endpoint produces no billed output (embeddings): the slot is zero
    /// and the usage row keeps `completion_tokens` unset.
    Absent,
    /// The endpoint produces output that is billed per token: the slot is the
    /// model's ceiling, which must exist.
    ModelCeiling,
}

impl Quantities {
    fn from_charge(charge: UnaryCharge<'_>) -> Result<Self> {
        match charge {
            UnaryCharge::Embeddings(req) => Ok(Self::Tokens {
                prompt: embedding_prompt_bound(&req.input)?,
                completion: Completion::Absent,
            }),
            UnaryCharge::SystemOne(req) => Ok(Self::Tokens {
                prompt: system_one_prompt_bound(req)?,
                completion: Completion::ModelCeiling,
            }),
            UnaryCharge::Audio { duration_ms } => Ok(Self::Audio { duration_ms }),
            UnaryCharge::Image { images } => Ok(Self::Images { count: images }),
        }
    }

    /// A tariff and an endpoint must agree on the unit being billed.
    fn check_price_kind(&self, kind: PriceKind, provider: &str, model: &str) -> Result<()> {
        match (self, kind) {
            // Audio bills per hour of audio; only that unit can price it.
            (Self::Audio { .. }, PriceKind::Audio) => Ok(()),
            (Self::Audio { .. }, _) => Err(CoreError::ServiceUnavailable(format!(
                "prepaid pricing for {provider}/{model} is not an hourly audio tariff; \
                 speech-to-text is refused instead of dispatched at zero cost"
            ))),
            // Images bill per image. A per-token rate times an image count is a
            // number with no contract behind it, so it is not a price.
            (Self::Images { .. }, PriceKind::Image) => Ok(()),
            (Self::Images { .. }, _) => Err(CoreError::ServiceUnavailable(format!(
                "prepaid pricing for {provider}/{model} is not a per-image tariff \
                 (found {kind:?}); the image request is refused instead of being billed \
                 with a rate that does not describe an image"
            ))),
            // Token-shaped endpoints bill per token.
            (Self::Tokens { .. }, PriceKind::Chat | PriceKind::Other) => Ok(()),
            (Self::Tokens { .. }, _) => Err(CoreError::ServiceUnavailable(format!(
                "prepaid pricing for {provider}/{model} is a {kind:?} tariff, which cannot \
                 bill a per-token endpoint; the request is refused instead of dispatched \
                 at zero cost"
            ))),
        }
    }

    /// The quantity that occupies the `prompt_tokens` slot on the usage row,
    /// matching what `openproxy_db::cost::record_row` bills.
    fn prompt_slot(&self, provider: &str, model: &str) -> Result<u32> {
        match *self {
            Self::Tokens { prompt, .. } => Ok(prompt),
            Self::Audio { duration_ms } => {
                duration_ms.ok_or_else(|| unknown_audio(provider, model))
            }
            Self::Images { count } => count.ok_or_else(|| unknown_images(provider, model)),
        }
    }

    /// The quantity that occupies the `completion_tokens` slot, or zero when
    /// the endpoint bills no output.
    fn completion_slot(
        &self,
        conn: &rusqlite::Connection,
        model_row_id: Option<openproxy_types::ids::ModelRowId>,
        model: &str,
        output_priced: bool,
        provider: &str,
    ) -> Result<u32> {
        match *self {
            Self::Tokens {
                completion: Completion::Absent,
                ..
            } => Ok(0),
            Self::Tokens {
                completion: Completion::ModelCeiling,
                ..
            } if !output_priced => Ok(0),
            Self::Tokens {
                completion: Completion::ModelCeiling,
                ..
            } => output_ceiling(conn, model_row_id, model)
                .ok_or_else(|| no_output_ceiling(provider, model)),
            Self::Audio { .. } | Self::Images { .. } => Ok(0),
        }
    }

    const fn quote(self, units: i64, completion_slot: u32) -> BudgetQuote {
        match self {
            Self::Tokens { prompt, completion } => BudgetQuote {
                units,
                prompt_tokens: Some(prompt),
                completion_tokens: match completion {
                    Completion::Absent => None,
                    Completion::ModelCeiling => Some(completion_slot),
                },
                audio_ms: None,
            },
            Self::Audio { duration_ms } => BudgetQuote {
                units,
                prompt_tokens: None,
                completion_tokens: None,
                audio_ms: duration_ms,
            },
            Self::Images { count } => BudgetQuote {
                units,
                prompt_tokens: count,
                completion_tokens: None,
                audio_ms: None,
            },
        }
    }

    const fn free_quote(self) -> BudgetQuote {
        match self {
            Self::Tokens { prompt, .. } => BudgetQuote {
                units: 0,
                prompt_tokens: Some(prompt),
                // A host-local engine has no billing ceiling to inherit; the
                // row keeps whatever the engine reports.
                completion_tokens: None,
                audio_ms: None,
            },
            Self::Audio { duration_ms } => BudgetQuote {
                units: 0,
                prompt_tokens: None,
                completion_tokens: None,
                audio_ms: duration_ms,
            },
            Self::Images { count } => BudgetQuote {
                units: 0,
                prompt_tokens: count,
                completion_tokens: None,
                audio_ms: None,
            },
        }
    }
}

fn unpriced(provider: &str, model: &str) -> CoreError {
    CoreError::ServiceUnavailable(format!(
        "no pricing for {provider}/{model}: a prepaid request is refused rather than \
         dispatched unbilled"
    ))
}

fn no_output_ceiling(provider: &str, model: &str) -> CoreError {
    CoreError::ServiceUnavailable(format!(
        "no output-token ceiling on record for {provider}/{model}: its output is priced per \
         token, so without an operator-declared `max_output_tokens` or a known model limit the \
         cost cannot be bounded and the prepaid request is refused"
    ))
}

fn unknown_audio(provider: &str, model: &str) -> CoreError {
    CoreError::Validation(format!(
        "could not determine the audio duration for {provider}/{model}: a prepaid \
         speech-to-text request needs a readable container (wav, ogg/opus, flac, mp3, \
         m4a/mp4) to bound its cost"
    ))
}

fn unknown_images(provider: &str, model: &str) -> CoreError {
    CoreError::Validation(format!(
        "image count is missing or not a positive integer for {provider}/{model}: a \
         prepaid image request needs a persistible quantity to bound its cost"
    ))
}

/// Fail closed when the pricing tables cannot be read: `lookup_for_request`
/// reports a broken read and an absent price identically.
///
/// `prepare` validates the statement (table and columns) without scanning it,
/// which is the cheapest way to tell "no price on record" from "cannot read the
/// records".
fn pricing_store_readable(conn: &rusqlite::Connection) -> Result<()> {
    for table in ["model_capabilities_sync", "combos"] {
        conn.prepare(&format!("SELECT 1 FROM {table} LIMIT 1"))
            .map_err(|error| {
                CoreError::Internal(format!(
                    "pricing store is unreadable ({table}: {error}); refusing to price a \
                     prepaid request from missing evidence"
                ))
            })?;
    }
    Ok(())
}

/// Convert a USD amount to ledger units, rounding **up** so a request is never
/// admitted for less than the cost math will charge.
fn usd_to_units_ceil(usd: f64) -> Result<i64> {
    if !usd.is_finite() || usd < 0.0 {
        return Err(CoreError::Internal(format!(
            "unpriceable request: computed {usd} USD"
        )));
    }
    let scaled = usd * UNITS_PER_USD as f64;
    if scaled >= i64::MAX as f64 {
        return Err(CoreError::ServiceUnavailable(
            "request cost exceeds the credit ledger's representable range".to_string(),
        ));
    }
    Ok(scaled.ceil() as i64)
}

/// Upper bound of the tokens an embedding payload can occupy.
///
/// Text is bounded by its byte length; token arrays by their exact length.
/// Media parts are refused: their token cost depends on the remote resource,
/// not on the URL that references it.
fn embedding_prompt_bound(input: &EmbeddingInput) -> Result<u32> {
    let items = input.items();
    let mut total = 0u32;
    for item in &items {
        total = total.saturating_add(match item {
            EmbeddingItem::Text(text) => text_token_bound(text.len()),
            EmbeddingItem::Tokens(ids) => u32::try_from(ids.len())
                .unwrap_or(u32::MAX)
                .saturating_add(4),
            EmbeddingItem::Parts(parts) => parts_prompt_bound(parts)?,
        });
    }
    Ok(total)
}

fn parts_prompt_bound(parts: &[EmbeddingPart]) -> Result<u32> {
    let mut total = ITEM_FRAMING_TOKENS;
    for part in parts {
        match part {
            EmbeddingPart::Text { text } => {
                total = total.saturating_add(text_token_bound(text.len()));
            }
            EmbeddingPart::Image { .. }
            | EmbeddingPart::Audio { .. }
            | EmbeddingPart::Video { .. }
            | EmbeddingPart::Document { .. } => {
                return Err(CoreError::ServiceUnavailable(
                    "multimodal embedding input cannot be priced before dispatch: the token \
                     cost of an image/audio/video/document reference is a property of the \
                     remote resource, not of the URL, so a prepaid request carrying one is \
                     refused instead of being dispatched without a cost bound"
                        .to_string(),
                ));
            }
        }
    }
    Ok(total.max(1))
}

fn text_token_bound(bytes: usize) -> u32 {
    u32::try_from(bytes)
        .unwrap_or(u32::MAX)
        .saturating_add(ITEM_FRAMING_TOKENS)
}

/// Upper bound of the tokens a SystemOne request can occupy: its state plus
/// its questions, both serialized exactly as the adapter sends them.
fn system_one_prompt_bound(req: &SystemOneRequest) -> Result<u32> {
    let state = serde_json::to_vec(&req.state)
        .map_err(|error| CoreError::Internal(format!("serialize systemone state: {error}")))?;
    let questions = serde_json::to_vec(&req.questions)
        .map_err(|error| CoreError::Internal(format!("serialize systemone questions: {error}")))?;
    let bytes = state.len().saturating_add(questions.len());
    Ok(text_token_bound(bytes))
}

/// The model's own output ceiling: the operator-declared limit on the model
/// row, else the shared catalog's documented limit for that model id.
///
/// There is deliberately no default. A guess here is the difference between
/// "this request can cost the balance" and "this request can cost anything",
/// and only the first may be admitted.
fn output_ceiling(
    conn: &rusqlite::Connection,
    model_row_id: Option<openproxy_types::ids::ModelRowId>,
    model: &str,
) -> Option<u32> {
    let from_row = model_row_id
        .and_then(|row_id| {
            openproxy_db::models::get_by_row_id(conn, row_id)
                .ok()
                .flatten()
        })
        .and_then(|row| row.max_output_tokens)
        .and_then(|limit| u32::try_from(limit).ok())
        .filter(|limit| *limit > 0);
    from_row.or_else(|| {
        openproxy_types::infer_max_output_tokens(model)
            .and_then(|limit| u32::try_from(limit).ok())
            .filter(|limit| *limit > 0)
    })
}

// Prepaid-budget tests build credit-backed fixtures, which only exist with
// the payments feature.
#[cfg(all(test, feature = "payments"))]
#[path = "budget_tests.rs"]
mod tests;
