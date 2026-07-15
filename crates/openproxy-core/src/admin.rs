//! Admin service layer. HTTP endpoints in openproxy-server call into this.
//!
//! Each function takes a `&rusqlite::Connection` and is intentionally free of
//! HTTP / axum concerns. The caller (the server crate) is responsible for
//! translating HTTP requests into these function calls and the resulting
//! [`crate::error::CoreError`] into HTTP status codes (see
//! [`crate::error::CoreError::http_status`]).
//!
//! Conventions:
//! - All input is parsed here. Invalid enums (auth_type, format, strategy) are
//!   surfaced as [`crate::error::CoreError::Validation`], which the server
//!   maps to HTTP 400.
//! - API keys are encrypted via [`crate::secrets::MasterKey`] before any
//!   plaintext touches the `accounts` table.
//! - `create_*` returns the newly-inserted id; `list_*` returns a typed view
//!   of rows.
//! - `delete_*` is idempotent: a missing id is a no-op (0 rows affected), not
//!   an error.

use crate::accounts;
use crate::combos;
use crate::cooldown;
use crate::error::{CoreError, Result};
use crate::adapters::ProviderAdapter;
use crate::ids::{AccountId, ComboId, ComboTargetId, ModelId, ModelRowId, ProviderId};
use crate::models;
use crate::providers::{self, AuthType, ProviderFormat};
use crate::quota::AccountQuota;
use crate::secrets::MasterKey;
use crate::upstream::UpstreamClient;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

// =====================================================================
// Helpers
// =====================================================================

/// Validate that a `base_url` is a well-formed HTTP(S) URL with a non-empty
/// host. Rejects data URIs, file URIs, bare hosts, and any other scheme.
fn validate_base_url(url: &str) -> Result<()> {
    // ponytail: [parseo url manual] [usar crate url o http::Uri en el futuro]
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(CoreError::Validation(format!(
            "base_url must start with http:// or https://, got: {url}"
        )));
    }
    // Strip the scheme and extract host (everything up to the first `/`,
    // or end-of-string after `://`). Port is allowed as part of host.
    let remainder = &url[url.find("://").unwrap() + 3..];
    let host_end = remainder.find('/').unwrap_or(remainder.len());
    let host_part = &remainder[..host_end];
    // Strip port if present
    let host = if let Some(colon_pos) = host_part.rfind(':') {
        &host_part[..colon_pos]
    } else {
        host_part
    };
    if host.is_empty() {
        return Err(CoreError::Validation(format!(
            "base_url must have a non-empty host, got: {url}"
        )));
    }
    Ok(())
}

// =====================================================================
// Providers
// =====================================================================

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

