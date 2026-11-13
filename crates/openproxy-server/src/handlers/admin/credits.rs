//! Admin credits section: plan status, plans (tiers), orders and the gateway
//! switch.
//!
//! Credit exists only as a plan: a tier carries the price, the leveraged
//! balance, the lifetime and the model allowlist. There is no prepaid balance
//! and no manual top-up. The public checkout (anonymous buy, reveal) lives in
//! `crate::handlers::credits_gateway`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

use crate::state::AppState;

use openproxy_types::ids::ApiKeyId;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/quota", get(quota_status))
        // Plans (tiers) and the order ledger — credit packages are gone.
        .route("/tiers", get(list_tiers).post(create_tier))
        .route(
            "/tiers/{id}",
            axum::routing::patch(update_tier).delete(delete_tier),
        )
        .route("/orders", get(list_orders))
        // Full model+combo catalog for tier allowlists (every entry, not
        // only the first page of active models the chat view shows).
        .route("/catalog", get(admin_catalog))
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
/// client (analytics/read) key cannot read someone else's plan.
fn target_key(
    identity: Option<&crate::handlers::admin::auth::AdminIdentity>,
    params: &HashMap<String, String>,
) -> Option<ApiKeyId> {
    let is_operator = identity.is_some_and(|i| i.has_manage_scope());
    if is_operator && let Some(k) = params.get("key_id").and_then(|v| v.parse::<i64>().ok()) {
        return Some(ApiKeyId(k));
    }
    identity.and_then(|i| i.key_id()).map(ApiKeyId)
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

// ── tier presets (the access contract packages point at) ────────────

async fn list_tiers(State(state): State<AppState>) -> Response {
    match state
        .db_pool()
        .spawn_read(openproxy_core::credits::tiers::list)
        .await
    {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "tiers_failed",
            &format!("{e}"),
        ),
    }
}

#[derive(Debug, Deserialize)]
struct TierRequest {
    name: String,
    config: openproxy_core::credits::TierConfig,
    /// Plan half (000115): price/credit/lifetime of the sellable plan.
    price_usd: Option<f64>,
    credit_usd: Option<f64>,
    #[serde(default)]
    ttl_days: i64,
}

async fn create_tier(State(state): State<AppState>, Json(req): Json<TierRequest>) -> Response {
    let plan = openproxy_core::credits::PlanCommerce {
        price_usd: req.price_usd,
        credit_usd: req.credit_usd,
        ttl_days: req.ttl_days,
    };
    match state
        .db_pool()
        .spawn_write(move |w| {
            openproxy_core::credits::tiers::create(w, &req.name, &req.config, &plan)
        })
        .await
    {
        Ok(tier) => (StatusCode::CREATED, Json(tier)).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "tier_failed", &format!("{e}")),
    }
}

/// A missing PATCH field is distinct from an explicit JSON null.
#[derive(Debug, Default)]
enum PatchField<T> {
    #[default]
    Missing,
    Present(Option<T>),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for PatchField<T> {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        Option::<T>::deserialize(de).map(Self::Present)
    }
}

impl<T> PatchField<T> {
    fn is_present(&self) -> bool {
        matches!(self, Self::Present(_))
    }

    fn merge(self, current: Option<T>) -> Option<T> {
        match self {
            Self::Missing => current,
            Self::Present(value) => value,
        }
    }
}

#[derive(Debug, Deserialize)]
struct UpdateTierRequest {
    name: Option<String>,
    config: Option<openproxy_core::credits::TierConfig>,
    active: Option<bool>,
    /// Plan half; absent preserves the stored value, `null` clears it.
    #[serde(default)]
    price_usd: PatchField<f64>,
    #[serde(default)]
    credit_usd: PatchField<f64>,
    #[serde(default)]
    ttl_days: PatchField<i64>,
}

