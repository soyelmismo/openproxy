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
use openproxy_core::credits::{UNITS_PER_USD, create_order, get_order, usd_to_base_units};
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
        .route("/packages", get(packages))
        .route("/order", post(create_checkout))
        .route("/order/{code}", get(order_status))
        .route("/order/{code}/reveal", post(reveal_key))
        // Self-service surface for an existing key (Bearer = the key itself):
        // balance, history and top-up without any admin session.
        .route("/me/balance", get(me_balance))
        .route("/me/transactions", get(me_transactions))
        .route("/me/topup", post(me_topup))
}

fn gateway_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json!({"error": {"code": code, "message": message}}).to_string(),
    )
        .into_response()
}

/// What a checkout costs and what the buyer gets.
#[derive(Debug, Serialize)]
struct PackageOffer {
    package_id: Option<i64>,
    usd: f64,
    base_units: i64,
    ttl_days: i64,
    label: String,
}

#[derive(Debug, Serialize)]
struct PackagesResponse {
    enabled: bool,
    chain_id: u64,
    usdc_address: String,
    collector_address: String,
    payment_window_minutes: u32,
    min_topup_usd: f64,
    ttl_days: Vec<u32>,
    packages: Vec<PackageOffer>,
}

async fn packages(State(state): State<AppState>) -> Response {
    let Some(cfg) = state.config().credits.clone() else {
        return Json(PackagesResponse {
            enabled: false,
            chain_id: 8453,
            usdc_address: String::new(),
            collector_address: String::new(),
            payment_window_minutes: 15,
            min_topup_usd: 0.0,
            ttl_days: vec![],
            packages: vec![],
        })
        .into_response();
    };
    // Admin-defined presets first; the flat dollar amounts are the fallback
    // when no package has been configured yet.
    let table_pkgs = state
        .db_pool()
        .spawn_read(openproxy_core::credits::packages::list_active)
        .await
        .unwrap_or_default();
    let buying_open = gateway_enabled(&state).await;
    let packages: Vec<PackageOffer> = if table_pkgs.is_empty() {
        [5.0_f64, 10.0, 25.0, 50.0, 100.0]
            .into_iter()
            .filter(|usd| *usd >= cfg.min_topup_usd)
            .filter_map(|usd| {
                usd_to_base_units(usd).map(|base_units| PackageOffer {
                    package_id: None,
                    usd,
                    base_units,
                    ttl_days: i64::from(*cfg.ttl_days.first().unwrap_or(&15)),
                    label: format!("${usd:.0}"),
                })
            })
            .collect()
    } else {
        table_pkgs
            .iter()
            .map(|p| PackageOffer {
                package_id: Some(p.id),
                usd: p.price_usd,
                base_units: openproxy_core::credits::usd_to_base_units(p.price_usd)
                    .unwrap_or_default(),
                ttl_days: p.ttl_days,
                label: p.name.clone(),
            })
            .collect()
    };
    Json(PackagesResponse {
        enabled: buying_open,
        chain_id: cfg.chain_id,
        usdc_address: cfg.usdc_address.clone(),
        collector_address: cfg.collector_address.clone(),
        payment_window_minutes: cfg.payment_window_minutes,
        min_topup_usd: cfg.min_topup_usd,
        ttl_days: cfg.ttl_days.clone(),
        packages,
    })
    .into_response()
}

/// Body of `POST /v1/credits/order`.
///
/// `mint_new_key: true` buys an anonymous key (the key is revealed once the
/// payment lands). `key_id` tops up an existing key instead. Exactly one of
/// the two must be given, and the wallet pays the jittered exact amount.
#[derive(Debug, Deserialize)]
struct CreateOrderRequest {
    usd: Option<f64>,
    ttl_days: Option<i64>,
    package_id: Option<i64>,
    mint_new_key: Option<bool>,
    key_id: Option<i64>,
}

