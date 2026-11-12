//! Admin credits section: balance, top-up, transactions, quota view.
//!
//! Session surface of the prepaid balance: everything a logged-in key owner
//! sees about their credits. The public checkout (anonymous buy, reveal) lives
//! in `crate::handlers::credits_gateway`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;

use crate::state::AppState;
use openproxy_core::credits::{create_order, usd_to_base_units};
use openproxy_types::ids::ApiKeyId;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/balance", get(balance))
        .route("/transactions", get(transactions))
        .route("/topup", post(create_topup))
        .route("/quota", get(quota_status))
        // Admin management: package presets and the order ledger.
        .route("/packages", get(list_packages).post(create_package))
        .route(
            "/packages/{id}",
            axum::routing::patch(update_package).delete(delete_package),
        )
        .route("/orders", get(list_orders))
        // Runtime master switch for the payment gateway (Config toggle).
        .route("/gateway", get(gateway_status).post(set_gateway))
}

fn err(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json!({"error": {"code": code, "message": message}}).to_string(),
    )
        .into_response()
}

/// Session key of the caller: the admin identity's key, or `?key_id=` for
/// an operator acting on someone else's key.
///
/// `?key_id=` is an OPERATOR override. A non-manage session key can only ever
/// address its own credits — the query parameter is ignored for it, so a
/// client (analytics/read) key cannot read or top up someone else's balance.
fn target_key(identity: Option<&crate::handlers::admin::auth::AdminIdentity>, params: &HashMap<String, String>) -> Option<ApiKeyId> {
    let is_operator = identity.map(|i| i.has_manage_scope()).unwrap_or(false);
    if is_operator
        && let Some(k) = params.get("key_id").and_then(|v| v.parse::<i64>().ok())
    {
        return Some(ApiKeyId(k));
    }
    identity.and_then(|i| i.key_id()).map(ApiKeyId)
}

#[derive(Debug, Serialize)]
struct BalanceResponse {
    key_id: i64,
    credit_based: bool,
    balance_usd: String,
    balance_units: i64,
    lots: Vec<LotView>,
}

#[derive(Debug, Serialize)]
struct LotView {
    remaining_usd: String,
    expires_at: Option<String>,
}

async fn balance(
    State(state): State<AppState>,
    identity: Option<axum::extract::Extension<super::auth::AdminIdentity>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if state.config().credits.is_none() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "credits_disabled",
            "on-chain top-ups are not enabled on this server",
        );
    }
    let Some(key_id) = target_key(
        identity.as_ref().map(|axum::extract::Extension(i)| i),
        &params,
    ) else {
        return err(
            StatusCode::UNAUTHORIZED,
            "no_session_key",
            "no session key: top up from the public checkout with your key",
        );
    };
    let kid = key_id.0;
    let res = state
        .db_pool()
        .spawn_read(move |w| {
            let credit_based = openproxy_db::credits_ledger::is_credit_based(w, kid)?;
            let units = openproxy_db::credits_ledger::balance_units(w, kid)?;
            let lots = openproxy_db::credits_ledger::active_lots(w, kid)?;
            Ok((credit_based, units, lots))
        })
        .await;
    match res {
        Ok((credit_based, units, lots)) => Json(BalanceResponse {
            key_id: key_id.0,
            credit_based,
            balance_units: units,
            balance_usd: openproxy_core::credits::units_to_usd_str(units),
            lots: lots
                .into_iter()
                .map(|l| LotView {
                    remaining_usd: openproxy_core::credits::units_to_usd_str(l.remaining_units),
                    expires_at: l.expires_at,
                })
                .collect(),
        })
        .into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "balance_failed",
            &format!("balance lookup failed: {e}"),
        ),
    }
}

#[derive(Debug, Serialize)]
struct TxView {
    kind: String,
    amount_usd: String,
    remaining_usd: String,
    status: String,
    order_code: Option<String>,
    tx_hash: Option<String>,
    basescan_url: Option<String>,
    created_at: String,
}

/// Every entry of the history points back into Base: `tx_hash` plus its
/// basescan URL, so any top-up is auditable on-chain.
fn basescan_url(chain_id: u64, tx: &str) -> String {
    let host = match chain_id {
        8453 => "basescan.org",
        84532 => "sepolia.basescan.org",
        _ => "basescan.org",
    };
    format!("https://{host}/tx/{tx}")
}

