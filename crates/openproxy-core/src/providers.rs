//! Provider registry CRUD.
//!
//! See docs/mvp-spec.md §3 (provider adapter interface) and §8 (schema).
//! Providers are the top-level entity: accounts and models hang off them via
//! `ON DELETE CASCADE` foreign keys, so deleting a provider also wipes its
//! accounts and models in a single transaction.

use crate::error::{CoreError, Result};
use crate::ids::ProviderId;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

// Re-export the built-in predicates from `seed` so callers (and the
// admin handlers) can use `providers::is_builtin(...)` without
// reaching across into the `seed` module directly. The
// implementation lives in `seed` because that's where the list of
// built-in providers is defined; the re-export is the
// public-facing handle.
pub use crate::seed::{builtin_provider_ids, is_builtin};

/// Wire format spoken by a provider.
///
/// `Mixed` covers aggregators like OpenCode Zen that serve OpenAI-shaped
/// `/chat/completions` for some models and Anthropic-shaped `/messages` for
/// others; the per-model choice is stored in `models.target_format`.
///
/// `Gemini` covers Google's native Gemini API and Cloud Code (Antigravity)
/// which use the Gemini `contents`/`generationConfig` wire format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderFormat {
    Openai,
    Anthropic,
    Mixed,
    Gemini,
    Responses,
}

impl ProviderFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            ProviderFormat::Openai => "openai",
            ProviderFormat::Anthropic => "anthropic",
            ProviderFormat::Mixed => "mixed",
            ProviderFormat::Gemini => "gemini",
            ProviderFormat::Responses => "responses",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "openai" => Ok(ProviderFormat::Openai),
            "anthropic" => Ok(ProviderFormat::Anthropic),
            "mixed" => Ok(ProviderFormat::Mixed),
            "gemini" => Ok(ProviderFormat::Gemini),
            "responses" => Ok(ProviderFormat::Responses),
            other => Err(CoreError::Validation(format!(
                "invalid provider format: {}",
                other
            ))),
        }
    }
}

/// How the upstream authenticates incoming requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthType {
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// `x-api-key: <key>`. Used by Anthropic.
    XApiKey,
    /// `x-goog-api-key: <key>`. Used by Google Gemini API.
    GoogApiKey,
    /// OAuth 2.0 bearer token (obtained via PKCE or device-code flow).
    OAuth,
    /// Anonymous access — no auth header sent.
    None,
}

impl AuthType {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuthType::Bearer => "bearer",
            AuthType::XApiKey => "x-api-key",
            AuthType::GoogApiKey => "goog-api-key",
            AuthType::OAuth => "oauth",
            AuthType::None => "none",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "bearer" => Ok(AuthType::Bearer),
            "x-api-key" => Ok(AuthType::XApiKey),
            "goog-api-key" => Ok(AuthType::GoogApiKey),
            "oauth" => Ok(AuthType::OAuth),
            "none" => Ok(AuthType::None),
            other => Err(CoreError::Validation(format!(
                "invalid auth_type: {}",
                other
            ))),
        }
    }
}

/// Row view of the `providers` table. `created_at` is the SQLite datetime
/// string (UTC) the DB stamped on insert.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Provider {
    pub id: ProviderId,
    pub name: String,
    pub base_url: String,
    pub auth_type: AuthType,
    pub format: ProviderFormat,
    pub extra_headers_json: Option<String>,
    /// Optional substring the model-refresh path matches against each
    /// discovered `model_id` to decide whether the new row is
    /// `active = 1` (matched) or `active = 0` (unmatched). When `None`,
    /// refresh leaves every discovered model active. Custom (hand-
    /// created) models are never touched by this logic.
    pub auto_activate_keyword: Option<String>,
    /// Soft-disable flag. `true` means the provider is eligible for
    /// routing; `false` means it has been deactivated — combo-target
    /// lookups skip it, but the row (and its accounts/models) stays
    /// in the DB so it can be reactivated later. Defaulted via
    /// `#[serde(default = "default_true")]` so old clients that don't
    /// send `active` (e.g. the built-in seed code, older frontend
    /// snapshots) still see `true` when the row has it set.
    #[serde(default = "default_true")]
    pub active: bool,
    pub created_at: String,
    #[serde(default)]
    pub use_proxies: bool,
    #[serde(default)]
    pub current_proxy_id: Option<String>,
    #[serde(default = "default_proxy_rotation_errors")]
    pub proxy_rotation_errors: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderMetadata {
    pub built_in: bool,
    pub deletable: bool,
    pub supports_quota: bool,
    pub quota_refresh_supported: bool,
}