async fn update_tier(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateTierRequest>,
) -> Response {
    let touches_plan =
        req.price_usd.is_present() || req.credit_usd.is_present() || req.ttl_days.is_present();
    let res = state
        .db_pool()
        .spawn_write(move |w| {
            let tx =
                rusqlite::Transaction::new_unchecked(&*w, rusqlite::TransactionBehavior::Immediate)
                    .map_err(openproxy_db::error::map_db_error)?;
            // Read and merge under the same transaction, including other processes.
            let commerce = if touches_plan {
                let Some(current) = openproxy_core::credits::tiers::get(&tx, id)? else {
                    return Err(openproxy_types::CoreError::NotFound {
                        what: "credit tier".into(),
                        id: id.to_string(),
                    });
                };
                // Tri-state semantics: absent keeps the stored value, an
                // explicit value replaces it, and an explicit `null` for
                // ttl_days is INVALID (a priced plan without a lifetime
                // cannot be sold). Never invent a default here.
                let ttl_days = match req.ttl_days {
                    PatchField::Missing => current.ttl_days,
                    PatchField::Present(None) => {
                        return Err(openproxy_types::CoreError::Validation(
                            "ttl_days cannot be null for a plan".into(),
                        ));
                    }
                    PatchField::Present(Some(days)) if days <= 0 => {
                        return Err(openproxy_types::CoreError::Validation(
                            "ttl_days must be positive".into(),
                        ));
                    }
                    PatchField::Present(Some(days)) => days,
                };
                Some(openproxy_core::credits::PlanCommerce {
                    price_usd: req.price_usd.merge(current.price_usd),
                    credit_usd: req.credit_usd.merge(current.credit_usd),
                    ttl_days,
                })
            } else {
                None
            };
            openproxy_core::credits::tiers::update(
                &tx,
                id,
                req.name.as_deref(),
                req.config.as_ref(),
                req.active,
                commerce.as_ref(),
            )?;
            tx.commit().map_err(openproxy_db::error::map_db_error)?;
            Ok(())
        })
        .await;
    match res {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "tier_failed", &format!("{e}")),
    }
}

async fn delete_tier(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    match state
        .db_pool()
        .spawn_write(move |w| openproxy_core::credits::tiers::delete(w, id))
        .await
    {
        Ok(()) => Json(json!({"ok": true})).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, "tier_failed", &format!("{e}")),
    }
}

/// The full order ledger for the admin: every checkout, its status, exact
/// amount and chain pointer. Searchable (`q` matches order code, payer email,
/// status or key id) and paginated (`limit`/`offset`, with the filtered total).
async fn list_orders(
    State(state): State<AppState>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(50)
        .clamp(1, 1000);
    let offset = params
        .get("offset")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0)
        .max(0);
    let q = params
        .get("q")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let res = state
        .db_pool()
        .spawn_read(move |r| {
            // Filter: exact status / key id, substring for code and email.
            let like = format!("%{}%", q.replace('%', ""));
            let q_key = q.parse::<i64>().ok();
            let where_clause = if q.is_empty() {
                String::new()
            } else {
                " WHERE (order_code LIKE ?1 OR payer_email LIKE ?1 OR LOWER(status) = LOWER(?1)                  OR (?2 IS NOT NULL AND api_key_id = ?2))".to_string()
            };
            let total: i64 = if q.is_empty() {
                r.query_row("SELECT COUNT(*) FROM credit_orders", [], |row| row.get(0))
                    .map_err(openproxy_db::error::map_db_error)?
            } else {
                r.query_row(
                    &format!("SELECT COUNT(*) FROM credit_orders{where_clause}"),
                    rusqlite::params![like, q_key],
                    |row| row.get(0),
                )
                .map_err(openproxy_db::error::map_db_error)?
            };
            let sql = if q.is_empty() {
                "SELECT o.id, o.order_code, o.api_key_id, o.amount_units, o.base_units, o.ttl_days, \
                 o.status, o.tx_hash, o.tier_id, t.name, o.created_at, o.paid_at, o.credit_units, \
                 o.payer_email \
                 FROM credit_orders o LEFT JOIN credit_tiers t ON t.id = o.tier_id \
                 ORDER BY o.id DESC LIMIT ?1 OFFSET ?2".to_string()
            } else {
                format!(
                    "SELECT o.id, o.order_code, o.api_key_id, o.amount_units, o.base_units, o.ttl_days, \
                     o.status, o.tx_hash, o.tier_id, t.name, o.created_at, o.paid_at, o.credit_units, \
                     o.payer_email \
                     FROM credit_orders o LEFT JOIN credit_tiers t ON t.id = o.tier_id{where_clause} \
                     ORDER BY o.id DESC LIMIT ?3 OFFSET ?4"
                )
            };
            let mut stmt = r.prepare(&sql).map_err(openproxy_db::error::map_db_error)?;
            let map_row = |row: &rusqlite::Row| -> rusqlite::Result<serde_json::Value> {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "order_code": row.get::<_, String>(1)?,
                    "key_id": row.get::<_, Option<i64>>(2)?,
                    "amount_units": row.get::<_, i64>(3)?,
                    "base_units": row.get::<_, i64>(4)?,
                    "ttl_days": row.get::<_, i64>(5)?,
                    "status": row.get::<_, String>(6)?,
                    "tx_hash": row.get::<_, Option<String>>(7)?,
                    "tier_id": row.get::<_, Option<i64>>(8)?,
                    "tier_name": row.get::<_, Option<String>>(9)?,
                    "created_at": row.get::<_, String>(10)?,
                    "paid_at": row.get::<_, Option<String>>(11)?,
                    "credit_units": row.get::<_, Option<i64>>(12)?.unwrap_or(row.get::<_, i64>(4)?),
                    "payer_email": row.get::<_, Option<String>>(13)?,
                }))
            };
            let rows: Vec<serde_json::Value> = if q.is_empty() {
                stmt.query_map(rusqlite::params![limit, offset], map_row)
                    .map_err(openproxy_db::error::map_db_error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(openproxy_db::error::map_db_error)?
            } else {
                stmt.query_map(rusqlite::params![like, q_key, limit, offset], map_row)
                    .map_err(openproxy_db::error::map_db_error)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(openproxy_db::error::map_db_error)?
            };
            Ok(json!({ "orders": rows, "total": total }))
        })
        .await;
    match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "orders_failed",
            &format!("{e}"),
        ),
    }
}