async fn transactions(
    State(state): State<AppState>,
    identity: Option<axum::extract::Extension<super::auth::AdminIdentity>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if state.config().credits.is_none() {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "credits_disabled",
            "on-chain top-ups are not enabled on this server",
        );
    }
    let Some(key_id) = target_key(
        identity.as_ref().map(|axum::extract::Extension(i)| i),
        &params,
    ) else {
        return err(
            StatusCode::UNAUTHORIZED,
            "no_session_key",
            "no session key: use ?key_id= or the public checkout",
        );
    };
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(100)
        .clamp(1, 1000);
    let kid = key_id.0;
    let res = state
        .db_pool()
        .spawn_read(move |w| openproxy_db::credits_ledger::list_transactions(w, kid, limit))
        .await;
    let chain_id = state.config().credits.as_ref().map_or(8453, |c| c.chain_id);
    match res {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|t| TxView {
                    kind: t.kind,
                    amount_usd: openproxy_core::credits::units_to_usd_str(t.amount_units),
                    remaining_usd: openproxy_core::credits::units_to_usd_str(t.remaining_units),
                    status: t.status,
                    order_code: t.order_code,
                    basescan_url: t
                        .tx_hash
                        .as_ref()
                        .map(|tx| basescan_url(chain_id, tx)),
                    tx_hash: t.tx_hash,
                    created_at: t.created_at,
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "tx_history_failed",
            &format!("history lookup failed: {e}"),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct TopUpRequest {
    usd: f64,
    ttl_days: i64,
}

async fn create_topup(
    State(state): State<AppState>,
    identity: Option<axum::extract::Extension<super::auth::AdminIdentity>>,
    Query(params): Query<HashMap<String, String>>,
    Json(req): Json<TopUpRequest>,
) -> Response {
    let Some(cfg) = state.config().credits.clone() else {
        return err(
            StatusCode::SERVICE_UNAVAILABLE,
            "credits_disabled",
            "on-chain top-ups are not enabled on this server",
        );
    };
    let Some(key_id) = target_key(
        identity.as_ref().map(|axum::extract::Extension(i)| i),
        &params,
    ) else {
        return err(
            StatusCode::UNAUTHORIZED,
            "no_session_key",
            "no session key: use ?key_id= or the public checkout",
        );
    };
    if !cfg.ttl_days.contains(&(req.ttl_days as u32)) {
        return err(
            StatusCode::BAD_REQUEST,
            "invalid_ttl",
            &format!("ttl_days must be one of {:?}", cfg.ttl_days),
        );
    }
    let base_units = match usd_to_base_units(req.usd) {
        Some(u) if u as f64 >= cfg.min_topup_usd * f64::from(openproxy_core::credits::UNITS_PER_USD as u32) => u,
        _ => {
            return err(
                StatusCode::BAD_REQUEST,
                "invalid_amount",
                &format!("amount must be a whole number of USD >= {}", cfg.min_topup_usd),
            );
        }
    };
    let created = state
        .db_pool()
        .spawn_write(move |w| create_order(w, Some(key_id), None, base_units, req.ttl_days, None))
        .await;
    match created {
        Ok(o) => {
            let qr_uri = crate::handlers::credits_qr::eip681_usdc(
                cfg.chain_id,
                &cfg.usdc_address,
                &cfg.collector_address,
                o.amount_units,
            );
            // Same render as the public checkout reveal: the panel paints the
            // QR straight from `qr_svg` (credits.ts paints it verbatim).
            let qr_svg = match crate::handlers::credits_qr::render(
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
            Json(json!({
                "order_code": o.order_code,
                "status": o.status,
                "amount_units": o.amount_units,
                "amount_display": openproxy_core::credits::units_to_usd_str(o.amount_units),
                "chain_id": cfg.chain_id,
                "usdc_address": cfg.usdc_address,
                "collector_address": cfg.collector_address,
                "qr_uri": qr_uri,
                "qr_svg": qr_svg,
                "expires_at": o.expires_at,
                "payment_window_secs": i64::from(cfg.payment_window_minutes) * 60,
                "no_refunds": true,
            }))
            .into_response()
        }
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "order_failed",
            &format!("could not create order: {e}"),
        ),
    }
}

async fn quota_status(
    State(state): State<AppState>,
    identity: Option<axum::extract::Extension<super::auth::AdminIdentity>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let Some(key_id) = target_key(
        identity.as_ref().map(|axum::extract::Extension(i)| i),
        &params,
    ) else {
        return err(
            StatusCode::UNAUTHORIZED,
            "no_session_key",
            "no session key: use ?key_id=",
        );
    };
    let kid = key_id;
    let res = state
        .db_pool()
        .spawn_read(move |w| openproxy_core::api_key_quotas::status(w, kid))
        .await;
    match res {
        Ok(s) => Json(s).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "quota_status_failed",
            &format!("quota status failed: {e}"),
        ),
    }
}

// ── package presets (admin-defined, copy-paste on purchase) ─────────

async fn list_packages(State(state): State<AppState>) -> Response {
    match state
        .db_pool()
        .spawn_read(openproxy_core::credits::packages::list)
        .await
    {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "packages_failed", &format!("{e}")),
    }
}

