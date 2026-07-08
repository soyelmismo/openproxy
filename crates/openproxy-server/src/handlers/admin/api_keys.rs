use super::*;
use axum::{
    Json,
    extract::{Path, State},
};

pub async fn list_api_keys(
    State(s): State<AppState>,
) -> ApiResult<Json<Vec<core_api_keys::ApiKey>>> {
    crate::api_try! {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let list = core_api_keys::list(&r)?;
        Ok(Json(list))
    }
}

pub async fn create_api_key(
    State(s): State<AppState>,
    Json(body): Json<core_api_keys::CreateApiKeyInput>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let w = s.db_pool().writer();
        let (key, plaintext) = core_api_keys::create(&w, body, "admin")?;
        Ok(Json(serde_json::json!({
            "key": key,
            "plaintext": plaintext,
        })))
    }
}

pub async fn get_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<core_api_keys::ApiKey>> {
    crate::api_try! {
        // Read-only SELECT — use the READER.
        let r = s.db_pool().reader();
        let key = core_api_keys::get_by_id(&r, ApiKeyId(id))?
            .ok_or_else(|| CoreError::Internal(format!("api_key {id} not found")))?;
        Ok(Json(key))
    }
}

pub async fn update_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let label = body.get("label").and_then(|v| v.as_str());

        let scopes_owned: Option<Vec<String>> =
            body.get("scopes").and_then(|v| v.as_array()).map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            });
        let scopes_slice: Option<&[String]> = scopes_owned.as_deref();

        // `allowed_models`: absent = no-op; present + null = clear to NULL;
        // present + array = set to that array.
        let allowed_models_owned: Option<Option<Vec<String>>> =
            body.get("allowed_models").map(|v| {
                if v.is_null() {
                    None
                } else {
                    v.as_array().map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(String::from))
                            .collect()
                    })
                }
            });
        let allowed_models_slice: Option<Option<&[String]>> =
            allowed_models_owned.as_ref().map(|o| o.as_deref());

        let allowed_combos_owned: Option<Option<Vec<i64>>> = body.get("allowed_combos").map(|v| {
            if v.is_null() {
                None
            } else {
                v.as_array()
                    .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
            }
        });
        let allowed_combos_slice: Option<Option<&[i64]>> =
            allowed_combos_owned.as_ref().map(|o| o.as_deref());

        let is_active = body.get("is_active").and_then(|v| v.as_bool());

        let expires_owned: Option<Option<String>> =
            body.get("expires_at").map(|v| v.as_str().map(String::from));
        let expires_slice: Option<Option<&str>> = expires_owned.as_ref().map(|o| o.as_deref());

        let w = s.db_pool().writer();
        core_api_keys::update(
            &w,
            ApiKeyId(id),
            core_api_keys::UpdateParams {
                label,
                scopes: scopes_slice,
                allowed_models: allowed_models_slice,
                allowed_combos: allowed_combos_slice,
                is_active,
                expires_at: expires_slice,
            },
        )?;
        Ok(Json(serde_json::json!({ "id": id })))
    }
}

pub async fn revoke_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let w = s.db_pool().writer();
        core_api_keys::revoke(&w, ApiKeyId(id))?;
        Ok(Json(serde_json::json!({ "id": id, "revoked": true })))
    }
}

pub async fn delete_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let w = s.db_pool().writer();
        core_api_keys::hard_delete(&w, ApiKeyId(id))?;
        Ok(Json(serde_json::json!({ "id": id, "deleted": true })))
    }
}

pub async fn regenerate_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        let w = s.db_pool().writer();
        let (key, plaintext) = core_api_keys::regenerate(&w, ApiKeyId(id))?;
        Ok(Json(serde_json::json!({
            "key": key,
            "plaintext": plaintext,
        })))
    }
}

pub async fn api_key_usage(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> ApiResult<Json<serde_json::Value>> {
    crate::api_try! {
        // Read-only SELECTs (get_by_id, usage_summary, core_usage::summary) —
        // use the READER.
        let r = s.db_pool().reader();

        // Confirm the key exists first so a 404 surfaces here
        // (cleaner) instead of an empty summary that could be
        // confused with "key has no traffic".
        let _ = core_api_keys::get_by_id(&r, ApiKeyId(id))?
            .ok_or_else(|| CoreError::Internal(format!("api_key {id} not found")))?;

        let head = core_api_keys::usage_summary(&r, ApiKeyId(id))?;
        let detailed = core_usage::summary(
            &r,
            &UsageFilter {
                api_key_id: Some(ApiKeyId(id)),
                ..Default::default()
            },
        )?;
        Ok(Json(serde_json::json!({
            "key": head,
            "summary": detailed,
        })))
    }
}