/// Insert a new provider. Returns the [`ProviderId`] used.
///
/// Errors:
/// - [`CoreError::Validation`] on unknown `auth_type` / `format` or duplicate
///   id (delegated to [`providers::create`]).
pub fn create_provider(conn: &Connection, input: CreateProviderInput) -> Result<ProviderId> {
    validate_base_url(&input.base_url)?;
    let id = ProviderId::new(input.id);
    let auth = AuthType::parse(&input.auth_type)?;
    let format = ProviderFormat::parse(&input.format)?;
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
            rate_limit_scope: input.rate_limit_scope.unwrap_or(crate::providers::RateLimitScope::Account),
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
/// [`crate::adapters::builtin_adapters`], and the operator can
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
            "provider '{}' is a built-in and cannot be deleted. Use POST \
             /admin/providers/{}/active with {{\"active\": false}} to \
             deactivate it instead.",
            id, id
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
    pub extra_headers_json: Option<String>,
    pub auto_activate_keyword: Option<Option<String>>,
    pub use_proxies: Option<bool>,
    pub proxy_rotation_errors: Option<String>,
    pub rate_limit_scope: Option<crate::providers::RateLimitScope>,
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
            RateLimitScope,
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
                        Field::ExtraHeadersJson => out.extra_headers_json = Some(map.next_value()?),
                        Field::UseProxies => out.use_proxies = Some(map.next_value()?),
                        Field::ProxyRotationErrors => {
                            out.proxy_rotation_errors = Some(map.next_value()?)
                        }
                        Field::RateLimitScope => {
                            out.rate_limit_scope = Some(map.next_value()?)
                        }
                        Field::AutoActivateKeyword => {
                            // The whole point of this custom deserialize:
                            // pull the raw value, then branch on whether
                            // it is null or a string. Default
                            // `Option<Option<String>>` would collapse
                            // these two into the same variant.
                            let raw: serde_json::Value = map.next_value()?;
                            out.auto_activate_keyword = Some(match raw {
                                serde_json::Value::Null => None,
                                serde_json::Value::String(s) => Some(s),
                                other => {
                                    return Err(serde::de::Error::custom(format!(
                                        "auto_activate_keyword must be string or null, got {}",
                                        other
                                    )));
                                }
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
/// `auto_activate_keyword` lets the caller clear the column without
/// sending an empty string (which would be a different semantic).
pub fn update_provider(
    conn: &Connection,
    id: &ProviderId,
    input: UpdateProviderInput,
) -> Result<()> {
    // Validate base_url if it is being updated.
    if let Some(ref url) = input.base_url {
        validate_base_url(url)?;
    }
    // Rust's `Option<Option<&str>>` is awkward to build; the inner
    // map over `Option<String>` keeps the call site readable.
    let keyword: Option<Option<&str>> = match input.auto_activate_keyword {
        None => None,
        Some(None) => Some(None),
        Some(Some(ref s)) => Some(Some(s.as_str())),
    };
    providers::update(
        conn,
        id,
        input.name.as_deref(),
        input.base_url.as_deref(),
        input.extra_headers_json.as_deref(),
        keyword,
        input.use_proxies,
        input.proxy_rotation_errors.as_deref(),
        input.rate_limit_scope,
    )
}

// =====================================================================
// Accounts
// =====================================================================

/// Inputs for [`create_account`]. The plaintext `api_key` is encrypted via
/// `master_key` before insertion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateAccountInput {
    pub provider_id: String,
    /// API key for api_key accounts. `None` for OAuth accounts.
    /// Accepts both `api_key` and `secret` as JSON field names for
    /// compatibility with the web UI (which sends `secret`).
    #[serde(alias = "secret")]
    pub api_key: Option<String>,
    pub label: Option<String>,
    pub priority: Option<i32>,
    pub extra_config_json: Option<String>,
}

/// Insert a new account. The plaintext `api_key` is encrypted with
/// `master_key` and only the resulting BLOB is stored.
///
/// `priority` defaults to `100` when not provided, matching the
/// "lower = higher priority" convention documented in
/// [`crate::accounts`].
pub fn create_account(
    conn: &Connection,
    master_key: &MasterKey,
    input: CreateAccountInput,
) -> Result<AccountId> {
    let provider = ProviderId::new(input.provider_id);
    let priority = input.priority.unwrap_or(100);
    accounts::create(
        conn,
        &provider,
        input.api_key.as_deref(),
        master_key,
        input.label.as_deref(),
        priority,
        input.extra_config_json.as_deref(),
    )
}

/// List accounts, optionally filtered by provider.
/// The `master_key` is required to decrypt `oauth_provider_specific`.
pub fn list_accounts(
    conn: &Connection,
    provider: Option<&ProviderId>,
    master_key: &MasterKey,
) -> Result<Vec<accounts::Account>> {
    accounts::list(conn, provider, master_key)
}

/// Delete an account by id. Idempotent.
pub fn delete_account(conn: &Connection, id: AccountId) -> Result<()> {
    accounts::delete(conn, id)
}

/// Input for [`update_account_api_key`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateAccountApiKeyInput {
    /// New API key. `null` clears the key (OAuth-only account).
    pub api_key: Option<String>,
}

/// Encrypt and store (or clear) the API key for an existing account.
/// Returns [`CoreError::AccountNotFound`] when `id` is missing.
pub fn update_account_api_key(
    conn: &Connection,
    master_key: &MasterKey,
    id: AccountId,
    input: UpdateAccountApiKeyInput,
) -> Result<()> {
    accounts::update_api_key(conn, id, input.api_key.as_deref(), master_key)
}

// =====================================================================
// Quota refresh
// =====================================================================

/// Decrypt an account's API key. The connection must be dropped by the
/// caller before any async work (e.g. the upstream HTTP call); this
/// helper exists so the quota-refresh path doesn't have to repeat the
/// `decrypt_api_key` boilerplate.
pub fn decrypt_api_key_for_account(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
) -> Result<String> {
    accounts::decrypt_api_key(conn, id, master_key)
}

/// Stamp a quota snapshot onto an account row. See
/// [`accounts::set_quota`] for the column-level semantics.
pub fn persist_account_quota(conn: &Connection, id: AccountId, q: &AccountQuota) -> Result<()> {
    accounts::set_quota(conn, id, q)
}

/// Look up the account row needed to route a quota refresh. Returns
/// the account on success, or [`CoreError::AccountNotFound`] when the
/// id is missing. The caller still holds the writer guard when this
/// returns; the typical pattern is to call this, drop the guard, then
/// fire the upstream HTTP call.
/// The `master_key` is required to decrypt `oauth_provider_specific`.
pub fn account_for_quota_refresh(
    conn: &Connection,
    id: AccountId,
    master_key: &MasterKey,
) -> Result<accounts::Account> {
    accounts::get(conn, id, master_key)?.ok_or(CoreError::AccountNotFound(id.0))
}

/// Return the set of provider ids that have a quota fetcher
/// implemented today. The HTTP handler uses this list to short-circuit
/// a quota refresh with a friendly `supported: false` response when
/// Fetch quota for a single account using the right provider-specific
/// fetcher. Today MiniMax (and its CN sibling), OpenRouter, and
/// Antigravity have fetchers; any other provider id returns an
/// `AccountQuota` with all-NULL numeric fields and a `fetch_error`
/// string saying the provider is unsupported.
///
/// `api_key` is the *plaintext* key (decrypted by the caller).
/// `access_token` is the *plaintext* OAuth access token — only used
/// for OAuth-based providers like Antigravity.
pub async fn fetch_account_quota(
    provider_id: &str,
    upstream: &Arc<UpstreamClient>,
    api_key: &str,
    access_token: Option<&str>,
    provider_specific: Option<&str>,
) -> AccountQuota {
    let mut result_quota = None;

    let mapped_id = match provider_id {
        "minimax-cn" => "minimax",
        "agy" => "antigravity",
        other => other,
    };

    let adapters = crate::adapters::builtin_adapters();
    if let Some(adapter) = adapters.iter().find(|a| a.id().as_str() == mapped_id) {
        if let Some(res) = adapter
            .fetch_quota(upstream, api_key, access_token, provider_specific)
            .await
        {
            result_quota = Some(match res {
                Ok(q) => q,
                Err(e) => AccountQuota {
                    session_used: None,
                    session_limit: None,
                    session_reset_at: None,
                    weekly_used: None,
                    weekly_limit: None,
                    weekly_reset_at: None,
                    plan_name: None,
                    last_fetched_at: now_unix_secs_str(),
                    fetch_error: Some(e.to_string()),
                    model_details: None,
                },
            });
        }
    }

    if let Some(q) = result_quota {
        q
    } else {
        AccountQuota {
            session_used: None,
            session_limit: None,
            session_reset_at: None,
            weekly_used: None,
            weekly_limit: None,
            weekly_reset_at: None,
            plan_name: None,
            last_fetched_at: now_unix_secs_str(),
            fetch_error: Some(format!(
                "quota fetching not implemented for provider '{}'",
                provider_id
            )),
            model_details: None,
        }
    }
}

/// Best-effort current-time stamp for an `AccountQuota::last_fetched_at`
/// field. Mirrors [`quota::now_unix_secs_str`] but lives here so the
/// `fetch_account_quota` fallback path can stamp an error-only quota
/// without crossing the `quota` module boundary just for a helper.
pub(crate) fn now_unix_secs_str() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    secs.to_string()
}

// =====================================================================
// Combos
// =====================================================================

/// Inputs for [`create_combo`].
///
/// The `priority_mode` / `cooldown_mode` / per-combo cooldown
/// overrides / `lkgp_exploration_rate` / `selection_window_secs`
/// fields are all optional (migration 000035). `None` means "use
/// the legacy default" — `Strict` priority mode, `Flat` cooldown
/// mode, and the global `[cooldown]` config for the cooldown
/// numbers. A non-`None` `priority_mode` / `cooldown_mode` is
/// parsed and validated by [`combos::PriorityMode::parse`] /
/// [`combos::CooldownMode::parse`]; an unknown value surfaces as
/// [`CoreError::Validation`] (HTTP 400).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateComboInput {
    pub name: String,
    pub strategy: String,
    pub race_size: Option<u8>,
    /// Priority mode for `Strategy::Priority`. `None` = `strict`
    /// (the legacy walk). Ignored for `RoundRobin` / `Shuffle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_mode: Option<String>,
    /// Cooldown growth mode. `None` = `flat` (the legacy behavior).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_mode: Option<String>,
    /// Per-combo cooldown base (seconds). `None` = use the global
    /// `[cooldown] cooldown_secs` / `[cooldown] base_secs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_base_secs: Option<u64>,
    /// Per-combo cooldown cap (seconds). `None` = use the global
    /// `[cooldown] max_secs` (default 3600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_max_secs: Option<u64>,
    /// Per-combo exponential growth factor. `None` = use the global
    /// `[cooldown] factor` (default 2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cooldown_factor: Option<u32>,
    /// LKGP exploration rate (0.0–1.0). `None` = default 0.1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lkgp_exploration_rate: Option<f64>,
    /// Selection window (seconds) for `least_used` / `p2c` modes.
    /// `None` = default 3600.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selection_window_secs: Option<u64>,
}