#[derive(Debug, Deserialize)]
struct CreatePackageRequest {
    name: String,
    price_usd: f64,
    ttl_days: i64,
    config: Option<openproxy_core::credits::PackageConfig>,
}

async fn create_package(
    State(state): State<AppState>,
    Json(req): Json<CreatePackageRequest>,
) -> Response {
    let name = req.name.trim().to_string();
    if name.is_empty() {
        return err(StatusCode::BAD_REQUEST, "invalid_name", "name is required");
    }
    match state.db_pool().spawn_write(move |w| {
        openproxy_core::credits::packages::create(w, &name, req.price_usd, req.ttl_days, req.config.as_ref())
    }).await {
        Ok(pkg) => (StatusCode::CREATED, Json(pkg)).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "package_failed", &format!("{e}")),
    }
}

#[derive(Debug, Deserialize)]
struct UpdatePackageRequest {
    price_usd: Option<f64>,
    ttl_days: Option<i64>,
    config: Option<openproxy_core::credits::PackageConfig>,
    active: Option<bool>,
}

async fn update_package(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<UpdatePackageRequest>,
) -> Response {
    let res = state.db_pool().spawn_write(move |w| {
        openproxy_core::credits::packages::update(w, id, req.price_usd, req.ttl_days, req.config.as_ref(), req.active)
    }).await;
    match res {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "package_failed", &format!("{e}")),
    }
}

async fn delete_package(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let res = state.db_pool().spawn_write(move |w| {
        openproxy_core::credits::packages::update(w, id, None, None, None, Some(false))
    }).await;
    match res {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "package_failed", &format!("{e}")),
    }
}

/// The full order ledger for the admin: every checkout, its status, exact
/// amount and chain pointer.
async fn list_orders(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(200)
        .clamp(1, 1000);
    match state
        .db_pool()
        .spawn_read(move |r| {
            let mut stmt = r.prepare(
                "SELECT id, order_code, api_key_id, amount_units, base_units, ttl_days, status, \
                 tx_hash, package_id, created_at, paid_at FROM credit_orders \
                 ORDER BY id DESC LIMIT ?1",
            ).map_err(openproxy_db::error::map_db_error)?;
            let rows = stmt.query_map(rusqlite::params![limit], |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "order_code": row.get::<_, String>(1)?,
                    "key_id": row.get::<_, Option<i64>>(2)?,
                    "amount_units": row.get::<_, i64>(3)?,
                    "base_units": row.get::<_, i64>(4)?,
                    "ttl_days": row.get::<_, i64>(5)?,
                    "status": row.get::<_, String>(6)?,
                    "tx_hash": row.get::<_, Option<String>>(7)?,
                    "package_id": row.get::<_, Option<i64>>(8)?,
                    "created_at": row.get::<_, String>(9)?,
                    "paid_at": row.get::<_, Option<String>>(10)?,
                }))
            }).map_err(openproxy_db::error::map_db_error)?;
            let vals: Vec<_> = rows.collect::<Result<_, _>>().map_err(openproxy_db::error::map_db_error)?;
            Ok(vals)
        })
        .await
    {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "orders_failed", &format!("{e}")),
    }
}

// ── gateway master switch ───────────────────────────────────────────

async fn gateway_status(State(state): State<AppState>) -> Response {
    let enabled = state
        .db_pool()
        .spawn_read(|r| {
            openproxy_db::app_config::load_credits_gateway_enabled_from_db(r)
                .map(|v| v.unwrap_or(true))
        })
        .await
        .unwrap_or(true);
    Json(json!({ "enabled": enabled })).into_response()
}

#[derive(Debug, Deserialize)]
struct GatewayToggleRequest {
    enabled: bool,
}

async fn set_gateway(
    State(state): State<AppState>,
    Json(req): Json<GatewayToggleRequest>,
) -> Response {
    let res = state.db_pool().spawn_write(move |w| {
        openproxy_db::app_config::save_credits_gateway_enabled_to_db(w, req.enabled, 0)
    }).await;
    match res {
        Ok(()) => Json(json!({ "enabled": req.enabled })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, "gateway_failed", &format!("{e}")),
    }
}