/// Every model and combo the tier editor can allowlist: all of them, active
/// or not (inactive entries are marked, not hidden — a tier may need to keep
/// naming a model a provider currently has off). Models are deduplicated by
/// `model_id` (the same model often appears under many providers).
async fn admin_catalog(State(state): State<AppState>) -> Response {
    let res = state
        .db_pool()
        .spawn_read(|r| {
            let mut models: Vec<serde_json::Value> = Vec::new();
            let mut stmt = r
                .prepare(
                    "SELECT model_id, GROUP_CONCAT(DISTINCT provider_id), MAX(active), COUNT(*) \
                     FROM models GROUP BY model_id ORDER BY model_id",
                )
                .map_err(openproxy_db::error::map_db_error)?;
            let rows = stmt
                .query_map([], |row| {
                    Ok(json!({
                        "kind": "model",
                        "id": row.get::<_, String>(0)?,
                        "provider": row.get::<_, String>(1)?,
                        "active": row.get::<_, i64>(2)? == 1,
                        "providers": row.get::<_, i64>(3)?,
                    }))
                })
                .map_err(openproxy_db::error::map_db_error)?;
            for row in rows {
                models.push(row.map_err(openproxy_db::error::map_db_error)?);
            }
            let mut combos: Vec<serde_json::Value> = Vec::new();
            // combos has no active flag: a combo is selectable while it exists.
            let mut stmt = r
                .prepare("SELECT name FROM combos ORDER BY name")
                .map_err(openproxy_db::error::map_db_error)?;
            let rows = stmt
                .query_map([], |row| {
                    let name: String = row.get(0)?;
                    Ok(json!({
                        "kind": "combo",
                        "id": format!("combo:{name}"),
                        "name": name,
                        "active": true,
                    }))
                })
                .map_err(openproxy_db::error::map_db_error)?;
            for row in rows {
                combos.push(row.map_err(openproxy_db::error::map_db_error)?);
            }
            Ok(json!({ "models": models, "combos": combos }))
        })
        .await;
    match res {
        Ok(v) => Json(v).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "catalog_failed",
            &format!("{e}"),
        ),
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
    let res = state
        .db_pool()
        .spawn_write(move |w| {
            openproxy_db::app_config::save_credits_gateway_enabled_to_db(w, req.enabled, 0)
        })
        .await;
    match res {
        Ok(()) => Json(json!({ "enabled": req.enabled })).into_response(),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            "gateway_failed",
            &format!("{e}"),
        ),
    }
}