/// Add a target to a combo. The wire shape is the historical one for
/// flat (model) targets, plus a new `sub_combo_id` field for combo-in-
/// combo targets. Exactly one of `model_row_id` / `sub_combo_id` must
/// be `Some`; the XOR is enforced by [`combos::add_target`] because
/// SQLite cannot add a CHECK constraint to a populated table.
///
/// For sub-combo targets the `provider_id` field is accepted for
/// backward-compatibility (the wire shape is uniform) but is
/// effectively ignored: the virtual `"combo"` provider is what the
/// stored row references, and the routing happens through the
/// sub-combo's children, not through the chosen provider. Pass any
/// value (e.g. `"combo"`) and the validator will be happy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddTargetInput {
    pub provider_id: String,
    pub account_id: Option<AccountId>,
    pub model_row_id: Option<ModelRowId>,
    pub sub_combo_id: Option<ComboId>,
    pub priority_order: i32,
}

/// Create a new combo. `race_size` defaults to `1`; `strategy` is parsed
/// from the wire string (e.g. `"priority"`, `"round_robin"`).
///
/// Newly-created combos are auto-populated with one target per active model
/// of the first provider that has a healthy account and at least one
/// active model. This is the "operator wants to test NOW" path: a combo
/// should never be born empty when the DB has routable targets. If no
/// candidate provider exists the combo is still created — it just stays
/// empty and the pipeline's `auto_populate` fallback will try again on
/// the next chat request.
pub fn create_combo(conn: &Connection, input: CreateComboInput) -> Result<ComboId> {
    let strategy = combos::Strategy::parse(&input.strategy)?;
    // Default of 1 is the "serial / one target at a time" race
    // window. NOTE: for `Strategy::Priority` the pipeline ignores
    // `race_size` entirely (the operator wants walk-the-row
    // behavior), so this default only meaningfully applies to
    // `Strategy::RoundRobin` and `Strategy::Shuffle`. Don't
    // change it without revisiting `pipeline.rs` step 5.
    let race_size = input.race_size.unwrap_or(1);
    let combo_id = combos::create_combo(conn, &input.name, strategy, race_size)?;
    // Best-effort auto-fill. Errors are non-fatal: the combo exists
    // already, and a later pipeline run can re-attempt the fill.
    let _ = combos::auto_populate_empty_combo(conn, combo_id);

    // Apply the migration-000035 per-combo overrides. Each helper
    // validates its inputs (e.g. `priority_mode` must be a known
    // enum value, `lkgp_exploration_rate` must be in `[0.0, 1.0]`)
    // and writes a single UPDATE. A validation failure here leaves
    // the combo created (with `auto_populate` already run) and
    // surfaces the error to the caller — the operator can fix the
    // bad input and re-POST, or PATCH the combo after the fact.
    //
    // We call the helpers unconditionally: a `None` field clears
    // the column back to `NULL` (the legacy default), which is the
    // right thing for a fresh create where the operator omitted the
    // field. The helpers are also no-ops on a missing combo id
    // (which can't happen here — we just inserted it).
    if input.priority_mode.is_some() {
        combos::update_priority_mode(conn, combo_id, input.priority_mode.as_deref())?;
    }
    if input.cooldown_mode.is_some()
        || input.cooldown_base_secs.is_some()
        || input.cooldown_max_secs.is_some()
        || input.cooldown_factor.is_some()
    {
        combos::update_cooldown_settings(
            conn,
            combo_id,
            input.cooldown_mode.as_deref(),
            input.cooldown_base_secs,
            input.cooldown_max_secs,
            input.cooldown_factor,
        )?;
    }
    if input.lkgp_exploration_rate.is_some() {
        combos::update_lkgp_settings(conn, combo_id, input.lkgp_exploration_rate)?;
    }
    if input.selection_window_secs.is_some() {
        combos::update_selection_window(conn, combo_id, input.selection_window_secs)?;
    }
    Ok(combo_id)
}

/// List all combos.
pub fn list_combos(conn: &Connection) -> Result<Vec<combos::Combo>> {
    combos::list_combos(conn)
}

/// Lightweight projection of a combo for the "add sub-combo target"
/// picker. We don't need the full [`combos::Combo`] (race_size,
/// created_at, …) — only the id and the name are surfaced in the UI.
#[derive(Debug, Clone, Serialize)]
pub struct ComboSummary {
    pub id: i64,
    pub name: String,
}

/// List combos that are valid sub-combo targets of `combo_id`.
///
/// A combo is *not* a valid sub-combo target if:
///
/// - it is the same combo (no self-loop), or
/// - adding it as a sub-combo would close a cycle in the sub-combo
///   graph — the probe uses [`combos::combo_in_chain`] with the
///   same depth cap as the row-level check in
///   [`combos::add_target`], so the picker never offers a choice
///   that the API would later reject.
///
/// The function returns the combos in id-ascending order so the UI
/// renders a stable list. Combos with the same id as `combo_id` are
/// silently filtered (no error — the picker is allowed to ask about
/// a combo's own valid sub-combos at any time).
pub fn list_valid_sub_combos(conn: &Connection, combo_id: ComboId) -> Result<Vec<ComboSummary>> {
    let all = combos::list_combos(conn)?;
    let mut out = Vec::with_capacity(all.len());
    for c in all {
        if c.id == combo_id {
            continue;
        }
        // Would adding `c` as a sub-combo of `combo_id` create a
        // cycle? Yes iff `combo_id` is already reachable from `c`
        // in the sub-combo graph (i.e. `c` already contains
        // `combo_id` somewhere downstream). The probe walks down
        // from `c`; see [`combos::combo_in_chain`].
        if combos::combo_in_chain(conn, combo_id, c.id, combos::MAX_SUB_COMBO_DEPTH)? {
            continue;
        }
        out.push(ComboSummary {
            id: c.id.0,
            name: c.name,
        });
    }
    Ok(out)
}

/// Add a target to an existing combo. Returns the new target id.
///
/// Validates that the combo, the model (for flat targets) or the
/// sub-combo (for combo-in-combo targets), and (if provided) the
/// account all exist; missing entities surface as
/// [`CoreError::ComboNotFound`], [`CoreError::AccountNotFound`], or
/// [`CoreError::Validation`] respectively (delegated to
/// [`combos::add_target`]). For sub-combo targets, the function also
/// rejects self-loops and would-be cycles via [`combos::combo_in_chain`].
pub fn add_target_to_combo(
    conn: &Connection,
    combo_id: ComboId,
    input: AddTargetInput,
) -> Result<ComboTargetId> {
    let provider = ProviderId::new(input.provider_id);
    combos::add_target(
        conn,
        combos::AddTargetInput {
            combo_id,
            provider_id: provider,
            account_id: input.account_id,
            model_row_id: input.model_row_id,
            sub_combo_id: input.sub_combo_id,
            priority_order: input.priority_order,
        },
    )
}

