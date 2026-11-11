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
            axum::routing::get(get_key_quota)
                .put(put_key_quota)
                .delete(delete_key_quota),
        )
        .route("/quotas", axum::routing::get(list_key_quotas))
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
    // Optional dollar quotas ride along in the same request body.
    let caps = match parse_quota_body(&body)? {
        QuotaBody::Set(caps) => Some(caps),
        QuotaBody::Ignore | QuotaBody::Remove => None,
    };
    if let Some(caps) = &caps {
        let key_id = key.id;
        let caps_for_db = caps.clone();
        s.db_pool()
            .spawn_write(move |w| openproxy_core::api_key_quotas::upsert(w, key_id, &caps_for_db))
            .await?;
    }
    Ok(Json(serde_json::json!({
        "key": key,
        "plaintext": plaintext,
        "quota": caps,
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
    // Parse quota intent before `body` moves into the blocking closure, and
    // clone the pool before `s` moves: the quota write happens afterwards.
    let quota_body = parse_quota_body(&body)?;
    let pool = Arc::clone(s.db_pool());
    let _ = crate::error::run_blocking(move || {
        let label = body.get("label").and_then(|v| v.as_str());

        let scopes_owned: Option<Vec<String>> =
            body.get("scopes").and_then(|v| v.as_array()).map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            });
        let scopes_slice: Option<&[String]> = scopes_owned.as_deref();

        // Tri-state array fields: absent = no-op; present + null = clear to NULL;
        // present + array = set to that array.
        let allowed_models_owned =
            extract_tristate_array(&body, "allowed_models", |x| x.as_str().map(String::from));
        let allowed_models_slice = allowed_models_owned.as_ref().map(|v| v.as_slice());

        let allowed_combos_owned =
            extract_tristate_array(&body, "allowed_combos", serde_json::Value::as_i64);
        let allowed_combos_slice = allowed_combos_owned.as_ref().map(|v| v.as_slice());

        let blacklisted_providers_owned =
            extract_tristate_array(&body, "blacklisted_providers", |x| {
                x.as_str().map(String::from)
            });
        let blacklisted_providers_slice =
            blacklisted_providers_owned.as_ref().map(|v| v.as_slice());

        let allowed_providers_owned =
            extract_tristate_array(&body, "allowed_providers", |x| x.as_str().map(String::from));
        let allowed_providers_slice = allowed_providers_owned.as_ref().map(|v| v.as_slice());

        let blacklisted_models_owned = extract_tristate_array(&body, "blacklisted_models", |x| {
            x.as_str().map(String::from)
        });
        let blacklisted_models_slice = blacklisted_models_owned.as_ref().map(|v| v.as_slice());

        let only_whitelisted = body
            .get("only_whitelisted")
            .and_then(serde_json::Value::as_bool);

        let is_active = body.get("is_active").and_then(serde_json::Value::as_bool);

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
                allowed_models: allowed_models_slice,
                allowed_combos: allowed_combos_slice,
                blacklisted_providers: blacklisted_providers_slice,
                allowed_providers: allowed_providers_slice,
                blacklisted_models: blacklisted_models_slice,
                only_whitelisted,
                is_active,
                expires_at: expires_slice,
            },
        )?;
        s.invalidate_api_key_cache(None);
        Ok(Json(serde_json::json!({ "id": id })))
    })
    .await?;

    // Optional dollar quotas: present object = upsert, explicit null = remove,
    // absent = leave untouched.
    match quota_body {
        QuotaBody::Ignore => {}
        QuotaBody::Remove => {
            pool.spawn_write(move |w| openproxy_core::api_key_quotas::delete(w, ApiKeyId(id)))
                .await?;
        }
        QuotaBody::Set(caps) => {
            pool.spawn_write(move |w| {
                openproxy_core::api_key_quotas::upsert(w, ApiKeyId(id), &caps)
            })
            .await?;
        }
    }
    Ok(Json(serde_json::json!({ "id": id })))
}

