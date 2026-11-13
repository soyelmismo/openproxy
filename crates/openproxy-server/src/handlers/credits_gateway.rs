//! Public credit checkout gateway: anonymous key purchase and top-up.
//!
//! Flow (TipRouter-style, adapted to the thin funnel):
//! 1. `POST /v1/credits/order` -> order with a jittered exact amount, the
//!    single collector address, a real 15-minute window and a QR payload.
//! 2. The buyer pays that exact amount on Base; the background watcher sees
//!    the transfer and the matcher credits exactly one order.
//! 3. `GET /v1/credits/order/{code}` -> status poll (5 s cadence from the UI);
//!    once paid, an anonymous order mints a key and reveals it.
//!
//! Money-path invariants enforced here:
//! - a `key_id` target REQUIRES the owner's Bearer token (anonymous minting is
//!   the only unauthenticated create);
//! - an anonymous mint REQUIRES `Idempotency-Key` at CREATION: its hash is
//!   stored on the order, so the minted plaintext can only ever be revealed to
//!   the browser that created the order — a leaked order code is not enough;
//! - creation is IDEMPOTENT per receipt: a retried `Idempotency-Key` returns
//!   the order the first attempt created (same amount, same code), including
//!   expired or already paid orders. A lost response never opens a second payment;
//! - the gateway toggle fails CLOSED: an unreadable flag closes the checkout;
//! - every response of this surface is `Cache-Control: no-store` (a reveal can
//!   carry a plaintext key);
//! - DB failures are reported as such (503), never disguised as "unknown plan";
//! - the order snapshot returned by the create path is what we quote back, and
//!   `payment_window_secs` is the window the DB actually applied.
//!
//! No third party, no escrow, no KYC: a signed on-chain transfer is the proof.

use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

use crate::handlers::credits_qr;
use crate::state::AppState;
use openproxy_core::credits::get_order;
use openproxy_core::credits::matcher::{acknowledge_minted_key, reveal_minted_key_with_receipt};
use openproxy_core::credits::{CreateOrderRequest as CoreCreateOrder, create_order_with_receipt};
use openproxy_types::CoreError;
use openproxy_types::ids::ApiKeyId;

/// Header carrying the client-generated reveal receipt (UUID-like, >= 32
/// chars). Two-phase delivery: the same receipt can retry the reveal and then
/// acknowledge it, so a dropped response never costs the buyer the key.
const IDEMPOTENCY_HEADER: &str = "idempotency-key";

/// Minimum receipt length: a UUIDv4 string is 36 chars; we accept any
/// URL-safe token of at least 32 so `crypto.randomUUID()` works verbatim.
const MIN_RECEIPT_LEN: usize = 32;
const MAX_RECEIPT_LEN: usize = 128;

/// Runtime master switch for the payment gateway (admin Config toggle).
/// Fails CLOSED: if the flag cannot be read, the checkout is closed.
async fn gateway_enabled(state: &AppState) -> Result<bool, CoreError> {
    state
        .db_pool()
        .spawn_read(|r| {
            openproxy_db::app_config::load_credits_gateway_enabled_from_db(r)
                .map(|v| v.unwrap_or(true))
        })
        .await
}

/// Force `no-store` on every response of the credits surface, errors and
/// reveals included. A cached reveal (or a cached 409) is a money bug.
async fn no_store(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    resp.headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/plans", get(plans))
        .route("/order", post(create_checkout))
        .route("/order/{code}", get(order_status))
        .route("/order/{code}/reveal", post(reveal_key))
        .route("/order/{code}/ack", post(ack_key))
        // Self-service surface for an existing key (Bearer = the key itself):
        // read the current plan and buy/renew one. Credit exists only as a
        // plan; there is no prepaid balance to top up.
        .route("/me/plan", get(me_plan).post(me_buy_plan))
        .layer(axum::middleware::from_fn(no_store))
}

fn gateway_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        json!({"error": {"code": code, "message": message}}).to_string(),
    )
        .into_response()
}

/// Map a core/DB failure onto a client-safe response. The detail goes to the
/// log; the buyer gets a stable code and no internals.
fn core_failure(op: &'static str, e: CoreError) -> Response {
    tracing::warn!(error = %e, op, "credits gateway failure");
    match e {
        CoreError::Validation(msg) => gateway_error(StatusCode::BAD_REQUEST, "invalid_order", &msg),
        CoreError::NotFound { .. } => {
            gateway_error(StatusCode::NOT_FOUND, "unknown_plan", "unknown plan")
        }
        // A DB/IO failure is NOT an unknown plan: fail closed and say so.
        _ => gateway_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "temporarily_unavailable",
            "the checkout is temporarily unavailable; try again",
        ),
    }
}

