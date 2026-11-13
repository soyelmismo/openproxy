//! Public credit checkout gateway: anonymous key purchase and top-up.
//!
//! Flow (TipRouter-style, adapted to the thin funnel):
//! 1. `POST /v1/credits/order` -> order with a jittered exact amount, the
//!    single collector address, a 15-minute window and a QR payload.
//! 2. The buyer pays that exact amount on Base; the background watcher sees
//!    the transfer and the matcher credits exactly one order.
//! 3. `GET /v1/credits/order/{code}` -> status poll (5 s cadence from the UI);
//!    once paid, an anonymous order mints a key and reveals it exactly once.
//!
//! No third party, no escrow, no KYC: a signed on-chain transfer is the proof.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

use crate::handlers::credits_qr;
use crate::state::AppState;
use openproxy_core::credits::{create_order, get_order};
use openproxy_core::credits::matcher::reveal_minted_key;
use openproxy_types::ids::ApiKeyId;

/// Checkout routes, mounted under `/v1/credits` on the public listener.

/// Runtime master switch for the payment gateway (admin Config toggle).
/// Reading the DB flag per checkout call is negligible next to the RPC poll;
/// it makes the toggle effective instantly, no restart. Default: enabled.
async fn gateway_enabled(state: &AppState) -> bool {
    state
        .db_pool()
        .spawn_read(|r| {
            openproxy_db::app_config::load_credits_gateway_enabled_from_db(r)
                .map(|v| v.unwrap_or(true))
        })
        .await
        .unwrap_or(true)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/plans", get(plans))
        .route("/order", post(create_checkout))
        .route("/order/{code}", get(order_status))
        .route("/order/{code}/reveal", post(reveal_key))
        // Self-service surface for an existing key (Bearer = the key itself):
        // read the current plan and buy/renew one. Credit exists only as a
        // plan; there is no prepaid balance to top up.
        .route("/me/plan", get(me_plan).post(me_buy_plan))
}

fn gateway_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json!({"error": {"code": code, "message": message}}).to_string(),
    )
        .into_response()
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
    let buying_open = gateway_enabled(&state).await;
    let tiers = state
        .db_pool()
        .spawn_read(openproxy_core::credits::tiers::list)
        .await
        .unwrap_or_default();
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

/// Body of `POST /v1/credits/order`.
///
/// `mint_new_key: true` buys an anonymous key (the key is revealed once the
/// payment lands). `key_id` tops up an existing key instead. Exactly one of
/// the two must be given, and the wallet pays the jittered exact amount.
/// Every checkout buys a specific plan (tier).
#[derive(Debug, Deserialize)]
struct CreateOrderRequest {
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
    qr_uri: String,
    qr_svg: String,
    expires_at: String,
    payment_window_secs: i64,
    no_refunds: bool,
}

