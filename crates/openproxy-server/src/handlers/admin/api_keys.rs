use super::{ApiError, ApiKeyId, AppState, CoreError, UsageFilter, core_api_keys, core_usage};
use axum::{
    Json,
    extract::{Path, State},
};

pub async fn list_api_keys(
    State(s): State<AppState>,
) -> Result<Json<Vec<core_api_keys::ApiKey>>, ApiError> {
    let list = s.services().api_keys.list()?;
    Ok(Json(list))
}

pub async fn create_api_key(
    State(s): State<AppState>,
    Json(body): Json<core_api_keys::CreateApiKeyInput>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (key, plaintext) = s.services().api_keys.create(body, "admin")?;
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
        .services()
        .api_keys
        .get_by_id(ApiKeyId(id))?
        .ok_or_else(|| CoreError::Internal(format!("api_key {id} not found")))?;
    Ok(Json(key))
}

pub async fn update_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, ApiError> {
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
    let allowed_models_slice =
        allowed_models_owned.as_ref().map(|v| v.as_slice()).into_nested_option();

    let allowed_combos_owned =
        extract_tristate_array(&body, "allowed_combos", serde_json::Value::as_i64);
    let allowed_combos_slice =
        allowed_combos_owned.as_ref().map(|v| v.as_slice()).into_nested_option();

    let blacklisted_providers_owned = extract_tristate_array(&body, "blacklisted_providers", |x| {
        x.as_str().map(String::from)
    });
    let blacklisted_providers_slice =
        blacklisted_providers_owned.as_ref().map(|v| v.as_slice()).into_nested_option();

    let blacklisted_models_owned = extract_tristate_array(&body, "blacklisted_models", |x| {
        x.as_str().map(String::from)
    });
    let blacklisted_models_slice =
        blacklisted_models_owned.as_ref().map(|v| v.as_slice()).into_nested_option();

    let is_active = body.get("is_active").and_then(serde_json::Value::as_bool);

    let expires_field = match body.get("expires_at") {
        None => UpdateField::Ignore,
        Some(v) => match v.as_str() {
            Some(s) => UpdateField::Set(s.to_string()),
            None => UpdateField::Reset,
        },
    };
    let expires_slice = expires_field.as_ref().map(|s| s.as_str()).into_nested_option();

    s.services().api_keys.update(
        ApiKeyId(id),
        core_api_keys::UpdateParams {
            label,
            scopes: scopes_slice,
            allowed_models: allowed_models_slice,
            allowed_combos: allowed_combos_slice,
            blacklisted_providers: blacklisted_providers_slice,
            blacklisted_models: blacklisted_models_slice,
            is_active,
            expires_at: expires_slice,
        },
    )?;
    Ok(Json(serde_json::json!({ "id": id })))
}

crate::admin_entity_action_handler! {
    pub async fn revoke_api_key(
        State(s) with writer(w),
        Path(id): Path<i64>,
    ) -> Result<Json<serde_json::Value>, ApiError> {
        core_api_keys::revoke(&w, ApiKeyId(id))?;
        Ok(Json(serde_json::json!({ "id": id, "revoked": true })))
    }
}

crate::admin_entity_action_handler! {
    pub async fn delete_api_key(
        State(s) with writer(w),
        Path(id): Path<i64>,
    ) -> Result<Json<serde_json::Value>, ApiError> {
        core_api_keys::hard_delete(&w, ApiKeyId(id))?;
        Ok(Json(serde_json::json!({ "id": id, "deleted": true })))
    }
}

pub async fn regenerate_api_key(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let w = s.db_pool().writer();
    let (key, plaintext) = core_api_keys::regenerate(&w, ApiKeyId(id))?;
    Ok(Json(serde_json::json!({
        "key": key,
        "plaintext": plaintext,
    })))
}

pub async fn api_key_usage(
    State(s): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpdateField<T> {
    Ignore,
    Reset,
    Set(T),
}

impl<T> UpdateField<T> {
    pub(crate) fn as_ref(&self) -> UpdateField<&T> {
        match self {
            Self::Ignore => UpdateField::Ignore,
            Self::Reset => UpdateField::Reset,
            Self::Set(v) => UpdateField::Set(v),
        }
    }

    pub(crate) fn map<U>(self, f: impl FnOnce(T) -> U) -> UpdateField<U> {
        match self {
            Self::Ignore => UpdateField::Ignore,
            Self::Reset => UpdateField::Reset,
            Self::Set(v) => UpdateField::Set(f(v)),
        }
    }

    #[allow(clippy::option_option)]
    pub(crate) fn into_nested_option(self) -> Option<Option<T>> {
        match self {
            Self::Ignore => None,
            Self::Reset => Some(None),
            Self::Set(v) => Some(Some(v)),
        }
    }
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