/// List targets for a combo, ordered by `(priority_order ASC, id ASC)`.
pub fn list_combo_targets(
    conn: &Connection,
    combo_id: ComboId,
) -> Result<Vec<combos::ComboTarget>> {
    combos::list_targets(conn, combo_id)
}

/// List targets enriched with the model's display name. Used by the admin
/// API so the dashboard can render the human-readable model id without
/// doing a per-row roundtrip to `GET /admin/models`. See
/// [`combos::list_targets_with_model`] for the SQL details.
pub fn list_combo_targets_with_model(
    conn: &Connection,
    combo_id: ComboId,
) -> Result<Vec<combos::ComboTargetWithModel>> {
    combos::list_targets_with_model(conn, combo_id)
}

/// Delete a combo by id. Idempotent. FK cascade removes its targets.
pub fn delete_combo(conn: &Connection, id: ComboId) -> Result<()> {
    combos::delete_combo(conn, id)
}

/// Delete a combo target by id. The combo_id is not strictly required
/// (the target id is unique on its own), but we validate that the
/// target belongs to the requested combo as a defense-in-depth check
/// against a malformed URL like
/// `DELETE /admin/combos/9999/targets/1` where the target exists
/// but in a different combo.
///
/// Missing rows surface as [`CoreError::Validation`] because there is
/// no dedicated "target not in combo" variant in [`CoreError`]; the
/// server maps that to HTTP 400, which is the right code for a
/// URL-shape mismatch the caller should fix.
fn ensure_target_in_combo(
    conn: &Connection,
    combo_id: ComboId,
    target_id: ComboTargetId,
) -> Result<()> {
    let belongs: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM combo_targets WHERE id = ?1 AND combo_id = ?2)",
            params![target_id.0, combo_id.0],
            |r| r.get::<_, i64>(0),
        )
        .map(|v| v != 0)
        .map_err(|e| CoreError::Database {
            message: format!(
                "check combo_target {} belongs to combo {}: {}",
                target_id.0, combo_id.0, e
            ),
            source: Some(Box::new(e)),
        })?;
    if !belongs {
        return Err(CoreError::Validation(format!(
            "target {} not in combo {}",
            target_id.0, combo_id.0
        )));
    }
    Ok(())
}

pub fn delete_combo_target(
    conn: &Connection,
    combo_id: ComboId,
    target_id: ComboTargetId,
) -> Result<()> {
    // Validate the target belongs to the combo (defense in depth).
    ensure_target_in_combo(conn, combo_id, target_id)?;
    combos::delete_target(conn, target_id)
}

/// Atomically reassign `priority_order` for every target of `combo_id`
/// so the order matches `ordered_ids` (index 0 becomes priority 1,
/// index 1 becomes priority 2, etc.). The call is the swap-style
/// alternative to the partial `update_target_priority` helper: it
/// renumbers the whole combo in a single transaction so two targets
/// can never briefly hold the same `priority_order`.
///
/// The reorder is rejected with [`CoreError::Validation`] when
/// `ordered_ids` is not a permutation of the combo's current target
/// ids (extra id, missing id, duplicate id, or cross-combo id).
///
/// Takes `&mut Connection` because the underlying
/// [`combos::reorder_targets`] opens an `IMMEDIATE` transaction; the
/// HTTP handler hands in the writer guard's `&mut` reborrow.
pub fn reorder_combo_targets(
    conn: &mut Connection,
    combo_id: ComboId,
    ordered_ids: &[ComboTargetId],
) -> Result<()> {
    combos::reorder_targets(conn, combo_id, ordered_ids)
}

/// Force-clear the cooldown for a single target. Used by the
/// dashboard's "Reset cooldown" button: an operator who has
/// diagnosed the upstream issue can clear a parked target without
/// waiting for `cooldown_secs` to elapse.
///
/// Validates that the target belongs to `combo_id` (defense in
/// depth, mirroring [`delete_combo_target`]). Cross-combo
/// combinations surface as [`CoreError::Validation`].
pub fn clear_combo_target_cooldown(
    conn: &Connection,
    combo_id: ComboId,
    target_id: ComboTargetId,
) -> Result<()> {
    // The check is the same shape as `delete_combo_target`: the
    // target row must reference the requested combo. The
    // cascade-on-delete FK on `target_cooldowns.combo_target_id`
    // means a delete of the target will *also* clear its
    // cooldown, but the operator's intent here is "clear the
    // cooldown, not delete the target", so we use the explicit
    // DELETE rather than a no-op target delete.
    ensure_target_in_combo(conn, combo_id, target_id)?;
    cooldown::clear(conn, target_id)
}

// =====================================================================
// Models
// =====================================================================

/// List all known models for a provider, optionally filtered.
///
/// When `provider` is `None`, every row in the `models` table is returned.
pub fn list_models(conn: &Connection, provider: Option<&ProviderId>) -> Result<Vec<models::Model>> {
    match provider {
        Some(p) => models::list_all(conn)?
            .into_iter()
            .filter(|m| &m.provider_id == p)
            .collect::<Vec<_>>()
            .pipe(Ok),
        None => models::list_all(conn),
    }
}

/// Inputs for [`create_custom_model`]. Distinct from the adapter-driven
/// [`refresh_models`] path: the operator hand-picks the `(provider_id,
/// model_id)` pair, the optional human-readable `display_name`, the
/// output `target_format` (the wire format the upstream speaks), and a
/// `ttl_seconds` cache lifetime (`0` means "never expire").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateCustomModelInput {
    pub provider_id: String,
    pub model_id: String,
    pub display_name: Option<String>,
    /// `"openai"` or `"anthropic"`. Anything else surfaces as
    /// [`CoreError::Validation`].
    pub target_format: String,
    pub ttl_seconds: i64,
}

/// Create a hand-picked model row. See [`models::create_custom`] for the
/// SQL semantics. Returns the row id of the new (or upserted) row.
pub fn create_custom_model(conn: &Connection, input: CreateCustomModelInput) -> Result<ModelRowId> {
    let provider = ProviderId::new(input.provider_id);
    let model = ModelId::new(input.model_id);
    let target_format = models::TargetFormat::parse(&input.target_format)?;
    models::create_custom(
        conn,
        &provider,
        &model,
        input.display_name.as_deref(),
        target_format,
        input.ttl_seconds,
    )
}

