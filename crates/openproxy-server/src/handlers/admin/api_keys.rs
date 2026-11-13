use super::{ApiError, ApiKeyId, AppState, CoreError, UsageFilter, core_api_keys, core_usage};
use axum::{
    Json,
    extract::{Path, State},
};
use openproxy_types::UpdateField;
use std::sync::Arc;

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/", axum::routing::get(list_api_keys).post(create_api_key))
        .route(
            "/{id}",
            axum::routing::get(get_api_key)
                .patch(update_api_key)
                .delete(delete_api_key),
        )
        .route("/{id}/revoke", axum::routing::post(revoke_api_key))
        .route("/{id}/regenerate", axum::routing::post(regenerate_api_key))
        .route("/{id}/usage", axum::routing::get(api_key_usage))
        .route(
            "/{id}/quota",
            axum::routing::get(get_key_quota),
        )
        .route("/quotas", axum::routing::get(list_key_quotas))
}

/// Privileges and dollar quotas are plan territory: the tier owns them. The
/// admin key editor may only link a key to a tier, adjust its balance, and
/// subtract access via blacklists. Allowlists / the whitelist gate / quotas
/// can never be set directly on a key.
const PLAN_OWNED_FIELDS: [&str; 5] = [
    "allowed_models",
    "allowed_combos",
    "allowed_providers",
    "only_whitelisted",
    "quota",
];

fn reject_plan_owned_fields(body: &serde_json::Value) -> Result<(), ApiError> {
    for field in PLAN_OWNED_FIELDS {
        if body.get(field).is_some() {
            return Err(ApiError(CoreError::Validation(format!(
                "'{field}' is owned by the key's plan (tier): change the tier \
                 or link this key to one with tier_id"
            ))));
        }
    }
    Ok(())
}

pub async fn list_api_keys(
    State(s): State<AppState>,
) -> Result<Json<Vec<core_api_keys::ApiKey>>, ApiError> {
    let list = s.db_pool().spawn_read(core_api_keys::list).await?;
    Ok(Json(list))
}

pub async fn create_api_key(
    State(s): State<AppState>,
    identity: super::auth::Identity,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let input: core_api_keys::CreateApiKeyInput = serde_json::from_value(body.clone())
        .map_err(|e| ApiError(CoreError::Validation(format!("invalid api key body: {e}"))))?;
    reject_plan_owned_fields(&body)?;
    let (key, plaintext) = s
        .db_pool()
        .spawn_write(move |w| core_api_keys::create(w, input, "admin"))
        .await?;
    s.cache_api_key(Arc::new(key.clone()));
    super::auth::audit_secret_read(
        &identity,
        "api_key_created",
        &format!("key:{} scopes:{:?}", key.id.0, key.scopes),
    );
    Ok(Json(serde_json::json!({
        "key": key,
        "plaintext": plaintext,
    })))
}

pub async fn get_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<core_api_keys::ApiKey>, ApiError> {
    let key = s
        .db_pool()
        .spawn_read(move |r| core_api_keys::get_by_id(r, ApiKeyId(id)))
        .await?
        .ok_or_else(|| CoreError::not_found("api_key", id.to_string()))?;
    Ok(Json(key))
}