async fn create_checkout(
    State(state): State<AppState>,
    Json(req): Json<CreateOrderRequest>,
) -> Response {
    let Some(cfg) = state.config().credits.clone() else {
        return gateway_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "credits_disabled",
            "on-chain top-ups are not enabled on this server",
        );
    };
    if !gateway_enabled(&state).await {
        return gateway_error(
            StatusCode::FORBIDDEN,
            "gateway_disabled",
            "the payment gateway is temporarily closed by the operator",
        );
    }
    // A plan buy takes its price and lifetime straight from the tier; the
    // tier must be active and carry a price (drafts are not sellable).
    let Some(tid) = req.tier_id else {
        return gateway_error(
            StatusCode::BAD_REQUEST,
            "invalid_target",
            "specify tier_id (every checkout buys a plan)",
        );
    };
    let Some(tier) = state
        .db_pool()
        .spawn_read(move |r| openproxy_core::credits::tiers::get(r, tid))
        .await
        .ok()
        .flatten()
    else {
        return gateway_error(StatusCode::NOT_FOUND, "unknown_plan", "unknown plan");
    };
    if !tier.active {
        return gateway_error(StatusCode::BAD_REQUEST, "inactive_plan", "plan is not active");
    }
    let Some(price) = tier.price_usd else {
        return gateway_error(StatusCode::BAD_REQUEST, "not_sellable", "plan has no price");
    };
    let base_units = (price * f64::from(openproxy_core::credits::UNITS_PER_USD as u32)).round() as i64;
    let ttl_days = tier.ttl_days;
    let tier_id = tier.id;
    // Exactly one target: mint a new anonymous key, or top one up.
    let api_key_id = match (req.mint_new_key.unwrap_or(false), req.key_id) {
        (true, None) => None,
        (false, Some(id)) => Some(ApiKeyId(id)),
        _ => {
            return gateway_error(
                StatusCode::BAD_REQUEST,
                "invalid_target",
                "specify either mint_new_key=true or key_id, not both",
            );
        }
    };
    let created = state.db_pool().spawn_write({
        move |w| create_order(w, api_key_id, None, base_units, ttl_days, Some(tier_id))
    });
    let order = match created.await {
        Ok(o) => o,
        Err(e) => {
            return gateway_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "order_failed",
                &format!("could not create order: {e}"),
            );
        }
    };
    let qr_uri = credits_qr::eip681_usdc(
        cfg.chain_id,
        &cfg.usdc_address,
        &cfg.collector_address,
        order.amount_units,
    );
    let qr_svg = match credits_qr::render(
        &qr_uri,
        "#6366f1",
        "#0b0d12",
        "#ffffff",
    ) {
        Ok(q) => q.svg,
        Err(e) => {
            tracing::warn!(error = %e, "qr render failed");
            String::new()
        }
    };
    Json(CheckoutResponse {
        order_code: order.order_code,
        status: order.status,
        usd: price,
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
        qr_uri,
        qr_svg,
        expires_at: order.expires_at,
        payment_window_secs: i64::from(cfg.payment_window_minutes) * 60,
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

async fn order_status(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Response {
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
        Err(e) => gateway_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "order_lookup_failed",
            &format!("order lookup failed: {e}"),
        ),
    }
}

#[derive(Debug, Serialize)]
struct RevealResponse {
    key: String,
    warning: String,
}

/// Reveal the minted anonymous key, once. Second attempt: 409. The plaintext
/// lives AES-GCM-encrypted in the order row and is wiped on reveal.
async fn reveal_key(
    State(state): State<AppState>,
    Path(code): Path<String>,
) -> Response {
    if state.config().credits.is_none() {
        return gateway_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "credits_disabled",
            "on-chain top-ups are not enabled on this server",
        );
    }
    let mk = Arc::clone(state.master_key());
    let res = state
        .db_pool()
        .spawn_write(move |w| reveal_minted_key(w, mk.as_ref(), &code))
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
        Err(e) => gateway_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "reveal_failed",
            &format!("reveal failed: {e}"),
        ),
    }
}

// ── self-service surface (Bearer = the buyer's own key) ─────────────

/// Resolve the key making a `/me` call. Accepts the key's own Bearer token
/// (the same one used for chat) with `chat` scope: a buyer never needs an
/// admin session to see their own plan or buy one.
async fn me_key_id(state: &AppState, headers: &HeaderMap) -> Result<ApiKeyId, Response> {
    let token = headers
        .get(axum::http::header::AUTHORIZATION)
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
    match crate::middleware::auth::verify_key_credentials_any_scope(state, token, &["chat", "manage"]).await {
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
        Err(e) => {
            return gateway_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "plan_failed",
                &format!("{e}"),
            );
        }
    };
    Json(json!({ "plan": plan })).into_response()
}

/// Buy (or renew) a plan for the calling key.
async fn me_buy_plan(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateOrderRequest>,
) -> Response {
    let key_id = match me_key_id(&state, &headers).await {
        Ok(k) => k,
        Err(resp) => return resp,
    };
    let mut order_req = req;
    order_req.mint_new_key = None;
    order_req.key_id = Some(key_id.0);
    match create_checkout(State(state), Json(order_req)).await {
        resp if resp.status().is_success() => resp,
        _resp => _resp,
    }
}