/// Refresh the model list for a provider by calling the adapter's
/// `fetch_models` and upserting the results.
///
/// The caller is responsible for:
/// - resolving the right adapter for `provider`,
/// - decrypting an account's API key and passing it in plaintext,
/// - supplying the shared [`crate::upstream::UpstreamClient`] (the
///   hyper-based client, with per-phase timeouts driven by
///   `TimeoutProfile::ModelDiscovery`),
/// - choosing `ttl_seconds` (typically the duration after which rows
///   should be re-discovered).
///
/// On success, returns an [`models::UpsertResult`] with the touched
/// count and the list of `model_id`s that were newly inserted (i.e.
/// not present in the table for this provider before the call). On
/// failure, returns an [`CoreError`] describing the upstream or DB
/// failure.
///
/// ## `Send` and the connection
///
/// The function takes the [`Connection`] by value (not by reference).
/// `rusqlite::Connection: !Sync` — it carries a `RefCell` internally for
/// the prepared-statement cache — so `&Connection: !Send`. Holding that
/// borrow across the `adapter.fetch_models(...).await` would propagate
/// `!Send` to the outer future, which breaks axum's `Handler` trait
/// (which requires the handler future to be `Send` for the multi-threaded
/// tokio runtime). The caller is expected to *clone* the connection (in
/// the production path: `DbPool::with_conn` plus a second open via the
/// pool's writer mutex that we then drop before awaiting) and hand the
/// owned handle in here, so the future stays `Send` end to end.
pub async fn refresh_models<A: crate::adapters::ProviderAdapter>(
    conn: Connection,
    provider: &ProviderId,
    api_key: &str,
    adapter: &A,
    upstream_client: &std::sync::Arc<crate::upstream::UpstreamClient>,
    ttl_seconds: i64,
    account_label: &str,
) -> Result<models::UpsertResult> {
    // Sanity: the provider must exist; otherwise we'd silently create rows
    // referencing a non-existent parent (the FK would actually reject them
    // later, but failing fast is friendlier).
    if providers::get(&conn, provider)?.is_none() {
        return Err(CoreError::ProviderNotFound(provider.to_string()));
    }

    let discovered = adapter
        .fetch_models_for_account(upstream_client, api_key, account_label)
        .await?;
    let ttl = Duration::from_secs(ttl_seconds.max(0) as u64);
    models::upsert_many(&conn, provider, &discovered, ttl)
}

/// Inputs for [`set_active_bulk`]. The dashboard sends one of these from
/// the "Enable all" / "Disable all" buttons; the handler does a single
/// SQL UPDATE over every non-custom row of the given provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BulkToggleInput {
    pub provider_id: String,
    pub active: bool,
}

/// Bulk set `active` for all non-custom models of a provider. Atomic
/// at the SQL level: a single `UPDATE ... WHERE provider_id = ? AND
/// custom = 0` statement flips every row in one shot, so a concurrent
/// `apply_auto_activation` cannot interleave and leave the table
/// half-toggled (the writer mutex on the pool already serializes the
/// two statements against each other).
///
/// Returns the number of rows updated. Missing provider is a no-op
/// (the WHERE clause just doesn't match anything).
pub fn set_active_bulk(conn: &Connection, input: BulkToggleInput) -> Result<u64> {
    let provider = ProviderId::new(input.provider_id);
    models::set_active_bulk(conn, &provider, input.active)
}

