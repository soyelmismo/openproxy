use super::{ApiError, ApiKeyId, AppState, CoreError, UsageFilter, core_api_keys, core_usage};
use axum::{
    Json,
    extract::{Path, State},
};
use openproxy_types::UpdateField;
#[cfg(feature = "payments")]
use rusqlite::OptionalExtension;
use std::sync::Arc;

pub fn router() -> axum::Router<AppState> {
    let router = axum::Router::new()
        .route("/", axum::routing::get(list_api_keys).post(create_api_key))
        .route("/kinds", axum::routing::get(list_key_kinds))
        .route(
            "/{id}",
            axum::routing::get(get_api_key)
                .patch(update_api_key)
                .delete(delete_api_key),
        )
        .route("/{id}/kind", axum::routing::post(set_key_kind))
        .route("/{id}/revoke", axum::routing::post(revoke_api_key))
        .route("/{id}/regenerate", axum::routing::post(regenerate_api_key))
        .route("/{id}/usage", axum::routing::get(api_key_usage));
    // Plan quota windows only exist with payments (api_key_quotas tables).
    #[cfg(feature = "payments")]
    let router = router
        .route("/{id}/quota", axum::routing::get(get_key_quota))
        .route("/quotas", axum::routing::get(list_key_quotas));
    router
}

/// Privileges are plan territory: the tier owns them. The admin key editor
/// may only link a key to a tier, adjust its balance, and subtract access
/// via blacklists. Allowlists / the whitelist gate can never be set directly
/// on a key. Dollar caps follow the same rule ONLY while a plan is linked:
/// a planless key may carry custom caps (`quota`), and detaching a plan
/// hands cap ownership back to the key.
///
/// Payments-disabled build: there are no plans, so nothing is plan-owned and
/// the editor simply has no tier flow.
#[cfg(feature = "payments")]
const PLAN_OWNED_FIELDS: [&str; 4] = [
    "allowed_models",
    "allowed_combos",
    "allowed_providers",
    "only_whitelisted",
];

#[cfg(not(feature = "payments"))]
const PLAN_OWNED_FIELDS: [&str; 0] = [];

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

/// Non-payments build: the `api_key_quotas` table is not embedded, so a
/// `quota` field in a key body can only be a caller mistake — reject it
/// instead of silently ignoring it.
#[cfg(not(feature = "payments"))]
fn reject_quota_unsupported(body: &serde_json::Value) -> Result<(), ApiError> {
    if body.get("quota").is_some() {
        return Err(ApiError(CoreError::Validation(
            "'quota' requires the payments build".into(),
        )));
    }
    Ok(())
}

/// Parse the optional `quota` field of a create/update body into a tri-state:
/// absent → `None` (no-op), null → `Some(None)` (clear the caps), object →
/// `Some(Some(caps))`. Each cap must be a positive number or null; unknown
/// keys are rejected so a typo cannot silently disable a cap.
#[cfg(feature = "payments")]
fn parse_quota_field(
    body: &serde_json::Value,
) -> Result<Option<Option<openproxy_core::api_key_quotas::QuotaCaps>>, ApiError> {
    use openproxy_core::api_key_quotas::QuotaCaps;
    let Some(v) = body.get("quota") else {
        return Ok(None);
    };
    if v.is_null() {
        return Ok(Some(None));
    }
    let obj = v.as_object().ok_or_else(|| {
        ApiError(CoreError::Validation(
            "quota must be null or an object {weekly_usd_cap?, five_hour_usd_cap?}".into(),
        ))
    })?;
    let cap = |key: &str| -> Result<Option<f64>, ApiError> {
        match obj.get(key) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(n) => {
                let f = n.as_f64().ok_or_else(|| {
                    ApiError(CoreError::Validation(format!(
                        "quota.{key} must be a positive number or null"
                    )))
                })?;
                if !f.is_finite() || f <= 0.0 {
                    return Err(ApiError(CoreError::Validation(format!(
                        "quota.{key} must be a positive number or null"
                    ))));
                }
                Ok(Some(f))
            }
        }
    };
    for key in obj.keys() {
        if key != "weekly_usd_cap" && key != "five_hour_usd_cap" {
            return Err(ApiError(CoreError::Validation(format!(
                "unknown quota field '{key}' (allowed: weekly_usd_cap, five_hour_usd_cap)"
            ))));
        }
    }
    Ok(Some(Some(QuotaCaps {
        weekly_usd_cap: cap("weekly_usd_cap")?,
        five_hour_usd_cap: cap("five_hour_usd_cap")?,
    })))
}