impl ProviderMetadata {
    pub fn custom_default() -> Self {
        Self {
            built_in: false,
            deletable: true,
            supports_quota: false,
            quota_refresh_supported: false,
        }
    }
}

fn default_proxy_rotation_errors() -> String {
    "429,connect_error,timeout".to_string()
}

fn default_true() -> bool {
    true
}

/// Inputs for [`providers::create`]. Bundled as a struct so the call site
/// can use field names instead of positional args; the DB row is keyed on
/// `id` (PRIMARY KEY) and validates `auth_type` / `format` against the
/// CHECK constraints, so a duplicate id surfaces as `CoreError::Validation`.
pub struct NewProvider<'a> {
    pub id: &'a ProviderId,
    pub name: &'a str,
    pub base_url: &'a str,
    pub auth_type: AuthType,
    pub format: ProviderFormat,
    pub extra_headers_json: Option<&'a str>,
    pub auto_activate_keyword: Option<&'a str>,
}

/// Insert a new provider. The DB enforces uniqueness on `id` (PRIMARY KEY)
/// and validates `auth_type` / `format` against the CHECK constraints; a
/// duplicate id surfaces here as `CoreError::Validation`.
pub fn create(conn: &Connection, new: NewProvider<'_>) -> Result<()> {
    let NewProvider {
        id,
        name,
        base_url,
        auth_type,
        format,
        extra_headers_json,
        auto_activate_keyword,
    } = new;
    let result = conn.execute(
        "INSERT INTO providers(id, name, base_url, auth_type, format, extra_headers_json, auto_activate_keyword) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            id.as_str(),
            name,
            base_url,
            auth_type.as_str(),
            format.as_str(),
            extra_headers_json,
            auto_activate_keyword,
        ],
    );

    match result {
        Ok(_) => Ok(()),
        Err(e) => {
            // SQLite raises a UNIQUE/PRIMARY KEY failure for duplicate ids.
            // We surface it as a validation error per the spec for `create`.
            let msg = e.to_string();
            if msg.contains("UNIQUE") || msg.contains("PRIMARY KEY") {
                Err(CoreError::Validation("provider id already exists".into()))
            } else {
                Err(crate::error::map_db_error_ctx(format!(
                    "insert provider {}",
                    id
                ))(e))
            }
        }
    }
}

/// Look up a single provider by id. Returns `Ok(None)` when absent.
///
/// Returns the row regardless of its `active` bit — this is the raw
/// lookup; the caller decides whether to filter. For the routing path
/// see [`list_active`].
pub fn get(conn: &Connection, id: &ProviderId) -> Result<Option<Provider>> {
    let row = conn
        .query_row(
            "SELECT id, name, base_url, auth_type, format, extra_headers_json, auto_activate_keyword, active, created_at, use_proxies, current_proxy_id, proxy_rotation_errors \
             FROM providers WHERE id = ?1",
            params![id.as_str()],
            row_to_provider,
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx(format!("get provider {}", id)))?;
    Ok(row)
}

/// List all (operator-visible) providers, ordered by id for
/// deterministic output.
///
/// Returns every row *except* the synthetic
/// [`crate::seed::VIRTUAL_COMBO_PROVIDER_ID`] placeholder. That row
/// exists only to satisfy the `combo_targets.provider_id` FK for
/// sub-combo targets and has no adapter, no accounts, and no models;
/// surfacing it on the dashboard would only confuse operators (it
/// cannot be deleted — see [`crate::seed::seed_virtual_combo_provider`]
/// for the rationale). The filter is hard-coded here rather than
/// relying on the caller so the exclusion is uniform across every
/// public endpoint that lists providers.
///
/// Deactivated built-ins are still returned: the dashboard's
/// "Providers" page wants to see *all* rows (with a visual marker
/// for inactive ones) so an operator can reactivate a disabled
/// provider without first having to know its id. The routing path
/// that picks active providers only is [`list_active`].
pub fn list(conn: &Connection) -> Result<Vec<Provider>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, base_url, auth_type, format, extra_headers_json, auto_activate_keyword, active, created_at, use_proxies, current_proxy_id, proxy_rotation_errors \
             FROM providers WHERE id != ?1 ORDER BY id",
        )
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map(
            params![crate::seed::VIRTUAL_COMBO_PROVIDER_ID],
            row_to_provider,
        )
        .map_err(crate::error::map_db_error)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(crate::error::map_db_error)?);
    }
    Ok(out)
}