#[derive(Debug, Serialize)]
struct CheckoutResponse {
    order_code: String,
    status: String,
    usd: f64,
    base_usd: f64,
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
    // A package buy takes its price and lifetime straight from the preset;
    // a free-amount buy validates usd/ttl against the server config.
    let (base_units, ttl_days, package_id) = if let Some(pid) = req.package_id {
        let Some(pkg) = state
            .db_pool()
            .spawn_read(move |r| openproxy_core::credits::packages::get(r, pid))
            .await
            .ok()
            .flatten()
        else {
            return gateway_error(StatusCode::NOT_FOUND, "unknown_package", "unknown package");
        };
        if !pkg.active {
            return gateway_error(StatusCode::BAD_REQUEST, "inactive_package", "package is not active");
        }
        let base = (pkg.price_usd * f64::from(openproxy_core::credits::UNITS_PER_USD as u32))
            .round() as i64;
        (base, pkg.ttl_days, Some(pkg.id))
    } else {
        let Some(usd) = req.usd else {
            return gateway_error(StatusCode::BAD_REQUEST, "invalid_amount", "specify package_id or usd");
        };
        let ttl = req.ttl_days.unwrap_or(15);
        match usd_to_base_units(usd) {
            Some(u) if u as f64 >= cfg.min_topup_usd * UNITS_PER_USD as f64 => {
                if !cfg.ttl_days.contains(&(ttl as u32)) {
                    return gateway_error(
                        StatusCode::BAD_REQUEST,
                        "invalid_ttl",
                        &format!("ttl_days must be one of {:?}", cfg.ttl_days),
                    );
                }
                (u, ttl, None)
            }
            _ => {
                return gateway_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_amount",
                    &format!(
                        "amount must be a whole number of USD >= {} (no fractions: the jitter owns the cents)",
                        cfg.min_topup_usd
                    ),
                );
            }
        }
    };
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
        move |w| create_order(w, api_key_id, None, base_units, ttl_days, package_id)
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
        usd: req.usd.unwrap_or(0.0),
        base_usd: openproxy_core::credits::units_to_usd_str(order.base_units)
            .parse::<f64>()
            .unwrap_or(0.0),
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
/// admin session to see their own balance or top up.
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

async fn me_balance(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let key_id = match me_key_id(&state, &headers).await {
        Ok(k) => k,
        Err(resp) => return resp,
    };
    let kid = key_id.0;
    match state
        .db_pool()
        .spawn_read(move |w| {
            let credit_based = openproxy_db::credits_ledger::is_credit_based(w, kid)?;
            let units = openproxy_db::credits_ledger::balance_units(w, kid)?;
            let lots = openproxy_db::credits_ledger::active_lots(w, kid)?;
            Ok((credit_based, units, lots))
        })
        .await
    {
        Ok((credit_based, units, lots)) => Json(json!({
            "key_id": kid,
            "credit_based": credit_based,
            "balance_units": units,
            "balance_usd": openproxy_core::credits::units_to_usd_str(units),
            "lots": lots.into_iter().map(|l| json!({
                "remaining_usd": openproxy_core::credits::units_to_usd_str(l.remaining_units),
                "expires_at": l.expires_at,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => gateway_error(StatusCode::INTERNAL_SERVER_ERROR, "balance_failed", &format!("{e}")),
    }
}

async fn me_transactions(
    State(state): State<AppState>,
    headers: HeaderMap,
    axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let key_id = match me_key_id(&state, &headers).await {
        Ok(k) => k,
        Err(resp) => return resp,
    };
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(100)
        .clamp(1, 1000);
    let kid = key_id.0;
    let chain_id = state.config().credits.as_ref().map_or(8453, |c| c.chain_id);
    match state
        .db_pool()
        .spawn_read(move |w| openproxy_db::credits_ledger::list_transactions(w, kid, limit))
        .await
    {
        Ok(rows) => Json(rows
            .into_iter()
            .map(|t| {
                let basescan_url = t
                    .tx_hash
                    .as_ref()
                    .map(|tx| format!("https://basescan.org/tx/{tx}"));
                json!({
                    "kind": t.kind,
                    "amount_usd": openproxy_core::credits::units_to_usd_str(t.amount_units),
                    "remaining_usd": openproxy_core::credits::units_to_usd_str(t.remaining_units),
                    "status": t.status,
                    "order_code": t.order_code,
                    "tx_hash": t.tx_hash,
                    "basescan_url": basescan_url,
                    "created_at": t.created_at,
                })
            })
            .collect::<Vec<_>>())
        .into_response(),
        Err(e) => gateway_error(StatusCode::INTERNAL_SERVER_ERROR, "history_failed", &format!("{e}")),
    }
}

async fn me_topup(
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