pub async fn list_api_keys(
    State(s): State<AppState>,
) -> Result<Json<Vec<core_api_keys::ApiKey>>, ApiError> {
    let list = s.db_pool().spawn_read(core_api_keys::list).await?;
    Ok(Json(list))
}

/// Customer/personal classification for every key. A separate, tiny map keeps
/// the key-list payload unchanged and lets both dashboard views switch on the
/// same source.
pub async fn list_key_kinds(
    State(s): State<AppState>,
) -> Result<Json<Vec<core_api_keys::kind::ApiKeyKindRow>>, ApiError> {
    let rows = s.db_pool().spawn_read(core_api_keys::kind::list).await?;
    Ok(Json(rows))
}

/// Reclassify one key. Customer keys are the ones sold through the checkout,
/// personal keys are the operator's own, and nothing else distinguishes them.
pub async fn set_key_kind(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let kind = body
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            ApiError(CoreError::Validation(
                "kind must be a string: 'personal' or 'customer'".into(),
            ))
        })?
        .to_owned();
    let stored = s
        .db_pool()
        .spawn_write(move |w| core_api_keys::kind::set(w, id, &kind))
        .await?;
    Ok(Json(serde_json::json!({ "id": id, "kind": stored })))
}

pub async fn create_api_key(
    State(s): State<AppState>,
    identity: super::auth::Identity,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let input: core_api_keys::CreateApiKeyInput = serde_json::from_value(body.clone())
        .map_err(|e| ApiError(CoreError::Validation(format!("invalid api key body: {e}"))))?;
    reject_plan_owned_fields(&body)?;
    #[cfg(not(feature = "payments"))]
    reject_quota_unsupported(&body)?;
    // A brand-new key has no plan, so custom caps always apply. Parse before
    // creating: a malformed quota must not leave a half-provisioned key.
    #[cfg(feature = "payments")]
    let quota_request = parse_quota_field(&body)?;
    let (key, plaintext) = s
        .db_pool()
        .spawn_write(move |w| core_api_keys::create(w, input, "admin"))
        .await?;
    #[cfg(feature = "payments")]
    if let Some(Some(caps)) = quota_request {
        let key_id = key.id;
        s.db_pool()
            .spawn_write(move |w| openproxy_core::api_key_quotas::upsert(w, key_id, &caps))
            .await?;
    }
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
    #[cfg(not(feature = "payments"))]
    reject_quota_unsupported(&body)?;

    // Tri-state tier link: absent = no-op, null = detach, number = link + apply.
    #[cfg(feature = "payments")]
    let tier_request: Option<Option<i64>> = match body.get("tier_id") {
        None => None,
        Some(v) if v.is_null() => Some(None),
        Some(v) => Some(Some(v.as_i64().ok_or_else(|| {
            ApiError(CoreError::Validation(
                "tier_id must be a number or null".into(),
            ))
        })?)),
    };

    // Tri-state custom caps: absent = no-op, null = clear (unlimited), object
    // = upsert. Only planless keys may carry them — see the apply block below.
    #[cfg(feature = "payments")]
    let quota_request = parse_quota_field(&body)?;

    // Validate BEFORE any write: a rejected quota+link request must not leave
    // the tier linked (and its credit granted) behind a 400.
    #[cfg(feature = "payments")]
    if quota_request.is_some() && matches!(tier_request, Some(Some(_))) {
        return Err(ApiError(CoreError::Validation(
            "quota cannot be set while linking a plan: the tier owns the caps. \
             Detach the plan first (tier_id: null) to set custom caps."
                .into(),
        )));
    }

    #[cfg(feature = "payments")]
    let pool = Arc::clone(s.db_pool());
    #[cfg(feature = "payments")]
    let state_for_cache = s.clone();
    crate::error::run_blocking(move || {
        let label = body.get("label").and_then(|v| v.as_str());
        let scopes_owned: Option<Vec<String>> =
            body.get("scopes").and_then(|v| v.as_array()).map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            });
        let scopes_slice: Option<&[String]> = scopes_owned.as_deref();
        let is_active = body.get("is_active").and_then(serde_json::Value::as_bool);
        let blacklisted_providers_owned =
            extract_tristate_array(&body, "blacklisted_providers", |x| {
                x.as_str().map(String::from)
            });
        let blacklisted_providers_slice =
            blacklisted_providers_owned.as_ref().map(|v| v.as_slice());
        let blacklisted_models_owned = extract_tristate_array(&body, "blacklisted_models", |x| {
            x.as_str().map(String::from)
        });
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

    // Plan assignment + custom caps: ONE transaction when both arrive (the
    // detach+caps handover must not split — a failed quota write must not
    // leave a detached key carrying the plan's stale caps).
    #[cfg(feature = "payments")]
    if tier_request.is_some() || quota_request.is_some() {
        pool.spawn_write(move |w| {
            if let Some(request) = tier_request {
                match request {
                    None => openproxy_core::credits::tiers::set_key_tier(w, ApiKeyId(id), None)?,
                    Some(tier_id) => {
                        let tier = openproxy_core::credits::tiers::get(w, tier_id)?
                            .ok_or_else(|| {
                                CoreError::not_found("credit_tier", tier_id.to_string())
                            })?;
                        openproxy_core::credits::tiers::apply_to_key(w, ApiKeyId(id), &tier.config)?;
                        openproxy_core::credits::tiers::set_key_tier(w, ApiKeyId(id), Some(tier.id))?;
                        openproxy_core::credits::tiers::grant_plan(w, ApiKeyId(id), &tier)?;
                    }
                }
            }
            if let Some(quota) = quota_request {
                // Read the tier link as Option<Option<i64>>: outer None = the
                // key does not exist (clean 404 instead of the FK violation an
                // upsert against a missing key would raise); inner None = the
                // key exists and is planless. `row.get(0)` typed as i64 breaks
                // on planless keys — the tier_id column is NULL there.
                let row_tier: Option<Option<i64>> = w
                    .query_row(
                        "SELECT tier_id FROM api_keys WHERE id = ?1",
                        [id],
                        |row| row.get::<_, Option<i64>>(0),
                    )
                    .optional()
                    .map_err(openproxy_db::error::map_db_error)?;
                let current_tier =
                    row_tier.ok_or_else(|| CoreError::not_found("api_key", id.to_string()))?;
                // A plan linked in THIS request owns the caps (attach+quota
                // was rejected before any write). Detach+quota is the
                // sanctioned handover: the plan goes away and the key keeps
                // (or loses) whatever caps the form carries. quota on an
                // already-tiered key is refused here, inside the transaction.
                if !matches!(tier_request, Some(None)) && current_tier.is_some() {
                    return Err(CoreError::Validation(
                        "this key has a plan: its dollar caps come from the tier. \
                         Detach the plan (tier_id: null) to set custom caps."
                            .into(),
                    ));
                }
                match &quota {
                    Some(caps) => openproxy_core::api_key_quotas::upsert(w, ApiKeyId(id), caps)?,
                    None => openproxy_core::api_key_quotas::clear(w, ApiKeyId(id))?,
                }
            }
            Ok(())
        })
        .await?;
        // The tier reshaped the key (allowlists, caps, balance) and quota may
        // have moved. The cache was invalidated before these writes, so drop
        // it again or the catalog keeps serving the pre-write shape until the
        // entry expires.
        state_for_cache.invalidate_api_key_cache(None);
    }

    Ok(Json(serde_json::json!({ "id": id })))
}