/// A purchasable plan: the tier IS the product (000115). `usd` is what the
/// buyer pays, `credit_usd` the leveraged balance they get (absent == 1:1),
/// `ttl_days` how long the credited balance lives. `models` + `combos` are
/// the display names of everything the plan unlocks.
#[derive(Debug, Serialize)]
struct PlanOffer {
    tier_id: i64,
    name: String,
    usd: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    credit_usd: Option<f64>,
    ttl_days: i64,
    models: Vec<String>,
    combos: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PlansResponse {
    enabled: bool,
    plans: Vec<PlanOffer>,
}

async fn plans(State(state): State<AppState>) -> Response {
    if state.config().credits.is_none() {
        return Json(PlansResponse {
            enabled: false,
            plans: vec![],
        })
        .into_response();
    }
    // Fail closed: an unreadable toggle is reported as "not selling".
    let buying_open = match gateway_enabled(&state).await {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(error = %e, "gateway flag unreadable; reporting plans as closed");
            false
        }
    };
    let tiers = match state
        .db_pool()
        .spawn_read(openproxy_core::credits::tiers::list)
        .await
    {
        Ok(t) => t,
        // Never turn a DB failure into "no plans available".
        Err(e) => return core_failure("plans", e),
    };
    let plans: Vec<PlanOffer> = tiers
        .iter()
        .filter(|t| t.sellable())
        .map(|t| PlanOffer {
            tier_id: t.id,
            name: t.name.clone(),
            usd: t.price_usd.unwrap_or_default(),
            credit_usd: t.credit_usd,
            ttl_days: t.ttl_days,
            models: t.config.allowed_models.clone().unwrap_or_default(),
            combos: t
                .config
                .allowed_combos
                .clone()
                .unwrap_or_default()
                .into_iter()
                .map(|c| c.strip_prefix("combo:").unwrap_or(&c).to_string())
                .collect(),
        })
        .collect();
    Json(PlansResponse {
        enabled: buying_open,
        plans,
    })
    .into_response()
}

/// Body of `POST /v1/credits/order` and `POST /v1/credits/me/plan`.
///
/// `mint_new_key: true` buys an anonymous key (the key is revealed once the
/// payment lands). `key_id` tops up an existing key instead and REQUIRES the
/// owner's Bearer token. Exactly one of the two must be given, and the wallet
/// pays the jittered exact amount.
///
/// Delivery contract: an anonymous mint must carry `Idempotency-Key`
/// (32..=128 URL-safe chars). The server hashes it into the order at creation
/// and only that exact receipt can reveal the key. A key-bound renewal is
/// authenticated by the owner's Bearer token, so the header is optional there
/// (when present it is honoured, which makes a retried renewal idempotent
/// enough to keep the same delivery capability).
///
/// `POST /v1/credits/me/plan` OVERRIDES the target: the key is always the one
/// the Bearer token proves (`key_id`/`mint_new_key` in the body are ignored),
/// so a caller can never bill a renewal to someone else's key through it.
#[derive(Debug, Deserialize)]
struct CheckoutOrderBody {
    tier_id: Option<i64>,
    mint_new_key: Option<bool>,
    key_id: Option<i64>,
}

#[derive(Debug, Serialize)]
struct CheckoutResponse {
    order_code: String,
    status: String,
    usd: f64,
    base_usd: f64,
    /// Balance credited when the payment lands (leveraged packages credit
    /// more than the charge). Absent == 1:1.
    #[serde(skip_serializing_if = "Option::is_none")]
    credit_usd: Option<f64>,
    amount_units: i64,
    amount_display: String,
    chain_id: u64,
    usdc_address: String,
    collector_address: String,
    /// Set when the order tops up an existing key: the result is a plan, not
    /// a plaintext key, so the UI must not offer a reveal.
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_id: Option<i64>,
    qr_uri: String,
    qr_svg: String,
    expires_at: String,
    /// The window the DATABASE applied to this order (not a config promise).
    payment_window_secs: i64,
    /// How long after `expires_at` a transfer is still accepted (block time).
    /// The page polls exactly this long, then stops.
    confirmation_grace_secs: i64,
    no_refunds: bool,
}