pub async fn update_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    // Plan fields are not editable here: they come from the tier.
    reject_plan_owned_fields(&body)?;

    // Tri-state tier link: absent = no-op, null = detach, number = link + apply.
    let tier_request: Option<Option<i64>> = match body.get("tier_id") {
        None => None,
        Some(v) if v.is_null() => Some(None),
        Some(v) => Some(Some(v.as_i64().ok_or_else(|| {
            ApiError(CoreError::Validation(
                "tier_id must be a number or null".into(),
            ))
        })?)),
    };

    // Manual balance adjustment: signed units recorded as a ledger adjust.
    let adjust_units: Option<i64> = match body.get("adjust_units") {
        None => None,
        Some(v) => Some(v.as_i64().ok_or_else(|| {
            ApiError(CoreError::Validation(
                "adjust_units must be an integer (units; 100 units = 1 USD)".into(),
            ))
        })?),
    };
    if adjust_units == Some(0) {
        return Err(ApiError(CoreError::Validation(
            "adjust_units must not be zero".into(),
        )));
    }

    let pool = Arc::clone(s.db_pool());
    let state_for_cache = s.clone();
    let _ = crate::error::run_blocking(move || {
        let label = body.get("label").and_then(|v| v.as_str());
        let scopes_owned: Option<Vec<String>> =
            body.get("scopes")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect());
        let scopes_slice: Option<&[String]> = scopes_owned.as_deref();
        let is_active = body.get("is_active").and_then(serde_json::Value::as_bool);
        let blacklisted_providers_owned =
            extract_tristate_array(&body, "blacklisted_providers", |x| x.as_str().map(String::from));
        let blacklisted_providers_slice = blacklisted_providers_owned.as_ref().map(|v| v.as_slice());
        let blacklisted_models_owned =
            extract_tristate_array(&body, "blacklisted_models", |x| x.as_str().map(String::from));
        let blacklisted_models_slice = blacklisted_models_owned.as_ref().map(|v| v.as_slice());
        let expires_field = match body.get("expires_at") {
            None => UpdateField::Ignore,
            Some(v) => match v.as_str() {
                Some(s) => UpdateField::Set(s.to_string()),
                None => UpdateField::Reset,
            },
        };
        let expires_slice = expires_field.as_ref().map(|s| s.as_str());

        s.services().api_keys.update(
            ApiKeyId(id),
            core_api_keys::UpdateParams {
                label,
                scopes: scopes_slice,
                allowed_models: UpdateField::Ignore,
                allowed_combos: UpdateField::Ignore,
                blacklisted_providers: blacklisted_providers_slice,
                allowed_providers: UpdateField::Ignore,
                blacklisted_models: blacklisted_models_slice,
                only_whitelisted: None,
                is_active,
                expires_at: expires_slice,
            },
        )?;
        s.invalidate_api_key_cache(None);
        Ok(())
    })
    .await?;

    // Plan link: applying a tier re-shapes the key (models/combos/caps).
    if let Some(request) = tier_request {
        pool.spawn_write(move |w| match request {
            None => openproxy_core::credits::tiers::set_key_tier(w, ApiKeyId(id), None),
            Some(tier_id) => {
                let tier = openproxy_core::credits::tiers::get(w, tier_id)?
                    .ok_or_else(|| CoreError::not_found("credit_tier", tier_id.to_string()))?;
                openproxy_core::credits::tiers::apply_to_key(w, ApiKeyId(id), &tier.config)?;
                openproxy_core::credits::tiers::set_key_tier(w, ApiKeyId(id), Some(tier.id))
            }
        })
        .await?;
        // The tier just reshaped the key (allowlists, caps). The cache was
        // invalidated before this write, so drop it again or the catalog keeps
        // serving the pre-plan shape until the entry expires.
        state_for_cache.invalidate_api_key_cache(None);
    }

    // Manual balance adjustment through the ledger. A positive delta becomes
    // an `adjust` lot (spendable, stamped with the plan TTL when the key has a
    // tier); a negative delta consumes existing lots FIFO. `adjust` stays out
    // of `kind = 'topup'` on purpose: the reserve guard reads topups as real
    // customer money and would fund the escrow against a manual grant.
    if let Some(delta) = adjust_units {
        pool.spawn_write(move |w| {
            if delta > 0 {
                w.execute(
                    "INSERT INTO credit_transactions \
                         (api_key_id, kind, amount_units, remaining_units, status, expires_at) \
                     SELECT ?1, 'adjust', ?2, ?2, 'active', \
                            CASE WHEN t.ttl_days > 0 \
                                 THEN datetime('now', '+' || t.ttl_days || ' days') END \
                     FROM api_keys k LEFT JOIN credit_tiers t ON t.id = k.tier_id \
                     WHERE k.id = ?1",
                    rusqlite::params![id, delta],
                )
                .map_err(openproxy_db::error::map_db_error)?;
            } else {
                openproxy_core::credits::ledger::record_spend(
                    w,
                    ApiKeyId(id),
                    delta.saturating_neg(),
                )?;
            }
            Ok(())
        })
        .await?;
        state_for_cache.invalidate_api_key_cache(None);
    }

    Ok(Json(serde_json::json!({ "id": id })))
}

pub async fn get_key_quota(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let out = crate::extractors::DbReader(Arc::clone(s.db_pool()))
        .run(move |r| fetch_key_quota(r, id))
        .await?;
    Ok(Json(out))
}

