//! Provider administration service layer.

use crate::error::{CoreError, Result};
use crate::ids::ProviderId;
use crate::providers::{self, AuthType, ProviderFormat};
use crate::validation::{Validatable, validate_base_url};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Inputs for [`create_provider`]. `auth_type` and `format` arrive as wire strings
/// (e.g. `"bearer"`, `"openai"`) and are parsed into typed enums at the boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateProviderInput {
    pub id: String,
    pub name: String,
    pub base_url: String,
    pub auth_type: String,
    pub format: String,
    pub extra_headers_json: Option<String>,
    pub rate_limit_scope: Option<crate::providers::RateLimitScope>,
}

impl Validatable for CreateProviderInput {
    fn validate(&self) -> Result<()> {
        validate_base_url(&self.base_url)?;
        AuthType::parse(&self.auth_type).map_err(CoreError::Validation)?;
        ProviderFormat::parse(&self.format).map_err(CoreError::Validation)?;
        Ok(())
    }
}

/// Insert a new provider, returning the [`ProviderId`] used. Unknown
/// `auth_type` / `format` or a duplicate id surface as [`CoreError::Validation`].
pub fn create_provider(conn: &Connection, input: CreateProviderInput) -> Result<ProviderId> {
    input.validate()?;
    validate_base_url(&input.base_url)?;
    let id = ProviderId::new(input.id);
    let auth = AuthType::parse(&input.auth_type).map_err(CoreError::Validation)?;
    let format = ProviderFormat::parse(&input.format).map_err(CoreError::Validation)?;
    providers::create(
        conn,
        providers::NewProvider {
            id: &id,
            name: &input.name,
            base_url: &input.base_url,
            auth_type: auth,
            format,
            extra_headers_json: input.extra_headers_json.as_deref(),
            auto_activate_keyword: None,
            rate_limit_scope: input
                .rate_limit_scope
                .unwrap_or(crate::providers::RateLimitScope::Account),
        },
    )?;
    Ok(id)
}

/// Every provider.
pub fn list_providers(conn: &Connection) -> Result<Vec<providers::Provider>> {
    providers::list(conn)
}

/// Delete a custom provider by id. Idempotent: a missing id is a no-op.
///
/// Built-in providers (see [`crate::seed::builtin_provider_ids`]) are rejected
/// with [`CoreError::Validation`], since removing the row would leave dangling
/// references in [`openproxy_adapters::adapters::builtin_adapters`]. Deactivating
/// via [`set_provider_active`] is the soft, reversible alternative.
pub fn delete_provider(conn: &Connection, id: &ProviderId) -> Result<()> {
    if crate::seed::is_builtin(id.as_str()) {
        return Err(CoreError::Validation(format!(
            "provider '{id}' is a built-in and cannot be deleted. Use POST \
             /admin/providers/{id}/active with {{\"active\": false}} to \
             deactivate it instead."
        )));
    }
    providers::delete(conn, id)
}

/// Flip a provider's soft-disable flag. A deactivated provider keeps its row,
/// accounts and models but drops out of combo-target resolution; reactivating
/// restores the targets. A missing id is a silent no-op.
pub fn set_provider_active(conn: &Connection, id: &ProviderId, active: bool) -> Result<()> {
    providers::set_active(conn, id, active)
}

/// Inputs for [`update_provider`]. Every field is optional. `extra_headers_json` is
/// the raw JSON string to store, validated only at apply time.
///
/// `auto_activate_keyword` and `notif_keyword_only` use a three-state encoding:
/// `None` leaves the column alone, `Some(None)` sets it to `NULL`, `Some(Some(s))`
/// sets the literal. The custom deserializer is what makes this work over JSON,
/// where the default `Option<Option<T>>` impl would fold `null` and absent into
/// the same `None` and lose the "clear" semantic.
#[derive(Debug, Clone, Serialize, Default)]
pub struct UpdateProviderInput {
    pub name: Option<String>,
    pub base_url: Option<String>,
    pub extra_headers_json: Option<Option<String>>,
    pub auto_activate_keyword: Option<Option<String>>,
    pub use_proxies: Option<bool>,
    pub proxy_rotation_errors: Option<String>,
    pub proxy_rotation_mode: Option<String>,
    pub rate_limit_scope: Option<crate::providers::RateLimitScope>,
    /// Toggle for `providers.notif_keyword_only` (migration 000074). A missing key
    /// is a no-op, `true` / `false` sets the flag, explicit `null` normalises to 0
    /// since the column is `NOT NULL DEFAULT 0`.
    pub notif_keyword_only: Option<Option<bool>>,
}

