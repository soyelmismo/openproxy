//! Provider administration service layer.

use crate::error::{CoreError, Result};
use crate::ids::ProviderId;
use crate::providers::{self, AuthType, ProviderFormat};
use crate::validation::{Validatable, validate_base_url};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

/// Inputs for [`create_provider`].
///
/// `auth_type` and `format` arrive as already-validated wire strings
/// (e.g. `"bearer"`, `"openai"`) — typically deserialized from a JSON body
/// — and are parsed into the typed enums at the boundary.
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

/// Insert a new provider. Returns the [`ProviderId`] used.
///
/// Errors:
/// - [`CoreError::Validation`] on unknown `auth_type` / `format` or duplicate
///   id (delegated to [`providers::create`]).
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

/// List all providers.
pub fn list_providers(conn: &Connection) -> Result<Vec<providers::Provider>> {
    providers::list(conn)
}

/// Delete a provider by id.
///
/// Built-in providers (the ones seeded on first run — see
/// [`crate::seed::builtin_provider_ids`]) are **not deletable**:
/// removing the row would leave dangling references in
/// [`openproxy_adapters::adapters::builtin_adapters`], and the operator can
/// always get the "this provider is no longer routed" effect
/// cheaply via [`set_provider_active`] (a soft, reversible flag).
/// This function therefore rejects built-in ids with
/// [`CoreError::Validation`], which the server maps to HTTP 400.
///
/// For non-built-in (custom) providers the call is forwarded to
/// [`providers::delete`] and is idempotent (a missing id is a
/// no-op).
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

/// Flip the soft-disable flag on a provider. A deactivated provider
/// stays in the DB (so its accounts and models are preserved) but is
/// excluded from combo-target resolution; reactivating it brings the
/// targets back automatically. Missing id is a silent no-op.
pub fn set_provider_active(conn: &Connection, id: &ProviderId, active: bool) -> Result<()> {
    providers::set_active(conn, id, active)
}

/// Inputs for [`update_provider`]. All fields are optional, mirroring
/// the partial-update semantics of [`providers::update`]. `name` and
/// `base_url` are straightforward; `extra_headers_json` is the raw
/// JSON string the user wants stored (validated only at apply time).
///
/// `auto_activate_keyword` uses a three-state encoding so the caller
/// can distinguish "don't touch" from "set to NULL":
/// * `None`     — the column is not part of this update (no-op).
/// * `Some(None)` — clear the column back to `NULL`.
/// * `Some(Some(s))` — set the column to the literal string `s`.
///
/// The custom deserializer on this field is what makes the three
/// states work over JSON: a missing key deserializes to `None`, an
/// explicit `null` deserializes to `Some(None)`, and any string
/// deserializes to `Some(Some(s))`. Without the custom deserialize
/// the default `Option<Option<T>>` impl would fold `null` and
/// "absent" into the same `None` and lose the "clear" semantic.
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
    /// Toggle for `providers.notif_keyword_only` (migration 000074).
    /// Three-state, mirroring `auto_activate_keyword`:
    /// * missing key -> `None` (no-op),
    /// * `true` / `false` -> `Some(Some(_))` (set the flag),
    /// * explicit `null` -> `Some(None)` (normalised to 0 on write: the column
    ///   is `NOT NULL DEFAULT 0`).
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

/// Apply a partial update to an existing provider. The three-state
/// `auto_activate_keyword` and `extra_headers_json` let the caller clear the column
/// without sending an empty string.
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
    let extra_headers = input.extra_headers_json.as_ref().map(|o| o.as_deref());
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