fn fetch_key_quota_list(r: &rusqlite::Connection) -> Result<serde_json::Value, ApiError> {
    let mut stmt = r
        .prepare_cached(
            "SELECT q.api_key_id, k.label, q.weekly_usd_cap, q.five_hour_usd_cap \
             FROM api_key_quotas q JOIN api_keys k ON k.id = q.api_key_id \
             ORDER BY q.updated_at DESC, q.api_key_id DESC",
        )
        .map_err(|e| ApiError(openproxy_db::error::map_db_error(e)))?;
    let mapped = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<f64>>(2)?,
                row.get::<_, Option<f64>>(3)?,
            ))
        })
        .map_err(|e| ApiError(openproxy_db::error::map_db_error(e)))?;
    let mut out = Vec::new();
    for row in mapped {
        let (api_key_id, label, weekly, five_hour) =
            row.map_err(|e| ApiError(openproxy_db::error::map_db_error(e)))?;
        out.push(serde_json::json!({
            "api_key_id": api_key_id,
            "label": label,
            "weekly_usd_cap": weekly,
            "five_hour_usd_cap": five_hour,
        }));
    }
    Ok(serde_json::json!({ "quotas": out }))
}

fn fetch_key_quota(r: &rusqlite::Connection, id: i64) -> Result<serde_json::Value, ApiError> {
    let key_id = ApiKeyId(id);
    if core_api_keys::get_by_id(r, key_id)?.is_none() {
        return Err(ApiError(CoreError::not_found("api_key", id.to_string())));
    }
    let caps = openproxy_core::api_key_quotas::get_caps(r, key_id)?;
    let spend = openproxy_core::api_key_quotas::status(r, key_id)?;
    Ok(serde_json::json!({
        "api_key_id": id,
        // Effective caps: a row with both windows NULL limits nothing.
        "enabled": caps.as_ref().is_some_and(|c| !c.is_empty()),
        "caps": caps,
        "spend": spend,
    }))
}

pub async fn list_key_quotas(
    State(s): State<AppState>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let out = crate::extractors::DbReader(Arc::clone(s.db_pool()))
        .run(fetch_key_quota_list)
        .await?;
    Ok(Json(out))
}

crate::admin_entity_action_handler! {
    pub async fn revoke_api_key(
        State(s) with writer(w),
        Path(id): Path<i64>,
    ) -> Result<Json<serde_json::Value>, ApiError> {
        core_api_keys::revoke(&w, ApiKeyId(id))?;
        s.invalidate_api_key_cache(None);
        Ok(Json(serde_json::json!({ "id": id, "revoked": true })))
    }
}

crate::admin_entity_action_handler! {
    pub async fn delete_api_key(
        State(s) with writer(w),
        Path(id): Path<i64>,
    ) -> Result<Json<serde_json::Value>, ApiError> {
        core_api_keys::hard_delete(&w, ApiKeyId(id))?;
        s.invalidate_api_key_cache(None);
        Ok(Json(serde_json::json!({ "id": id, "deleted": true })))
    }
}

pub async fn regenerate_api_key(
    State(s): State<AppState>,
    identity: super::auth::Identity,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (key, plaintext) = s
        .db_pool()
        .spawn_write(move |w| core_api_keys::regenerate(w, ApiKeyId(id)))
        .await?;
    s.invalidate_api_key_cache(None);
    super::auth::audit_secret_read(&identity, "api_key_regenerated", &format!("key:{id}"));
    Ok(Json(serde_json::json!({
        "key": key,
        "plaintext": plaintext,
    })))
}

fn fetch_api_key_usage(r: &rusqlite::Connection, id: i64) -> Result<serde_json::Value, ApiError> {
    let key_id = ApiKeyId(id);
    if core_api_keys::get_by_id(r, key_id)?.is_none() {
        return Err(ApiError(CoreError::not_found("api_key", id.to_string())));
    }
    let head = core_api_keys::usage_summary(r, key_id)?;
    let detailed = core_usage::summary(
        r,
        &UsageFilter {
            api_key_id: Some(key_id),
            ..Default::default()
        },
    )?;
    Ok(serde_json::json!({
        "key": head,
        "summary": detailed,
    }))
}

pub async fn api_key_usage(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let usage = crate::extractors::DbReader(Arc::clone(s.db_pool()))
        .run(move |r| fetch_api_key_usage(r, id))
        .await?;
    Ok(Json(usage))
}

fn extract_tristate_array<T>(
    body: &serde_json::Value,
    key: &str,
    extractor: impl Fn(&serde_json::Value) -> Option<T>,
) -> UpdateField<Vec<T>> {
    match body.get(key) {
        None => UpdateField::Ignore,
        Some(v) if v.is_null() => UpdateField::Reset,
        Some(v) => match v.as_array() {
            Some(a) => UpdateField::Set(a.iter().filter_map(&extractor).collect()),
            None => UpdateField::Reset,
        },
    }
}