async fn create_checkout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CheckoutOrderBody>,
) -> Response {
    create_order_impl(state, &headers, req, None).await
}

/// Shared create path. `authorized` carries the key already authenticated by
/// the caller (`/me/plan`); the public route passes `None` and any `key_id`
/// in the body must then be proven with the owner's Bearer token.
async fn create_order_impl(
    state: AppState,
    headers: &HeaderMap,
    req: CheckoutOrderBody,
    authorized: Option<ApiKeyId>,
) -> Response {
    let Some(cfg) = state.config().credits.clone() else {
        return gateway_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "credits_disabled",
            "on-chain top-ups are not enabled on this server",
        );
    };
    // The window quoted to the buyer is the one the DB snapshots on the
    // order. Validate the configured value here so a bad config is a clean
    // 400 instead of an opaque write failure.
    let window_secs = i64::from(cfg.payment_window_minutes) * 60;
    if !(60..=86_400).contains(&window_secs) {
        return gateway_error(
            StatusCode::BAD_REQUEST,
            "invalid_payment_window",
            "payment_window_minutes must be between 1 and 1440",
        );
    }
    match gateway_enabled(&state).await {
        Ok(true) => {}
        Ok(false) => {
            return gateway_error(
                StatusCode::FORBIDDEN,
                "gateway_disabled",
                "the payment gateway is temporarily closed by the operator",
            );
        }
        // Fail closed: an unknown switch state must not sell anything.
        Err(e) => return core_failure("create_order.toggle", e),
    }
    let Some(tid) = req.tier_id else {
        return gateway_error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "specify tier_id (every checkout buys a plan)",
        );
    };
    // Exactly one target: mint a new anonymous key, or top one up.
    let api_key_id = match (req.mint_new_key.unwrap_or(false), req.key_id) {
        (true, None) => None,
        (false, Some(id)) => {
            let kid = ApiKeyId(id);
            // Cross-tenant guard: `key_id` may only address the caller's key.
            let proven = match authorized {
                Some(a) => a,
                None => match me_key_id(&state, headers).await {
                    Ok(k) => k,
                    Err(resp) => return resp,
                },
            };
            if proven != kid {
                return gateway_error(
                    StatusCode::FORBIDDEN,
                    "not_your_key",
                    "the Bearer key does not own this key_id",
                );
            }
            Some(kid)
        }
        _ => {
            return gateway_error(
                StatusCode::BAD_REQUEST,
                "invalid_target",
                "specify either mint_new_key=true or key_id, not both",
            );
        }
    };
    // Delivery receipt. Anonymous mints cannot exist without one: the key
    // would be unclaimable (or claimable by anyone holding the code), so fail
    // before touching the DB. Renewals are Bearer-authenticated and reveal
    // nothing, so the header stays optional there.
    let receipt = match receipt_of(headers) {
        Some(Ok(r)) => Some(r),
        Some(Err(())) => {
            return gateway_error(
                StatusCode::BAD_REQUEST,
                "invalid_receipt",
                "Idempotency-Key must be 32-128 url-safe characters",
            );
        }
        None if api_key_id.is_none() => {
            return gateway_error(
                StatusCode::BAD_REQUEST,
                "missing_receipt",
                "anonymous checkout requires Idempotency-Key (32-128 url-safe \
                 characters); it is the only capability that can reveal the key",
            );
        }
        None => None,
    };
    // ONE writer transaction validates the plan/key and creates the order.
    // `create_order` derives price, lifetime and leveraged credit from the
    // tier inside its own transaction (0/0 placeholders are never used for a
    // tier order), so the handler never quotes a stale price.
    let created = state
        .db_pool()
        .spawn_write(move |w| {
            let Some(receipt) = receipt else {
                // Only a Bearer-authenticated renewal can reach this branch.
                return openproxy_core::credits::orders::create_order_with_window(
                    w,
                    api_key_id,
                    None,
                    0,
                    0,
                    Some(tid),
                    window_secs,
                );
            };
            create_order_with_receipt(
                w,
                CoreCreateOrder {
                    api_key_id,
                    payer_email: None,
                    base_units: 0,
                    ttl_days: 0,
                    tier_id: Some(tid),
                },
                window_secs,
                &receipt,
            )
        })
        .await;
    let order = match created {
        Ok(o) => o,
        Err(e) => return core_failure("create_order", e),
    };
    let qr_uri = credits_qr::eip681_usdc(
        cfg.chain_id,
        &cfg.usdc_address,
        &cfg.collector_address,
        order.amount_units,
    );
    let qr_svg = match credits_qr::render(&qr_uri, "#6366f1", "#0b0d12", "#ffffff") {
        Ok(q) => q.svg,
        Err(e) => {
            tracing::warn!(error = %e, "qr render failed");
            String::new()
        }
    };
    Json(CheckoutResponse {
        order_code: order.order_code,
        status: order.status,
        // Charge == base amount (the jitter is not billed); every number here
        // comes from the persisted order snapshot, never from an earlier read.
        usd: openproxy_core::credits::units_to_usd_str(order.base_units)
            .parse::<f64>()
            .unwrap_or(0.0),
        base_usd: openproxy_core::credits::units_to_usd_str(order.base_units)
            .parse::<f64>()
            .unwrap_or(0.0),
        credit_usd: if order.credit_units != order.base_units {
            openproxy_core::credits::units_to_usd_str(order.credit_units)
                .parse::<f64>()
                .ok()
        } else {
            None
        },
        amount_units: order.amount_units,
        amount_display: openproxy_core::credits::units_to_usd_str(order.amount_units),
        chain_id: cfg.chain_id,
        usdc_address: cfg.usdc_address.clone(),
        collector_address: cfg.collector_address.clone(),
        api_key_id: order.checkout_key_id,
        qr_uri,
        qr_svg,
        expires_at: order.expires_at,
        payment_window_secs: order.payment_window_secs,
        confirmation_grace_secs: openproxy_core::credits::orders::CONFIRMATION_GRACE_SECS,
        no_refunds: true,
    })
    .into_response()
}