/// List only providers with `active = 1`. Used by code paths that
/// decide what's routable today (combo-target resolution, the model
/// refresh page) — a deactivated provider must not show up as a
/// candidate for new combos or be used in routing decisions.
///
/// Like [`list`], the synthetic
/// [`crate::seed::VIRTUAL_COMBO_PROVIDER_ID`] placeholder is excluded
/// so it never bleeds into routing decisions (it has `active = 1`
/// but no adapter and no accounts).
///
/// Ordered by id to match [`list`].
pub fn list_active(conn: &Connection) -> Result<Vec<Provider>> {
    let mut stmt = conn
        .prepare(
            "SELECT id, name, base_url, auth_type, format, extra_headers_json, auto_activate_keyword, active, created_at, use_proxies, current_proxy_id, proxy_rotation_errors \
             FROM providers WHERE active = 1 AND id != ?1 ORDER BY id",
        )
        .map_err(crate::error::map_db_error)?;
    let rows = stmt
        .query_map(
            params![crate::seed::VIRTUAL_COMBO_PROVIDER_ID],
            row_to_provider,
        )
        .map_err(crate::error::map_db_error)?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r.map_err(crate::error::map_db_error)?);
    }
    Ok(out)
}

/// Flip the `active` flag on a single provider. A missing id is a
/// silent no-op (0 rows affected) — matches the idempotent style of
/// the other `*_delete` / `*_set_*` helpers so the handler doesn't
/// have to special-case a 404.
pub fn set_active(conn: &Connection, id: &ProviderId, active: bool) -> Result<()> {
    conn.execute(
        "UPDATE providers SET active = ?1 WHERE id = ?2",
        params![active as i64, id.as_str()],
    )
    .map_err(crate::error::map_db_error_ctx(format!(
        "set active for provider {}",
        id
    )))?;
    Ok(())
}

/// Delete a provider by id. FK cascade wipes its accounts and models.
/// A missing id is a no-op (0 rows affected), not an error: deletes are
/// idempotent.
pub fn delete(conn: &Connection, id: &ProviderId) -> Result<()> {
    conn.execute("DELETE FROM providers WHERE id = ?1", params![id.as_str()])
        .map_err(crate::error::map_db_error_ctx(format!(
            "delete provider {}",
            id
        )))?;
    Ok(())
}