impl Validatable for UpdateProviderInput {
    fn validate(&self) -> Result<()> {
        if let Some(ref url) = self.base_url {
            validate_base_url(url)?;
        }
        Ok(())
    }
}

impl<'de> Deserialize<'de> for UpdateProviderInput {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(field_identifier, rename_all = "snake_case")]
        enum Field {
            Name,
            BaseUrl,
            ExtraHeadersJson,
            AutoActivateKeyword,
            UseProxies,
            ProxyRotationErrors,
            ProxyRotationMode,
            RateLimitScope,
            NotifKeywordOnly,
        }

        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = UpdateProviderInput;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("UpdateProviderInput JSON object")
            }

            fn visit_map<M>(self, mut map: M) -> std::result::Result<UpdateProviderInput, M::Error>
            where
                M: serde::de::MapAccess<'de>,
            {
                let mut out = UpdateProviderInput::default();
                while let Some(key) = map.next_key::<Field>()? {
                    match key {
                        Field::Name => out.name = Some(map.next_value()?),
                        Field::BaseUrl => out.base_url = Some(map.next_value()?),
                        Field::ExtraHeadersJson => {
                            let raw: serde_json::Value = map.next_value()?;
                            out.extra_headers_json =
                                Some(if let serde_json::Value::String(s) = raw {
                                    Some(s)
                                } else if raw.is_null() {
                                    None
                                } else {
                                    return Err(serde::de::Error::custom(format!(
                                        "extra_headers_json must be string or null, got {raw}"
                                    )));
                                });
                        }
                        Field::UseProxies => out.use_proxies = Some(map.next_value()?),
                        Field::ProxyRotationErrors => {
                            out.proxy_rotation_errors = Some(map.next_value()?);
                        }
                        Field::ProxyRotationMode => {
                            out.proxy_rotation_mode = Some(map.next_value()?);
                        }
                        Field::RateLimitScope => out.rate_limit_scope = Some(map.next_value()?),
                        Field::NotifKeywordOnly => {
                            let raw: serde_json::Value = map.next_value()?;
                            out.notif_keyword_only = Some(if raw.is_null() {
                                None
                            } else {
                                Some(serde_json::from_value(raw).map_err(|e| {
                                    serde::de::Error::custom(format!(
                                        "notif_keyword_only must be bool or null: {e}"
                                    ))
                                })?)
                            });
                        }
                        Field::AutoActivateKeyword => {
                            let raw: serde_json::Value = map.next_value()?;
                            out.auto_activate_keyword =
                                Some(if let serde_json::Value::String(s) = raw {
                                    Some(s)
                                } else if raw.is_null() {
                                    None
                                } else {
                                    return Err(serde::de::Error::custom(format!(
                                        "auto_activate_keyword must be string or null, got {raw}"
                                    )));
                                });
                        }
                    }
                }
                Ok(out)
            }
        }

        deserializer.deserialize_map(V)
    }
}

/// Sentinel returned by the admin list/get provider endpoints in place of
/// `extra_headers_json` (OP-07: that field carries upstream credentials for
/// custom providers and must not be disclosed read-only). A PATCH that echoes
/// the sentinel back is treated as "keep the stored value" so a
/// GET → edit unrelated field → PATCH roundtrip never wipes the credentials.
pub const REDACTED_EXTRA_HEADERS_SENTINEL: &str = "***redacted***";

/// Apply a partial update to an existing provider.
pub fn update_provider(
    conn: &Connection,
    id: &ProviderId,
    input: &UpdateProviderInput,
) -> Result<()> {
    input.validate()?;
    if let Some(ref url) = input.base_url {
        validate_base_url(url)?;
    }
    let keyword = input.auto_activate_keyword.as_ref().map(|o| o.as_deref());
    // OP-07: an update echoing the redaction sentinel back (SPA edit forms
    // re-submit the object they got from GET) means "keep the stored value".
    let extra_headers = input
        .extra_headers_json
        .as_ref()
        .map(|o| o.as_deref())
        .map(|opt| {
            if opt == Some(REDACTED_EXTRA_HEADERS_SENTINEL) {
                None
            } else {
                opt
            }
        });
    providers::update(
        conn,
        id,
        providers::UpdateProviderParams {
            name: input.name.as_deref(),
            base_url: input.base_url.as_deref(),
            extra_headers_json: extra_headers,
            auto_activate_keyword: keyword,
            use_proxies: input.use_proxies,
            proxy_rotation_errors: input.proxy_rotation_errors.as_deref(),
            proxy_rotation_mode: input.proxy_rotation_mode.as_deref(),
            rate_limit_scope: input.rate_limit_scope,
            notif_keyword_only: input.notif_keyword_only,
        },
    )
}