#[derive(Debug, Serialize)]
struct OrderStatusResponse {
    order_code: String,
    status: String,
    amount_units: i64,
    base_units: i64,
    ttl_days: i64,
    api_key_id: Option<i64>,
    expires_at: String,
    paid_at: Option<String>,
    tx_hash: Option<String>,
    block_number: Option<i64>,
    key_revealed: bool,
    no_refunds: bool,
}

async fn order_status(State(state): State<AppState>, Path(code): Path<String>) -> Response {
    if state.config().credits.is_none() {
        return gateway_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "credits_disabled",
            "on-chain top-ups are not enabled on this server",
        );
    }
    let found = state
        .db_pool()
        .spawn_read(move |w| get_order(w, &code))
        .await;
    match found {
        Ok(Some(o)) => Json(OrderStatusResponse {
            order_code: o.order_code,
            status: o.status,
            amount_units: o.amount_units,
            base_units: o.base_units,
            ttl_days: o.ttl_days,
            api_key_id: o.api_key_id,
            expires_at: o.expires_at,
            paid_at: o.paid_at,
            tx_hash: o.tx_hash.clone(),
            block_number: o.block_number,
            key_revealed: o.key_revealed,
            no_refunds: true,
        })
        .into_response(),
        Ok(None) => gateway_error(StatusCode::NOT_FOUND, "not_found", "unknown order code"),
        Err(e) => core_failure("order_status", e),
    }
}

#[derive(Debug, Serialize)]
struct RevealResponse {
    key: String,
    warning: String,
}

/// Read the client receipt. `Some(Err)` = present but unusable.
fn receipt_of(headers: &HeaderMap) -> Option<Result<String, ()>> {
    let Ok(raw) = headers.get(IDEMPOTENCY_HEADER)?.to_str() else {
        return Some(Err(()));
    };
    if raw.len() < MIN_RECEIPT_LEN || raw.len() > MAX_RECEIPT_LEN {
        return Some(Err(()));
    }
    if !raw
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Some(Err(()));
    }
    Some(Ok(raw.to_string()))
}