/// Partial update: only the fields the caller supplies are touched.
/// `auth_type` and `format` are intentionally not updatable here — they are
/// structural and changing them mid-flight would invalidate routing state.
/// CHECK constraints in the schema validate `auth_type` / `format` on read.
///
/// `auto_activate_keyword` uses a three-state encoding so the caller can
/// distinguish "leave it alone" from "set it to NULL":
/// * `None` — column is not part of this update (no-op).
/// * `Some(None)` — set the column to `NULL` (clears any existing keyword).
/// * `Some(Some(s))` — set the column to the literal string `s`.
// ponytail: [Demasiados argumentos] -> [Refactorizar a struct en el futuro]
pub fn update(
    conn: &Connection,
    id: &ProviderId,
    name: Option<&str>,
    base_url: Option<&str>,
    extra_headers_json: Option<&str>,
    auto_activate_keyword: Option<Option<&str>>,
    use_proxies: Option<bool>,
    proxy_rotation_errors: Option<&str>,
) -> Result<()> {
    // Build the SET clause dynamically so we only touch the supplied columns.
    // Each branch adds a fragment plus its bound value to `bound_values`.
    let mut sets: Vec<&'static str> = Vec::new();
    let mut bound_values: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(v) = name {
        sets.push("name = ?");
        bound_values.push(Box::new(v.to_string()));
    }
    if let Some(v) = base_url {
        sets.push("base_url = ?");
        bound_values.push(Box::new(v.to_string()));
    }
    if let Some(v) = extra_headers_json {
        sets.push("extra_headers_json = ?");
        bound_values.push(Box::new(v.to_string()));
    }
    if let Some(v) = auto_activate_keyword {
        sets.push("auto_activate_keyword = ?");
        bound_values.push(Box::new(v.map(|s| s.to_string())));
    }
    if let Some(v) = use_proxies {
        sets.push("use_proxies = ?");
        bound_values.push(Box::new(v as i64));
    }
    if let Some(v) = proxy_rotation_errors {
        sets.push("proxy_rotation_errors = ?");
        bound_values.push(Box::new(v.to_string()));
    }

    if sets.is_empty() {
        // Nothing to update. Don't issue a no-op UPDATE; just verify the row
        // exists so the caller gets a consistent "missing id" signal.
        if get(conn, id)?.is_none() {
            return Err(CoreError::ProviderNotFound(id.to_string()));
        }
        return Ok(());
    }

    let sql = format!("UPDATE providers SET {} WHERE id = ?", sets.join(", "));

    // The id is bound last; promote it to an owned String so the borrow
    // lives for the duration of `execute`.
    let id_owned = id.as_str().to_string();
    let mut bound: Vec<&dyn rusqlite::ToSql> = Vec::new();
    for b in &bound_values {
        bound.push(b.as_ref());
    }
    bound.push(&id_owned);

    let affected = conn
        .execute(&sql, rusqlite::params_from_iter(bound.iter().copied()))
        .map_err(crate::error::map_db_error_ctx(format!(
            "update provider {}",
            id
        )))?;

    if affected == 0 {
        return Err(CoreError::ProviderNotFound(id.to_string()));
    }
    Ok(())
}

/// Update the current proxy ID assigned to a provider.
pub fn update_current_proxy(
    conn: &Connection,
    id: &ProviderId,
    proxy_id: Option<&str>,
) -> Result<()> {
    conn.execute(
        "UPDATE providers SET current_proxy_id = ?1 WHERE id = ?2",
        params![proxy_id, id.as_str()],
    )
    .map_err(crate::error::map_db_error_ctx(format!(
        "update current proxy for provider {}",
        id
    )))?;
    Ok(())
}

/// Map a single SELECT row into a `Provider`. Shared by `get`, `list`,
/// and `list_active`. The expected column order is the SELECT in each
/// of those three queries — column index `7` is the `active` flag.
fn row_to_provider(row: &rusqlite::Row<'_>) -> rusqlite::Result<Provider> {
    let id: String = row.get(0)?;
    let name: String = row.get(1)?;
    let base_url: String = row.get(2)?;
    let auth_type: String = row.get(3)?;
    let format: String = row.get(4)?;
    let extra_headers_json: Option<String> = row.get(5)?;
    let auto_activate_keyword: Option<String> = row.get(6)?;
    let active: i64 = row.get(7)?;
    let created_at: String = row.get(8)?;

    let use_proxies: i64 = row.get(9)?;
    let current_proxy_id: Option<String> = row.get(10)?;
    let proxy_rotation_errors: String = row.get(11)?;

    // The DB's CHECK constraints guarantee these parse, so a Validation
    // error here would indicate schema/data corruption, not a user mistake.
    // Map to a rusqlite error so the caller surfaces it as Database.
    let auth_type = AuthType::parse(&auth_type).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(FromStrError(format!("{}", e))),
        )
    })?;
    let format = ProviderFormat::parse(&format).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            4,
            rusqlite::types::Type::Text,
            Box::new(FromStrError(format!("{}", e))),
        )
    })?;

    // `active` is a 0/1 INTEGER; anything other than 1 is treated as
    // inactive. The schema's CHECK constraint prevents other values at
    // the DB level, so a non-0/1 reading indicates on-disk corruption
    // — the same mapping is used by `accounts::HealthStatus` and
    // `models::set_active`.
    let active = active != 0;
    let use_proxies = use_proxies != 0;

    Ok(Provider {
        id: ProviderId::new(id),
        name,
        base_url,
        auth_type,
        format,
        extra_headers_json,
        auto_activate_keyword,
        active,
        created_at,
        use_proxies,
        current_proxy_id,
        proxy_rotation_errors,
    })
}