// Tiny pipe helper to keep `list_models` readable without pulling in a
// dependency on a full pipe library. Lives here (not in lib root) because
// it has no use outside this module.
trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::ProviderAdapter;
    use crate::db::conn::DbPool;
    use crate::db::migrations;
    use std::path::PathBuf;

    /// Build a fresh in-process pool: temp dir on disk, migrations applied.
    fn fresh_pool() -> (DbPool, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("openproxy-admin-test-{}-{}-{}", pid, nanos, n));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("admin.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    #[test]
    fn create_provider_then_list() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        let id = create_provider(
            &conn,
            CreateProviderInput { rate_limit_scope: None,
                id: "openrouter".into(),
                name: "OpenRouter".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                auth_type: "bearer".into(),
                format: "openai".into(),
                extra_headers_json: Some(r#"{"X-Title":"openproxy"}"#.into()),
            },
        )
        .expect("create provider");
        assert_eq!(id, ProviderId::new("openrouter"));

        let listed = list_providers(&conn).expect("list providers");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert_eq!(listed[0].auth_type, AuthType::Bearer);
        assert_eq!(listed[0].format, ProviderFormat::Openai);

        // Invalid auth_type surfaces as Validation.
        let err = create_provider(
            &conn,
            CreateProviderInput { rate_limit_scope: None,
                id: "bad".into(),
                name: "x".into(),
                base_url: "https://x".into(),
                auth_type: "magic".into(),
                format: "openai".into(),
                extra_headers_json: None,
            },
        )
        .expect_err("invalid auth_type");
        assert!(matches!(err, CoreError::Validation(_)));
    }

    #[test]
    fn test_quota_capability_anti_drift() {
        let adapters = crate::adapters::builtin_adapters();
        let providers_with_quota = [
            "minimax",
            "minimax-cn",
            "antigravity",
            "agy",
            "codex",
            "kiro",
        ];
        let providers_with_fetcher = [
            "minimax",
            "minimax-cn",
            "antigravity",
            "agy",
            "codex",
            "kiro",
        ];
        for adapter in adapters {
            let id = adapter.id().as_str();
            let metadata = adapter.metadata();
            let has_quota = providers_with_quota.contains(&id);
            let has_fetcher = providers_with_fetcher.contains(&id);
            assert_eq!(
                metadata.supports_quota, has_quota,
                "provider {} supports_quota mismatch: expected {}",
                id, has_quota
            );
            assert_eq!(
                metadata.quota_refresh_supported, has_fetcher,
                "provider {} quota_refresh_supported mismatch: expected {}",
                id, has_fetcher
            );
        }
    }

    #[test]
    fn create_account_encrypts_and_lists() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        create_provider(
            &conn,
            CreateProviderInput { rate_limit_scope: None,
                id: "openrouter".into(),
                name: "OpenRouter".into(),
                base_url: "https://openrouter.ai/api/v1".into(),
                auth_type: "bearer".into(),
                format: "openai".into(),
                extra_headers_json: None,
            },
        )
        .expect("seed provider");

        let mk = MasterKey::generate();
        let plaintext = "sk-supersecret-DO-NOT-LEAK";
        let id = create_account(
            &conn,
            &mk,
            CreateAccountInput {
                provider_id: "openrouter".into(),
                api_key: Some(plaintext.into()),
                label: Some("primary".into()),
                priority: Some(10),
                extra_config_json: None,
            },
        )
        .expect("create account");

        // Listing returns the row.
        let all = list_accounts(&conn, None, &mk).expect("list all");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id);
        assert_eq!(all[0].label.as_deref(), Some("primary"));
        assert_eq!(all[0].priority, 10);

        // Filtered by provider.
        let filtered =
            list_accounts(&conn, Some(&ProviderId::new("openrouter")), &mk).expect("list filtered");
        assert_eq!(filtered.len(), 1);

        // The plaintext is NOT visible in the raw BLOB.
        let raw: Vec<u8> = rusqlite::Connection::query_row(
            &conn,
            "SELECT api_key_encrypted FROM accounts WHERE id = ?1",
            rusqlite::params![id.0],
            |r| r.get(0),
        )
        .expect("raw select");
        let raw_str = String::from_utf8_lossy(&raw);
        assert!(
            !raw_str.contains(plaintext),
            "plaintext must not appear in stored blob"
        );

        // Decrypting with the same key recovers the plaintext.
        let recovered = accounts::decrypt_api_key(&conn, id, &mk).expect("decrypt");
        assert_eq!(recovered, plaintext);

        // Default priority is 100.
        let id2 = create_account(
            &conn,
            &mk,
            CreateAccountInput {
                provider_id: "openrouter".into(),
                api_key: Some("sk-another".into()),
                label: None,
                priority: None,
                extra_config_json: None,
            },
        )
        .expect("create default-prio");
        let a = list_accounts(&conn, None, &mk)
            .expect("list")
            .into_iter()
            .find(|a| a.id == id2)
            .expect("present");
        assert_eq!(a.priority, 100, "default priority is 100");
    }

    #[test]
    fn create_combo_with_targets_then_list() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        // Two providers, one model each.
        for pid in ["p1", "p2"] {
            create_provider(
                &conn,
                CreateProviderInput { rate_limit_scope: None,
                    id: pid.into(),
                    name: pid.into(),
                    base_url: "https://example.com".into(),
                    auth_type: "bearer".into(),
                    format: "openai".into(),
                    extra_headers_json: None,
                },
            )
            .expect("seed provider");
        }
        // Seed two model rows.
        // Use `last_insert_rowid()` to capture the actual rowid;
        // `conn.execute(...)` returns the number of affected rows,
        // not the rowid, so naively casting it would make every
        // ModelRowId equal to 1 and silently break the add_target
        // path (the model/provider cross-check in add_target was
        // added together with this fix, which is what surfaced
        // the latent test bug).
        let m1_rowid: i64 = {
            conn.execute(
                "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p1', 'm1', 'openai')",
                [],
            )
            .expect("seed m1");
            conn.query_row("SELECT last_insert_rowid()", [], |r| r.get::<_, i64>(0))
                .expect("last_insert_rowid m1")
        };
        let m2_rowid: i64 = {
            conn.execute(
                "INSERT INTO models(provider_id, model_id, target_format) VALUES ('p2', 'm2', 'openai')",
                [],
            )
            .expect("seed m2");
            conn.query_row("SELECT last_insert_rowid()", [], |r| r.get::<_, i64>(0))
                .expect("last_insert_rowid m2")
        };
        let m1 = ModelRowId(m1_rowid);
        let m2 = ModelRowId(m2_rowid);

        // Default race_size: 1; valid strategy string parses.
        let combo_id = create_combo(
            &conn,
            CreateComboInput {
                name: "primary".into(),
                strategy: "priority".into(),
                race_size: None,
                priority_mode: None,
                cooldown_mode: None,
                cooldown_base_secs: None,
                cooldown_max_secs: None,
                cooldown_factor: None,
                lkgp_exploration_rate: None,
                selection_window_secs: None,
            },
        )
        .expect("create combo");
        let listed = list_combos(&conn).expect("list combos");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, combo_id);
        assert_eq!(listed[0].strategy, combos::Strategy::Priority);
        assert_eq!(listed[0].race_size, 1, "default race_size is 1");

        // Custom race_size is honored.
        let combo_rr = create_combo(
            &conn,
            CreateComboInput {
                name: "rr".into(),
                strategy: "round_robin".into(),
                race_size: Some(3),
                priority_mode: None,
                cooldown_mode: None,
                cooldown_base_secs: None,
                cooldown_max_secs: None,
                cooldown_factor: None,
                lkgp_exploration_rate: None,
                selection_window_secs: None,
            },
        )
        .expect("create rr combo");
        let combo_rr_got = list_combos(&conn)
            .expect("list")
            .into_iter()
            .find(|c| c.id == combo_rr)
            .expect("present");
        assert_eq!(combo_rr_got.race_size, 3);
        assert_eq!(combo_rr_got.strategy, combos::Strategy::RoundRobin);

        // Add two targets.
        let t1 = add_target_to_combo(
            &conn,
            combo_id,
            AddTargetInput {
                provider_id: "p1".into(),
                account_id: None,
                model_row_id: Some(m1),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add t1");
        let t2 = add_target_to_combo(
            &conn,
            combo_id,
            AddTargetInput {
                provider_id: "p2".into(),
                account_id: None,
                model_row_id: Some(m2),
                sub_combo_id: None,
                priority_order: 20,
            },
        )
        .expect("add t2");

        let targets = list_combo_targets(&conn, combo_id).expect("list targets");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].id, t1);
        assert_eq!(targets[1].id, t2);
        assert_eq!(targets[0].provider_id, ProviderId::new("p1"));
        assert_eq!(targets[1].provider_id, ProviderId::new("p2"));
        assert_eq!(targets[0].priority_order, 10);
        assert_eq!(targets[1].priority_order, 20);

        // Validation: invalid strategy.
        let err = create_combo(
            &conn,
            CreateComboInput {
                name: "bad".into(),
                strategy: "fifo".into(),
                race_size: None,
                priority_mode: None,
                cooldown_mode: None,
                cooldown_base_secs: None,
                cooldown_max_secs: None,
                cooldown_factor: None,
                lkgp_exploration_rate: None,
                selection_window_secs: None,
            },
        )
        .expect_err("invalid strategy");
        assert!(matches!(err, CoreError::Validation(_)));

        // Validation: missing combo on add_target.
        let err = add_target_to_combo(
            &conn,
            ComboId(99999),
            AddTargetInput {
                provider_id: "p1".into(),
                account_id: None,
                model_row_id: Some(m1),
                sub_combo_id: None,
                priority_order: 0,
            },
        )
        .expect_err("missing combo");
        assert!(matches!(err, CoreError::ComboNotFound(_)));

        // Validation: missing model on add_target.
        let err = add_target_to_combo(
            &conn,
            combo_id,
            AddTargetInput {
                provider_id: "p1".into(),
                account_id: None,
                model_row_id: Some(ModelRowId(88888)),
                sub_combo_id: None,
                priority_order: 0,
            },
        )
        .expect_err("missing model");
        assert!(matches!(err, CoreError::Validation(_)));
    }

    #[test]
    fn refresh_models_with_invalid_provider_fails() {
        // refresh_models first does a `providers::get` and returns
        // ProviderNotFound when the provider does not exist. We don't need
        // an actual adapter or HTTP roundtrip to exercise this branch.
        let (pool, _path) = fresh_pool();
        let conn = pool.open_connection().expect("open conn");

        // We need a minimal adapter impl to satisfy the trait. We don't
        // call `fetch_models` on it.
        let adapter = crate::adapters::ProviderAdapterEnum::Mock(
            crate::pipeline::test_utils::MockAdapter::new(
                "stub",
                String::new(),
                crate::adapters::AdapterFormat::Openai,
            ),
        );
        let upstream = crate::upstream::UpstreamClient::new();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let res = rt.block_on(refresh_models(
            conn,
            &ProviderId::new("does-not-exist"),
            "sk-doesnt-matter",
            &adapter,
            &upstream,
            3600,
            "",
        ));
        match res {
            Err(CoreError::ProviderNotFound(id)) => assert_eq!(id, "does-not-exist"),
            other => panic!("expected ProviderNotFound, got {:?}", other),
        }
    }

    // NOTE: integration tests that actually hit the network via
    // `refresh_models` are intentionally not included here. The signature
    // takes a fully wired `&crate::adapters::ProviderAdapterEnum` and an
    // `&Arc<UpstreamClient>` precisely so the server crate can drive
    // it end-to-end; any wire-level exercise belongs in the server's
    // integration test suite.

    #[test]
    fn update_provider_changes_name_and_keyword() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        create_provider(
            &conn,
            CreateProviderInput { rate_limit_scope: None,
                id: "p".into(),
                name: "Original".into(),
                base_url: "https://example.com".into(),
                auth_type: "bearer".into(),
                format: "openai".into(),
                extra_headers_json: None,
            },
        )
        .expect("seed");

        // Set name + keyword.
        update_provider(
            &conn,
            &ProviderId::new("p"),
            UpdateProviderInput { rate_limit_scope: None,
                name: Some("Renamed".into()),
                base_url: None,
                extra_headers_json: None,
                auto_activate_keyword: Some(Some("claude".into())),
                use_proxies: None,
                proxy_rotation_errors: None,
            },
        )
        .expect("update");
        let p = list_providers(&conn).expect("list").pop().expect("present");
        assert_eq!(p.name, "Renamed");
        assert_eq!(p.auto_activate_keyword.as_deref(), Some("claude"));

        // Clear keyword with Some(None).
        update_provider(
            &conn,
            &ProviderId::new("p"),
            UpdateProviderInput { rate_limit_scope: None,
                name: None,
                base_url: None,
                extra_headers_json: None,
                auto_activate_keyword: Some(None),
                use_proxies: None,
                proxy_rotation_errors: None,
            },
        )
        .expect("clear");
        let p = list_providers(&conn).expect("list").pop().expect("present");
        assert_eq!(p.auto_activate_keyword, None);

        // Missing id: ProviderNotFound.
        let err = update_provider(
            &conn,
            &ProviderId::new("nope"),
            UpdateProviderInput::default(),
        )
        .expect_err("missing id");
        assert!(matches!(err, CoreError::ProviderNotFound(_)));
    }

    #[test]
    fn update_provider_input_three_state_deserialize() {
        // Triple-state: missing key -> None; explicit null -> Some(None);
        // string -> Some(Some(s)). The default `Option<Option<T>>`
        // deserializer would fold "missing" and "null" into the same
        // variant; the custom impl on `UpdateProviderInput` is what
        // gives us the "clear without setting" semantic.
        let absent: UpdateProviderInput = serde_json::from_str("{}").unwrap();
        assert!(absent.auto_activate_keyword.is_none(), "missing key");

        let cleared: UpdateProviderInput =
            serde_json::from_str(r#"{"auto_activate_keyword": null}"#).unwrap();
        assert!(matches!(cleared.auto_activate_keyword, Some(None)));

        let set: UpdateProviderInput =
            serde_json::from_str(r#"{"auto_activate_keyword": "claude"}"#).unwrap();
        assert!(matches!(set.auto_activate_keyword, Some(Some(ref s)) if s == "claude"));

        // Bad type surfaces as a deserialization error.
        let bad = serde_json::from_str::<UpdateProviderInput>(r#"{"auto_activate_keyword": 42}"#);
        assert!(bad.is_err());
    }

    #[test]
    fn create_custom_model_wraps_validation() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        create_provider(
            &conn,
            CreateProviderInput { rate_limit_scope: None,
                id: "p".into(),
                name: "P".into(),
                base_url: "https://x".into(),
                auth_type: "bearer".into(),
                format: "openai".into(),
                extra_headers_json: None,
            },
        )
        .expect("seed");

        // Bad target_format -> Validation.
        let err = create_custom_model(
            &conn,
            CreateCustomModelInput {
                provider_id: "p".into(),
                model_id: "m".into(),
                display_name: None,
                target_format: "xml".into(),
                ttl_seconds: 60,
            },
        )
        .expect_err("bad format");
        assert!(matches!(err, CoreError::Validation(_)));

        // Happy path.
        let row_id = create_custom_model(
            &conn,
            CreateCustomModelInput {
                provider_id: "p".into(),
                model_id: "my-model".into(),
                display_name: Some("Display".into()),
                target_format: "openai".into(),
                ttl_seconds: 60,
            },
        )
        .expect("create custom");
        let m = models::get_by_row_id(&conn, row_id)
            .unwrap()
            .expect("present");
        assert!(m.custom);
        assert!(m.active);
    }

    #[test]
    fn delete_provider_rejects_builtin() {
        // Seed the three built-ins, then try to delete each one. The
        // delete must fail with `CoreError::Validation` (which the
        // server maps to HTTP 400) and the row must still be present
        // afterwards. The error message must point the operator at
        // the deactivate endpoint so the dashboard's error toast
        // tells them what to do next.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        crate::seed::seed_builtin_providers(&conn).expect("seed builtins");

        for builtin_id in crate::seed::builtin_provider_ids() {
            let id = ProviderId::new(*builtin_id);
            let err = delete_provider(&conn, &id).expect_err("built-in delete must fail");
            match &err {
                CoreError::Validation(msg) => {
                    assert!(
                        msg.contains("built-in"),
                        "error message should call out the built-in status, got: {}",
                        msg
                    );
                    assert!(
                        msg.contains(builtin_id),
                        "error message should name the provider, got: {}",
                        msg
                    );
                }
                other => panic!("expected Validation, got {:?}", other),
            }
            // The row must still be present — we rejected the delete,
            // we did not silently swallow it.
            assert!(
                providers::get(&conn, &id).expect("get").is_some(),
                "row for {} must still be present",
                builtin_id
            );
        }
    }

    #[test]
    fn delete_provider_allows_custom() {
        // A custom (operator-created) provider is not in the
        // built-in list, so the delete path is the normal
        // forward-to-providers::delete. Verifies the guard is *only*
        // triggered for built-ins and not as a blanket refusal.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        create_provider(
            &conn,
            CreateProviderInput { rate_limit_scope: None,
                id: "my-custom".into(),
                name: "My Custom".into(),
                base_url: "https://example.test".into(),
                auth_type: "bearer".into(),
                format: "openai".into(),
                extra_headers_json: None,
            },
        )
        .expect("create custom");

        let id = ProviderId::new("my-custom");
        delete_provider(&conn, &id).expect("custom delete should succeed");
        assert!(
            providers::get(&conn, &id).expect("get").is_none(),
            "row should be gone"
        );

        // A second delete is a no-op (idempotent), not an error —
        // the built-in guard does not change this behavior.
        delete_provider(&conn, &id).expect("idempotent re-delete is fine");
    }

    // -----------------------------------------------------------------
    // delete_combo_target / reorder_combo_targets / list_combo_targets_with_model
    // -----------------------------------------------------------------

    /// Seed two providers + two models + a combo + two targets, used
    /// by the delete/reorder tests below. Returns the ids in a
    /// struct so each test can name exactly what it needs.
    struct ComboTargetFixture {
        combo_id: ComboId,
        m1: ModelRowId,
        t1: ComboTargetId,
        t2: ComboTargetId,
    }
    fn seed_combo_with_two_targets(conn: &Connection) -> ComboTargetFixture {
        for pid in ["p1", "p2"] {
            create_provider(
                conn,
                CreateProviderInput { rate_limit_scope: None,
                    id: pid.into(),
                    name: pid.into(),
                    base_url: "https://example.com".into(),
                    auth_type: "bearer".into(),
                    format: "openai".into(),
                    extra_headers_json: None,
                },
            )
            .expect("seed provider");
        }
        let m1 = ModelRowId({
            conn.execute(
                "INSERT INTO models(provider_id, model_id, target_format, display_name) \
                 VALUES ('p1', 'm1', 'openai', 'Model One')",
                [],
            )
            .expect("seed m1");
            conn.query_row("SELECT last_insert_rowid()", [], |r| r.get::<_, i64>(0))
                .expect("last_insert_rowid m1")
        });
        let m2 = ModelRowId({
            conn.execute(
                "INSERT INTO models(provider_id, model_id, target_format, display_name) \
                 VALUES ('p2', 'm2', 'openai', 'Model Two')",
                [],
            )
            .expect("seed m2");
            conn.query_row("SELECT last_insert_rowid()", [], |r| r.get::<_, i64>(0))
                .expect("last_insert_rowid m2")
        });
        let combo_id = create_combo(
            conn,
            CreateComboInput {
                name: "two-target".into(),
                strategy: "priority".into(),
                race_size: None,
                priority_mode: None,
                cooldown_mode: None,
                cooldown_base_secs: None,
                cooldown_max_secs: None,
                cooldown_factor: None,
                lkgp_exploration_rate: None,
                selection_window_secs: None,
            },
        )
        .expect("create combo");
        // Clear the auto-populated targets so the test owns the row
        // set from a known starting state.
        combos::clear_targets(conn, combo_id).expect("clear auto-populate");
        let t1 = add_target_to_combo(
            conn,
            combo_id,
            AddTargetInput {
                provider_id: "p1".into(),
                account_id: None,
                model_row_id: Some(m1),
                sub_combo_id: None,
                priority_order: 10,
            },
        )
        .expect("add t1");
        let t2 = add_target_to_combo(
            conn,
            combo_id,
            AddTargetInput {
                provider_id: "p2".into(),
                account_id: None,
                model_row_id: Some(m2),
                sub_combo_id: None,
                priority_order: 20,
            },
        )
        .expect("add t2");
        ComboTargetFixture {
            combo_id,
            m1,
            t1,
            t2,
        }
    }

    #[test]
    fn delete_combo_target_removes_row() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let fx = seed_combo_with_two_targets(&conn);

        delete_combo_target(&conn, fx.combo_id, fx.t1).expect("delete t1");

        // Only t2 survives.
        let remaining = list_combo_targets(&conn, fx.combo_id).expect("list");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, fx.t2);
    }

    #[test]
    fn delete_combo_target_rejects_cross_combo_id() {
        // The defense-in-depth check: a URL like
        // DELETE /admin/combos/9999/targets/<t1_id> should fail
        // even though <t1_id> is a real target, because it belongs
        // to a different combo. Without the check, a dashboard bug
        // could silently delete a target from the wrong combo.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let fx = seed_combo_with_two_targets(&conn);

        let other_combo = ComboId(99999);
        let err = delete_combo_target(&conn, other_combo, fx.t1)
            .expect_err("cross-combo delete must fail");
        match &err {
            CoreError::Validation(msg) => {
                assert!(
                    msg.contains("not in combo"),
                    "error message must explain the cross-combo mismatch, got: {}",
                    msg
                );
            }
            other => panic!("expected Validation, got {:?}", other),
        }

        // t1 is still present in the original combo.
        let remaining = list_combo_targets(&conn, fx.combo_id).expect("list");
        assert_eq!(
            remaining.len(),
            2,
            "rejected delete must not touch anything"
        );
    }

    #[test]
    fn reorder_combo_targets_assigns_priority_1_2_3() {
        // After reorder, priority_order must be 1, 2, ... in the
        // order the caller sent, regardless of what the values
        // were before.
        let (pool, _path) = fresh_pool();
        let mut conn = pool.writer();
        let fx = seed_combo_with_two_targets(&conn);

        // Reverse the order: send [t2, t1] and expect priority 1 = t2, priority 2 = t1.
        reorder_combo_targets(&mut conn, fx.combo_id, &[fx.t2, fx.t1]).expect("reorder");

        let targets = list_combo_targets(&conn, fx.combo_id).expect("list");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].id, fx.t2, "t2 is now first");
        assert_eq!(targets[0].priority_order, 1);
        assert_eq!(targets[1].id, fx.t1, "t1 is now second");
        assert_eq!(targets[1].priority_order, 2);
    }

    #[test]
    fn reorder_combo_targets_rejects_non_permutation() {
        // Adding an id that doesn't exist in the combo must fail
        // with Validation and leave the priority_order values
        // untouched.
        let (pool, _path) = fresh_pool();
        let mut conn = pool.writer();
        let fx = seed_combo_with_two_targets(&conn);

        // Snapshot the current priorities.
        let before: Vec<i32> = list_combo_targets(&conn, fx.combo_id)
            .expect("list")
            .into_iter()
            .map(|t| t.priority_order)
            .collect();

        let err = reorder_combo_targets(
            &mut conn,
            fx.combo_id,
            &[fx.t1, fx.t2, ComboTargetId(88888)],
        )
        .expect_err("extra id must be rejected");
        assert!(matches!(err, CoreError::Validation(_)));

        // Unchanged.
        let after: Vec<i32> = list_combo_targets(&conn, fx.combo_id)
            .expect("list")
            .into_iter()
            .map(|t| t.priority_order)
            .collect();
        assert_eq!(before, after, "rejected reorder must not touch priorities");
    }

    #[test]
    fn list_combo_targets_with_model_returns_display_name() {
        // The enriched variant must include the model's upstream id
        // and the display name, so the dashboard doesn't have to do
        // a per-row roundtrip.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let fx = seed_combo_with_two_targets(&conn);

        let enriched = list_combo_targets_with_model(&conn, fx.combo_id).expect("list with model");
        assert_eq!(enriched.len(), 2);
        // Order matches `priority_order ASC` (t1=10, t2=20).
        assert_eq!(enriched[0].model_id, "m1");
        assert_eq!(enriched[0].model_display_name.as_deref(), Some("Model One"));
        assert_eq!(enriched[0].model_row_id, Some(fx.m1));
        assert_eq!(enriched[1].model_id, "m2");
        assert_eq!(enriched[1].model_display_name.as_deref(), Some("Model Two"));
    }
}