/// Reveal the minted anonymous key. `Idempotency-Key` is REQUIRED: the
/// receipt is the delivery capability. The ciphertext is kept until
/// `POST /order/{code}/ack` with the same receipt wipes it, so a dropped
/// response can be retried — and a caller without the receipt can never claim
/// the key, even with the order code (there is no header-less legacy path:
/// that path let any holder of a leaked order code consume the ciphertext).
async fn reveal_key(
    State(state): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Response {
    if state.config().credits.is_none() {
        return gateway_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "credits_disabled",
            "on-chain top-ups are not enabled on this server",
        );
    }
    let receipt = match receipt_of(&headers) {
        Some(Ok(r)) => r,
        Some(Err(())) => {
            return gateway_error(
                StatusCode::BAD_REQUEST,
                "invalid_receipt",
                "Idempotency-Key must be 32-128 url-safe characters",
            );
        }
        None => {
            return gateway_error(
                StatusCode::BAD_REQUEST,
                "missing_receipt",
                "reveal requires the Idempotency-Key used for this checkout",
            );
        }
    };
    let mk = Arc::clone(state.master_key());
    let res = state
        .db_pool()
        .spawn_write(move |w| reveal_minted_key_with_receipt(w, mk.as_ref(), &code, &receipt))
        .await;
    match res {
        Ok(Some(key)) => Json(RevealResponse {
            key,
            warning: "shown once: save it now; it cannot be recovered".into(),
        })
        .into_response(),
        Ok(None) => gateway_error(
            StatusCode::CONFLICT,
            "already_revealed_or_unpaid",
            "key already revealed, not paid yet, or not an anonymous order",
        ),
        Err(e) => core_failure("reveal_key", e),
    }
}

/// Wipe the stored ciphertext once the client confirms it saved the key. The
/// receipt must match the one used for the retryable reveal, and a repeated
/// ACK of the SAME receipt is a no-op success (200), so a lost ACK response
/// never turns into a spurious error.
async fn ack_key(
    State(state): State<AppState>,
    Path(code): Path<String>,
    headers: HeaderMap,
) -> Response {
    if state.config().credits.is_none() {
        return gateway_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "credits_disabled",
            "on-chain top-ups are not enabled on this server",
        );
    }
    let Some(Ok(receipt)) = receipt_of(&headers) else {
        return gateway_error(
            StatusCode::BAD_REQUEST,
            "missing_receipt",
            "ack requires the same Idempotency-Key used for the reveal",
        );
    };
    match state
        .db_pool()
        .spawn_write(move |w| acknowledge_minted_key(w, &code, &receipt))
        .await
    {
        Ok(true) => Json(json!({ "ok": true })).into_response(),
        Ok(false) => gateway_error(
            StatusCode::CONFLICT,
            "nothing_to_ack",
            "no delivered-but-unacknowledged key for this order and receipt",
        ),
        Err(e) => core_failure("ack_key", e),
    }
}

// ── self-service surface (Bearer = the buyer's own key) ─────────────

/// Resolve the key making a `/me` call. Accepts the key's own Bearer token
/// (the same one used for chat) with any buyer-visible scope: a buyer never
/// needs an admin session to see their own plan or buy one. The scope list is
/// aligned with `admin/auth.rs` so `read`/`analytics` client keys work too.
async fn me_key_id(state: &AppState, headers: &HeaderMap) -> Result<ApiKeyId, Response> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(str::trim)
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
        })
        .filter(|t| !t.is_empty());
    let Some(token) = token else {
        return Err(gateway_error(
            StatusCode::UNAUTHORIZED,
            "missing_key",
            "pass your API key as Authorization: Bearer <key>",
        ));
    };
    match crate::middleware::auth::verify_key_credentials_any_scope(
        state,
        token,
        &["chat", "manage", "analytics", "read"],
    )
    .await
    {
        Ok(key) => Ok(key.id),
        Err(e) => Err(gateway_error(
            StatusCode::UNAUTHORIZED,
            "invalid_key",
            &format!("invalid api key: {e}"),
        )),
    }
}

/// The key's current plan: name, leveraged credit, remaining balance and the
/// lot's expiry. `plan` is null when the key has no tier assigned.
async fn me_plan(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let key_id = match me_key_id(&state, &headers).await {
        Ok(k) => k,
        Err(resp) => return resp,
    };
    let plan = match state
        .db_pool()
        .spawn_read(move |r| openproxy_core::api_key_quotas::plan_status(r, key_id))
        .await
    {
        Ok(p) => p,
        Err(e) => return core_failure("me_plan", e),
    };
    Json(json!({ "plan": plan })).into_response()
}

/// Buy (or renew) a plan for the calling key.
async fn me_buy_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CheckoutOrderBody>,
) -> Response {
    let key_id = match me_key_id(&state, &headers).await {
        Ok(k) => k,
        Err(resp) => return resp,
    };
    let mut order_req = req;
    order_req.mint_new_key = None;
    order_req.key_id = Some(key_id.0);
    create_order_impl(state, &headers, order_req, Some(key_id)).await
}