#[derive(Debug)]
struct FromStrError(String);
impl std::fmt::Display for FromStrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for FromStrError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::conn::DbPool;
    use crate::db::migrations;
    use std::path::PathBuf;

    /// Build an in-memory pool for one test: temp dir on disk (rusqlite's
    /// `:memory:` doesn't survive `DbPool`'s two-handle open), run migrations,
    /// return the pool.
    fn fresh_pool() -> (DbPool, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("openproxy-providers-test-{}-{}-{}", pid, nanos, n));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("providers.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    #[test]
    fn create_and_get() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        let id = ProviderId::new("openrouter");
        create(
            &conn,
            NewProvider {
                id: &id,
                name: "OpenRouter",
                base_url: "https://openrouter.ai/api/v1",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: Some(r#"{"X-Title":"openproxy"}"#),
                auto_activate_keyword: Some("claude"),
            },
        )
        .expect("create");

        let got = get(&conn, &id).expect("get").expect("present");
        assert_eq!(got.id, id);
        assert_eq!(got.name, "OpenRouter");
        assert_eq!(got.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(got.auth_type, AuthType::Bearer);
        assert_eq!(got.format, ProviderFormat::Openai);
        assert_eq!(
            got.extra_headers_json.as_deref(),
            Some(r#"{"X-Title":"openproxy"}"#)
        );
        assert_eq!(got.auto_activate_keyword.as_deref(), Some("claude"));
        assert!(!got.created_at.is_empty(), "created_at stamped by DB");
    }

    #[test]
    fn create_duplicate_id_fails() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        let id = ProviderId::new("anthropic");
        create(
            &conn,
            NewProvider {
                id: &id,
                name: "Anthropic",
                base_url: "https://api.anthropic.com",
                auth_type: AuthType::XApiKey,
                format: ProviderFormat::Anthropic,
                extra_headers_json: None,
                auto_activate_keyword: None,
            },
        )
        .expect("first create");

        let err = create(
            &conn,
            NewProvider {
                id: &id,
                name: "Dup",
                base_url: "https://dup.example",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
            },
        )
        .expect_err("duplicate must fail");
        match err {
            CoreError::Validation(msg) => assert_eq!(msg, "provider id already exists"),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn list_returns_all() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        for (id, name) in [("a", "A"), ("b", "B"), ("c", "C")] {
            create(
                &conn,
                NewProvider {
                    id: &ProviderId::new(id),
                    name,
                    base_url: "https://example.com",
                    auth_type: AuthType::Bearer,
                    format: ProviderFormat::Openai,
                    extra_headers_json: None,
                    auto_activate_keyword: None,
                },
            )
            .expect("create");
        }

        let all = list(&conn).expect("list");
        assert_eq!(all.len(), 3);
        // Ordered by id ASC.
        assert_eq!(all[0].id, ProviderId::new("a"));
        assert_eq!(all[1].id, ProviderId::new("b"));
        assert_eq!(all[2].id, ProviderId::new("c"));
    }

    #[test]
    fn delete_removes_provider() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        let id = ProviderId::new("to-delete");
        create(
            &conn,
            NewProvider {
                id: &id,
                name: "X",
                base_url: "https://x.example",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
            },
        )
        .expect("create");

        // Pre-seed an account that should be cascade-deleted with the provider.
        // api_key_encrypted is BLOB; we don't need a real key, just any bytes.
        conn.execute(
            "INSERT INTO accounts(provider_id, api_key_encrypted) VALUES (?1, ?2)",
            rusqlite::params![id.as_str(), &[1u8, 2, 3][..]],
        )
        .expect("seed account");

        let accounts_before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM accounts WHERE provider_id = ?1",
                rusqlite::params![id.as_str()],
                |r| r.get(0),
            )
            .expect("count accounts");
        assert_eq!(accounts_before, 1, "account seeded");

        delete(&conn, &id).expect("delete");

        assert!(get(&conn, &id).expect("get").is_none(), "provider gone");
        let accounts_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM accounts WHERE provider_id = ?1",
                rusqlite::params![id.as_str()],
                |r| r.get(0),
            )
            .expect("count accounts");
        assert_eq!(accounts_after, 0, "FK cascade removed the account");

        // Idempotent: a second delete is a no-op, not an error.
        delete(&conn, &id).expect("delete again is fine");
    }

    #[test]
    fn update_modifies_fields() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();

        let id = ProviderId::new("upd");
        create(
            &conn,
            NewProvider {
                id: &id,
                name: "Original",
                base_url: "https://original.example",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: Some(r#"{"old":true}"#),
                auto_activate_keyword: None,
            },
        )
        .expect("create");

        // Partial: only name.
        update(&conn, &id, Some("Renamed"), None, None, None, None, None).expect("update name");
        let p = get(&conn, &id).expect("get").expect("present");
        assert_eq!(p.name, "Renamed");
        assert_eq!(p.base_url, "https://original.example", "untouched");
        assert_eq!(
            p.extra_headers_json.as_deref(),
            Some(r#"{"old":true}"#),
            "untouched"
        );
        assert_eq!(p.auto_activate_keyword, None, "untouched");

        // Partial: base_url, extra_headers_json, keyword (set), name untouched.
        update(
            &conn,
            &id,
            None,
            Some("https://new.example"),
            Some(r#"{"new":true}"#),
            Some(Some("claude")),
            None,
            None,
        )
        .expect("update url+headers+keyword");
        let p = get(&conn, &id).expect("get").expect("present");
        assert_eq!(p.name, "Renamed", "untouched");
        assert_eq!(p.base_url, "https://new.example");
        assert_eq!(p.extra_headers_json.as_deref(), Some(r#"{"new":true}"#));
        assert_eq!(p.auto_activate_keyword.as_deref(), Some("claude"));

        // Clear the keyword: Some(None) sets NULL.
        update(&conn, &id, None, None, None, Some(None), None, None).expect("clear keyword");
        let p = get(&conn, &id).expect("get").expect("present");
        assert_eq!(p.auto_activate_keyword, None);

        // No-op update on an existing id: should not error and not touch row.
        update(&conn, &id, None, None, None, None, None, None).expect("no-op");
        let p = get(&conn, &id).expect("get").expect("present");
        assert_eq!(p.base_url, "https://new.example");

        // Update on a missing id: ProviderNotFound.
        let missing = ProviderId::new("nope");
        let err = update(&conn, &missing, Some("X"), None, None, None, None, None)
            .expect_err("missing id must error");
        assert!(matches!(err, CoreError::ProviderNotFound(_)));
    }

    #[test]
    fn provider_format_parse_roundtrip() {
        for (variant, s) in [
            (ProviderFormat::Openai, "openai"),
            (ProviderFormat::Anthropic, "anthropic"),
            (ProviderFormat::Mixed, "mixed"),
            (ProviderFormat::Gemini, "gemini"),
        ] {
            assert_eq!(variant.as_str(), s);
            assert_eq!(ProviderFormat::parse(s).expect("parse"), variant);
        }
        assert!(ProviderFormat::parse("bogus").is_err());
    }

    #[test]
    fn auth_type_parse_roundtrip() {
        for (variant, s) in [
            (AuthType::Bearer, "bearer"),
            (AuthType::XApiKey, "x-api-key"),
            (AuthType::GoogApiKey, "goog-api-key"),
            (AuthType::OAuth, "oauth"),
            (AuthType::None, "none"),
        ] {
            assert_eq!(variant.as_str(), s);
            assert_eq!(AuthType::parse(s).expect("parse"), variant);
        }
        assert!(AuthType::parse("basic").is_err());
    }

    #[test]
    fn new_providers_default_to_active() {
        // The migration stamps `active = 1` as the default, so a brand-
        // new row comes back with `active = true` without the caller
        // having to think about it.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let id = ProviderId::new("active-by-default");
        create(
            &conn,
            NewProvider {
                id: &id,
                name: "X",
                base_url: "https://x.example",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
            },
        )
        .expect("create");
        let got = get(&conn, &id).expect("get").expect("present");
        assert!(got.active, "freshly created providers are active");
    }

    #[test]
    fn set_active_flips_and_idempotent() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let id = ProviderId::new("toggle");
        create(
            &conn,
            NewProvider {
                id: &id,
                name: "T",
                base_url: "https://t.example",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
            },
        )
        .expect("create");

        set_active(&conn, &id, false).expect("deactivate");
        let p = get(&conn, &id).expect("get").expect("present");
        assert!(!p.active, "deactivated");

        set_active(&conn, &id, false).expect("re-apply is a no-op, not an error");
        let p = get(&conn, &id).expect("get").expect("present");
        assert!(!p.active);

        set_active(&conn, &id, true).expect("reactivate");
        let p = get(&conn, &id).expect("get").expect("present");
        assert!(p.active, "reactivated");

        // Missing id is a silent no-op (matches the idempotent style of
        // delete / set_active elsewhere).
        set_active(&conn, &ProviderId::new("does-not-exist"), false)
            .expect("missing id is a no-op");
    }

    #[test]
    fn list_active_filters_out_inactive() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        for (id, name) in [("a", "A"), ("b", "B"), ("c", "C")] {
            create(
                &conn,
                NewProvider {
                    id: &ProviderId::new(id),
                    name,
                    base_url: "https://x.example",
                    auth_type: AuthType::Bearer,
                    format: ProviderFormat::Openai,
                    extra_headers_json: None,
                    auto_activate_keyword: None,
                },
            )
            .expect("create");
        }

        // All active initially.
        let active = list_active(&conn).expect("list active");
        assert_eq!(active.len(), 3, "all three initially active");

        // `list` still returns all three (deactivated rows aren't hidden
        // from the dashboard).
        let all = list(&conn).expect("list");
        assert_eq!(all.len(), 3);

        // Deactivate `b`.
        set_active(&conn, &ProviderId::new("b"), false).expect("deactivate b");

        let active = list_active(&conn).expect("list active");
        assert_eq!(active.len(), 2, "b is filtered out");
        let ids: Vec<&str> = active.iter().map(|p| p.id.as_str()).collect();
        assert!(ids.contains(&"a") && ids.contains(&"c"));
        assert!(!ids.contains(&"b"));

        // `list` still shows b so the operator can see and reactivate.
        let all = list(&conn).expect("list");
        assert_eq!(all.len(), 3);
        let b = all
            .iter()
            .find(|p| p.id == ProviderId::new("b"))
            .expect("b present");
        assert!(!b.active, "b is marked inactive in the full list");
    }

    #[test]
    fn list_and_list_active_hide_virtual_combo_provider() {
        // The synthetic `combo` row exists only to satisfy the
        // `combo_targets.provider_id` FK for sub-combo targets. It has
        // `active = 1` but no adapter and no accounts, so it must not
        // appear in any operator-facing listing.
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        for (id, name) in [("a", "A"), ("b", "B")] {
            create(
                &conn,
                NewProvider {
                    id: &ProviderId::new(id),
                    name,
                    base_url: "https://x.example",
                    auth_type: AuthType::Bearer,
                    format: ProviderFormat::Openai,
                    extra_headers_json: None,
                    auto_activate_keyword: None,
                },
            )
            .expect("create");
        }
        // Seed the virtual row the same way `seed_virtual_combo_provider`
        // would (it is normally seeded by the bootstrap, not by
        // `list`/`list_active` callers, so we replicate the row here).
        crate::seed::seed_virtual_combo_provider(&conn).expect("seed virtual");

        // Raw table has 3 rows (the virtual one is present).
        let raw_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM providers", [], |r| r.get(0))
            .expect("count");
        assert_eq!(raw_count, 3, "raw table contains the virtual row");

        // `list` and `list_active` both hide it.
        let all = list(&conn).expect("list");
        assert_eq!(all.len(), 2, "list hides the virtual combo row");
        assert!(
            all.iter()
                .all(|p| p.id.as_str() != crate::seed::VIRTUAL_COMBO_PROVIDER_ID),
            "virtual combo provider id absent from list()"
        );

        let active = list_active(&conn).expect("list_active");
        assert_eq!(active.len(), 2, "list_active hides the virtual combo row");
        assert!(
            active
                .iter()
                .all(|p| p.id.as_str() != crate::seed::VIRTUAL_COMBO_PROVIDER_ID),
            "virtual combo provider id absent from list_active()"
        );

        // `get` still returns it: a direct lookup is not a list, and
        // other code paths (e.g. `combo_targets` joins) need to be
        // able to read the row to resolve sub-combo targets.
        let got = get(
            &conn,
            &ProviderId::new(crate::seed::VIRTUAL_COMBO_PROVIDER_ID),
        )
        .expect("get")
        .expect("present");
        assert_eq!(got.id.as_str(), crate::seed::VIRTUAL_COMBO_PROVIDER_ID);
    }
}