/// Parsed `quota` field from a request body.
enum QuotaBody {
    /// `quota` absent — don't touch existing caps.
    Ignore,
    /// `quota: null` — remove caps (key becomes unlimited).
    Remove,
    /// `quota: {...}` — upsert caps (null members = disabled windows).
    Set(openproxy_core::api_key_quotas::QuotaCaps),
}

fn parse_quota_body(body: &serde_json::Value) -> Result<QuotaBody, ApiError> {
    let Some(q) = body.get("quota") else {
        return Ok(QuotaBody::Ignore);
    };
    if q.is_null() {
        return Ok(QuotaBody::Remove);
    }
    let obj = q.as_object().ok_or_else(|| {
        ApiError(CoreError::Validation(
            "quota must be an object or null".into(),
        ))
    })?;
    let num = |key: &str| -> Result<Option<f64>, ApiError> {
        match obj.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(serde_json::Value::Number(n)) => {
                let v = n.as_f64().ok_or_else(|| {
                    ApiError(CoreError::Validation(format!(
                        "quota.{key} must be a number"
                    )))
                })?;
                if !v.is_finite() || v < 0.0 {
                    return Err(ApiError(CoreError::Validation(format!(
                        "quota.{key} must be >= 0"
                    ))));
                }
                Ok(Some(v))
            }
            Some(_) => Err(ApiError(CoreError::Validation(format!(
                "quota.{key} must be a number or null"
            )))),
        }
    };
    Ok(QuotaBody::Set(
        openproxy_core::api_key_quotas::QuotaCaps {
            monthly_usd_cap: num("monthly_usd_cap")?,
            weekly_usd_cap: num("weekly_usd_cap")?,
            five_hour_usd_cap: num("five_hour_usd_cap")?,
        }
        .sanitized(),
    ))
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
        "enabled": caps.is_some(),
        "caps": caps,
        "spend": spend,
    }))
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

pub async fn put_key_quota(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    match parse_quota_body(&body)? {
        QuotaBody::Ignore => {
            return Err(ApiError(CoreError::Validation(
                "quota field is required (object or null)".into(),
            )));
        }
        QuotaBody::Remove => {
            s.db_pool()
                .spawn_write(move |w| openproxy_core::api_key_quotas::delete(w, ApiKeyId(id)))
                .await?;
        }
        QuotaBody::Set(caps) => {
            s.db_pool()
                .spawn_write(move |w| {
                    openproxy_core::api_key_quotas::upsert(w, ApiKeyId(id), &caps)
                })
                .await?;
        }
    }
    Ok(Json(serde_json::json!({ "id": id, "quota_set": true })))
}

pub async fn delete_key_quota(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    s.db_pool()
        .spawn_write(move |w| openproxy_core::api_key_quotas::delete(w, ApiKeyId(id)))
        .await?;
    Ok(Json(serde_json::json!({ "id": id, "quota_set": false })))
}

/// Keys that carry a quota row, labels joined for the analytics selector.
fn fetch_key_quota_list(r: &rusqlite::Connection) -> Result<serde_json::Value, ApiError> {
    let mut stmt = r
        .prepare_cached(
            "SELECT q.api_key_id, k.label, q.monthly_usd_cap, q.weekly_usd_cap, q.five_hour_usd_cap \
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
                row.get::<_, Option<f64>>(4)?,
            ))
        })
        .map_err(|e| ApiError(openproxy_db::error::map_db_error(e)))?;
    let mut out = Vec::new();
    for row in mapped {
        let (api_key_id, label, monthly, weekly, five_hour) =
            row.map_err(|e| ApiError(openproxy_db::error::map_db_error(e)))?;
        out.push(serde_json::json!({
            "api_key_id": api_key_id,
            "label": label,
            "monthly_usd_cap": monthly,
            "weekly_usd_cap": weekly,
            "five_hour_usd_cap": five_hour,
        }));
    }
    Ok(serde_json::json!({ "quotas": out }))
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