/// GET /admin/api/keys/{id}/quota — plan window caps + live spend.
#[cfg(feature = "payments")]
pub async fn get_key_quota(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let out = crate::extractors::DbReader(Arc::clone(s.db_pool()))
        .run(move |r| fetch_key_quota(r, id))
        .await?;
    Ok(Json(out))
}

#[cfg(feature = "payments")]
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

#[cfg(feature = "payments")]
fn fetch_key_quota(r: &rusqlite::Connection, id: i64) -> Result<serde_json::Value, ApiError> {
    let key_id = ApiKeyId(id);
    if core_api_keys::get_by_id(r, key_id)?.is_none() {
        return Err(ApiError(CoreError::not_found("api_key", id.to_string())));
    }
    // The ApiKey DTO does not carry the tier link; the editor needs it to
    // know whether the caps below are the plan's or the key's own.
    let tier_id: Option<i64> = r
        .query_row(
            "SELECT tier_id FROM api_keys WHERE id = ?1",
            [id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
        .map_err(openproxy_db::error::map_db_error)?
        .flatten();
    let caps = openproxy_core::api_key_quotas::get_caps(r, key_id)?;
    let spend = openproxy_core::api_key_quotas::status(r, key_id)?;
    Ok(serde_json::json!({
        "api_key_id": id,
        "tier_id": tier_id,
        // Effective caps: a row with both windows NULL limits nothing.
        "enabled": caps.as_ref().is_some_and(|c| !c.is_empty()),
        "caps": caps,
        "spend": spend,
    }))
}

#[cfg(feature = "payments")]
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

#[cfg(all(test, feature = "payments"))]
mod quota_field_tests {
    use super::*;
    use openproxy_core::api_key_quotas::QuotaCaps;

    #[test]
    fn absent_quota_is_a_noop() {
        assert!(parse_quota_field(&serde_json::json!({})).unwrap().is_none());
        assert!(parse_quota_field(&serde_json::json!({"label": "x"}))
            .unwrap()
            .is_none());
    }

    #[test]
    fn null_quota_requests_a_clear() {
        assert_eq!(
            parse_quota_field(&serde_json::json!({"quota": null})).unwrap(),
            Some(None)
        );
    }

    #[test]
    fn caps_parse_with_null_windows() {
        let out = parse_quota_field(&serde_json::json!({
            "quota": {"weekly_usd_cap": 7.5, "five_hour_usd_cap": null}
        }))
        .unwrap();
        assert_eq!(
            out,
            Some(Some(QuotaCaps {
                weekly_usd_cap: Some(7.5),
                five_hour_usd_cap: None,
            }))
        );
    }

    #[test]
    fn zero_negative_and_non_numeric_caps_are_rejected() {
        for bad in [
            serde_json::json!(0.0),
            serde_json::json!(-1.0),
            serde_json::json!("5"),
            serde_json::json!(true),
        ] {
            let body = serde_json::json!({ "quota": { "weekly_usd_cap": bad } });
            assert!(parse_quota_field(&body).is_err(), "bad cap must be rejected");
        }
    }

    #[test]
    fn unknown_quota_keys_are_rejected() {
        let body = serde_json::json!({ "quota": { "monthly_usd_cap": 10.0 } });
        assert!(parse_quota_field(&body).is_err());
    }

    #[test]
    fn non_object_quota_is_rejected() {
        assert!(parse_quota_field(&serde_json::json!({"quota": 5})).is_err());
        assert!(parse_quota_field(&serde_json::json!({"quota": []})).is_err());
    }

    #[test]
    fn quota_fetch_for_a_planless_key_returns_null_tier() {
        let temp_dir = openproxy_db::testing::TempDir::new("quota-fetch-test").expect("mkdir");
        let mut conn = rusqlite::Connection::open(temp_dir.path().join("test.db")).expect("open");
        openproxy_db::migrations::run(&mut conn).expect("migrations");
        let (key, _plaintext) = core_api_keys::create(
            &conn,
            openproxy_core::api_keys::CreateApiKeyInput {
                label: Some("quota-fetch".into()),
                scopes: vec!["chat".into()],
                ..Default::default()
            },
            "test",
        )
        .expect("create key");
        let caps = QuotaCaps {
            weekly_usd_cap: Some(7.5),
            five_hour_usd_cap: None,
        };
        openproxy_core::api_key_quotas::upsert(&conn, key.id, &caps).expect("upsert");

        let out = fetch_key_quota(&conn, key.id.0).expect("fetch must not fail on NULL tier_id");
        assert_eq!(out["tier_id"], serde_json::Value::Null);
        assert_eq!(out["enabled"], serde_json::json!(true));
        assert_eq!(out["caps"]["weekly_usd_cap"], serde_json::json!(7.5));
        assert_eq!(out["caps"]["five_hour_usd_cap"], serde_json::Value::Null);
    }
}
