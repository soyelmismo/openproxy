//! Provider registry CRUD.
//!
//! See docs/mvp-spec.md §3 (provider adapter interface) and §8 (schema).
//! Providers are the top-level entity: accounts and models hang off them via
//! `ON DELETE CASCADE` foreign keys, so deleting a provider also wipes its
//! accounts and models in a single transaction.

use crate::error::{CoreError, Result};
use crate::ids::ProviderId;
pub use openproxy_types::providers::*;
use rusqlite::{Connection, OptionalExtension, params};

// Re-export the built-in predicates from `seed` so callers (and the
// admin handlers) can use `providers::is_builtin(...)` without
// reaching across into the `seed` module directly. The
// implementation lives in `seed` because that's where the list of
// built-in providers is defined; the re-export is the
// public-facing handle.
pub use crate::seed::{builtin_provider_ids, is_builtin};

/// Inputs for [`providers::create`]. Bundled as a struct so the call site
/// can use field names instead of positional args; the DB row is keyed on
/// `id` (PRIMARY KEY) and validates `auth_type` / `format` against the
/// CHECK constraints, so a duplicate id surfaces as `CoreError::Validation`.
#[derive(Debug, Clone, Copy)]
pub struct NewProvider<'a> {
    pub id: &'a ProviderId,
    pub name: &'a str,
    pub base_url: &'a str,
    pub auth_type: AuthType,
    pub format: ProviderFormat,
    pub extra_headers_json: Option<&'a str>,
    pub auto_activate_keyword: Option<&'a str>,
    pub rate_limit_scope: RateLimitScope,
}

fn map_create_provider_error(e: rusqlite::Error, id: &ProviderId) -> CoreError {
    let msg = e.to_string();
    if msg.contains("UNIQUE") || msg.contains("PRIMARY KEY") {
        CoreError::Validation("provider id already exists".into())
    } else {
        openproxy_db::error::map_db_error_ctx(format!("insert provider {id}"))(e)
    }
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
        rate_limit_scope,
    } = new;
    conn.execute(
        "INSERT INTO providers(id, name, base_url, auth_type, format, extra_headers_json, auto_activate_keyword, rate_limit_scope) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id.as_str(),
            name,
            base_url,
            auth_type.as_str(),
            format.as_str(),
            extra_headers_json,
            auto_activate_keyword,
            rate_limit_scope.as_str(),
        ],
    )
    .map_err(|e| map_create_provider_error(e, id))?;
    Ok(())
}

/// Look up a single provider by id. Returns `Ok(None)` when absent.
///
/// Returns the row regardless of its `active` bit — this is the raw
/// lookup; the caller decides whether to filter. For the routing path
/// see [`list_active`].
pub fn get(conn: &Connection, id: &ProviderId) -> Result<Option<Provider>> {
    let row = conn
        .query_row(
            "SELECT id, name, base_url, auth_type, format, extra_headers_json, auto_activate_keyword, active, created_at, use_proxies, current_proxy_id, proxy_rotation_errors, rate_limit_scope, proxy_rotation_mode, favicon_base64 \
             FROM providers WHERE id = ?1",
            params![id.as_str()],
            row_to_provider,
        )
        .optional()
        .map_err(openproxy_db::error::map_db_error_ctx(format!("get provider {id}")))?;
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
            "SELECT id, name, base_url, auth_type, format, extra_headers_json, auto_activate_keyword, active, created_at, use_proxies, current_proxy_id, proxy_rotation_errors, rate_limit_scope, proxy_rotation_mode, favicon_base64 \
             FROM providers WHERE id != ?1 ORDER BY id",
        )
        .map_err(openproxy_db::error::map_db_error)?;
    let rows = stmt
        .query_map(
            params![crate::seed::VIRTUAL_COMBO_PROVIDER_ID],
            row_to_provider,
        )
        .map_err(openproxy_db::error::map_db_error)?;
    rows.map(|r| r.map_err(openproxy_db::error::map_db_error))
        .collect()
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
            "SELECT id, name, base_url, auth_type, format, extra_headers_json, auto_activate_keyword, active, created_at, use_proxies, current_proxy_id, proxy_rotation_errors, rate_limit_scope, proxy_rotation_mode, favicon_base64 \
             FROM providers WHERE active = 1 AND id != ?1 ORDER BY id",
        )
        .map_err(openproxy_db::error::map_db_error)?;
    let rows = stmt
        .query_map(
            params![crate::seed::VIRTUAL_COMBO_PROVIDER_ID],
            row_to_provider,
        )
        .map_err(openproxy_db::error::map_db_error)?;
    rows.map(|r| r.map_err(openproxy_db::error::map_db_error))
        .collect()
}

/// Flip the `active` flag on a single provider. A missing id is a
/// silent no-op (0 rows affected) — matches the idempotent style of
/// the other `*_delete` / `*_set_*` helpers so the handler doesn't
/// have to special-case a 404.
pub fn set_active(conn: &Connection, id: &ProviderId, active: bool) -> Result<()> {
    conn.execute(
        "UPDATE providers SET active = ?1 WHERE id = ?2",
        params![i64::from(active), id.as_str()],
    )
    .map_err(openproxy_db::error::map_db_error_ctx(format!(
        "set active for provider {id}"
    )))?;
    Ok(())
}

/// Update the `favicon_base64` column for a provider.
pub fn set_favicon(conn: &Connection, id: &ProviderId, favicon: &str) -> Result<()> {
    conn.execute(
        "UPDATE providers SET favicon_base64 = ?1 WHERE id = ?2",
        params![favicon, id.as_str()],
    )
    .map_err(openproxy_db::error::map_db_error_ctx(format!(
        "set favicon for provider {id}"
    )))?;
    Ok(())
}

/// Extract clean domain/host from a base_url string.
pub fn extract_domain(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    let stripped = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let host_part = stripped.split_once('/').map_or(stripped, |(h, _)| h);
    let host = host_part
        .split_once(':')
        .map_or(host_part, |(h, _)| h)
        .trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn is_compound_tld(second_to_last: &str, last: &str) -> bool {
    matches!(second_to_last, "co" | "com" | "org" | "net" | "gov" | "edu") && last.len() == 2
}

/// Extract apex/root domain from host (e.g. "api.fireworks.ai" -> "fireworks.ai").
pub fn extract_apex_domain(host: &str) -> String {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() <= 2 {
        return host.to_string();
    }
    let second_to_last = parts[parts.len() - 2];
    let last = parts[parts.len() - 1];
    if is_compound_tld(second_to_last, last) && parts.len() >= 3 {
        parts[parts.len() - 3..].join(".")
    } else {
        parts[parts.len() - 2..].join(".")
    }
}

/// Fetch the favicon for a provider from multiple fallback sources
/// as a data URI (`data:image/png;base64,...`).
pub async fn fetch_favicon_data_uri(
    base_url: &str,
    upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
) -> Option<String> {
    let host = extract_domain(base_url)?;
    let apex = extract_apex_domain(&host);

    let mut domains = vec![host.clone()];
    if apex != host {
        domains.push(apex);
    }

    async fn try_fetch_b64(
        upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
        url: &str,
        mime: &str,
    ) -> Option<String> {
        let bytes = openproxy_adapters::adapters::upstream_get_bytes(upstream_client, url, &[])
            .await
            .ok()?;
        if bytes.len() > 100 {
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Some(format!("data:{mime};base64,{b64}"))
        } else {
            None
        }
    }

    for domain in domains {
        if let Some(data) = try_fetch_b64(
            upstream_client,
            &format!("https://www.google.com/s2/favicons?domain={domain}&sz=64"),
            "image/png",
        )
        .await
        {
            return Some(data);
        }

        if let Some(data) = try_fetch_b64(
            upstream_client,
            &format!("https://icons.duckduckgo.com/ip3/{domain}.ico"),
            "image/x-icon",
        )
        .await
        {
            return Some(data);
        }

        if let Some(data) = try_fetch_b64(
            upstream_client,
            &format!("https://{domain}/favicon.ico"),
            "image/x-icon",
        )
        .await
        {
            return Some(data);
        }
    }

    None
}

/// Fetch the favicon for a provider and store it in the database.
pub async fn fetch_and_cache_favicon(
    db_pool: &std::sync::Arc<openproxy_db::conn::DbPool>,
    id: &ProviderId,
    base_url: &str,
    upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
) -> Result<()> {
    if let Some(data_uri) = fetch_favicon_data_uri(base_url, upstream_client).await {
        let pool = std::sync::Arc::clone(db_pool);
        let id_clone = id.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.writer();
            set_favicon(&conn, &id_clone, &data_uri)
        })
        .await
        .map_err(|e| openproxy_types::error::CoreError::Internal(format!("join error: {e}")))??;
    }
    Ok(())
}

/// Delete a provider by id. FK cascade wipes its accounts and models.
/// A missing id is a no-op (0 rows affected), not an error: deletes are
/// idempotent.
pub fn delete(conn: &Connection, id: &ProviderId) -> Result<()> {
    conn.execute("DELETE FROM providers WHERE id = ?1", params![id.as_str()])
        .map_err(openproxy_db::error::map_db_error_ctx(format!(
            "delete provider {id}"
        )))?;
    Ok(())
}

/// Inputs for [`providers::update`]. Bundled as a struct to avoid excessive
/// function arguments and improve API extensibility.
#[derive(Debug, Clone, Copy, Default)]
pub struct UpdateProviderParams<'a> {
    pub name: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub extra_headers_json: Option<Option<&'a str>>,
    pub auto_activate_keyword: Option<Option<&'a str>>,
    pub use_proxies: Option<bool>,
    pub proxy_rotation_errors: Option<&'a str>,
    pub proxy_rotation_mode: Option<&'a str>,
    pub rate_limit_scope: Option<RateLimitScope>,
}

/// Partial update: only the fields the caller supplies are touched.
/// `auth_type` and `format` are intentionally not updatable here — they are
/// structural and changing them mid-flight would invalidate routing state.
/// CHECK constraints in the schema validate `auth_type` / `format` on read.
///
/// `auto_activate_keyword` and `extra_headers_json` use a three-state encoding so the caller can
/// distinguish "leave it alone" from "set it to NULL":
/// * `None` — column is not part of this update (no-op).
/// * `Some(None)` — set the column to `NULL` (clears any existing value).
/// * `Some(Some(s))` — set the column to the literal string `s`.
fn build_provider_update_clauses(
    params: &UpdateProviderParams<'_>,
    sets: &mut Vec<&'static str>,
    bound_values: &mut Vec<Box<dyn rusqlite::ToSql>>,
) {
    if let Some(v) = params.name {
        sets.push("name = ?");
        bound_values.push(Box::new(v.to_string()));
    }
    if let Some(v) = params.base_url {
        sets.push("base_url = ?");
        bound_values.push(Box::new(v.to_string()));
    }
    if let Some(v) = params.extra_headers_json {
        sets.push("extra_headers_json = ?");
        bound_values.push(Box::new(v.map(std::string::ToString::to_string)));
    }
    if let Some(v) = params.auto_activate_keyword {
        sets.push("auto_activate_keyword = ?");
        bound_values.push(Box::new(v.map(std::string::ToString::to_string)));
    }
    if let Some(v) = params.use_proxies {
        sets.push("use_proxies = ?");
        bound_values.push(Box::new(i64::from(v)));
    }
    if let Some(v) = params.proxy_rotation_errors {
        sets.push("proxy_rotation_errors = ?");
        bound_values.push(Box::new(v.to_string()));
    }
    if let Some(v) = params.proxy_rotation_mode {
        sets.push("proxy_rotation_mode = ?");
        bound_values.push(Box::new(v.to_string()));
    }
    if let Some(v) = params.rate_limit_scope {
        sets.push("rate_limit_scope = ?");
        bound_values.push(Box::new(v.as_str().to_string()));
    }
}

/// Partial update: only the fields the caller supplies are touched.
/// `auth_type` and `format` are intentionally not updatable here — they are
/// structural and changing them mid-flight would invalidate routing state.
/// CHECK constraints in the schema validate `auth_type` / `format` on read.
///
/// `auto_activate_keyword` and `extra_headers_json` use a three-state encoding so the caller can
/// distinguish "leave it alone" from "set it to NULL":
/// * `None` — column is not part of this update (no-op).
/// * `Some(None)` — set the column to `NULL` (clears any existing value).
/// * `Some(Some(s))` — set the column to the literal string `s`.
pub fn update(conn: &Connection, id: &ProviderId, params: UpdateProviderParams<'_>) -> Result<()> {
    let mut sets = Vec::new();
    let mut bound_values = Vec::new();
    build_provider_update_clauses(&params, &mut sets, &mut bound_values);

    if sets.is_empty() {
        if get(conn, id)?.is_none() {
            return Err(CoreError::ProviderNotFound(id.to_string()));
        }
        return Ok(());
    }

    let sql = format!("UPDATE providers SET {} WHERE id = ?", sets.join(", "));
    let id_owned = id.as_str().to_string();
    let mut bound: Vec<&dyn rusqlite::ToSql> = bound_values.iter().map(|b| b.as_ref()).collect();
    bound.push(&id_owned);

    let affected = conn
        .execute(&sql, rusqlite::params_from_iter(bound.iter().copied()))
        .map_err(openproxy_db::error::map_db_error_ctx(format!(
            "update provider {id}"
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
    .map_err(openproxy_db::error::map_db_error_ctx(format!(
        "update current proxy for provider {id}"
    )))?;
    Ok(())
}

fn parse_from_sql<T, F>(val: &str, col: usize, parser: F) -> rusqlite::Result<T>
where
    F: FnOnce(&str) -> std::result::Result<T, String>,
{
    parser(val).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            col,
            rusqlite::types::Type::Text,
            Box::new(FromStrError(e)),
        )
    })
}

/// Map a single SELECT row into a `Provider`. Shared by `get`, `list`,
/// and `list_active`. The expected column order is the SELECT in each
/// of those three queries — column index `7` is the `active` flag.
fn row_to_provider(row: &rusqlite::Row<'_>) -> rusqlite::Result<Provider> {
    let id: String = row.get(0)?;
    let name: String = row.get(1)?;
    let base_url: String = row.get(2)?;
    let auth_type_str: String = row.get(3)?;
    let format_str: String = row.get(4)?;
    let extra_headers_json: Option<String> = row.get(5)?;
    let auto_activate_keyword: Option<String> = row.get(6)?;
    let active: i64 = row.get(7)?;
    let created_at: String = row.get(8)?;

    let use_proxies: i64 = row.get(9)?;
    let current_proxy_id: Option<String> = row.get(10)?;
    let proxy_rotation_errors: String = row.get(11)?;
    let rate_limit_scope_str: String = row.get(12)?;

    let auth_type = parse_from_sql(&auth_type_str, 3, AuthType::parse)?;
    let format = parse_from_sql(&format_str, 4, ProviderFormat::parse)?;
    let rate_limit_scope = parse_from_sql(&rate_limit_scope_str, 12, RateLimitScope::parse)?;

    let active = active != 0;
    let use_proxies = use_proxies != 0;
    let proxy_rotation_mode: String = row.get(13)?;
    let favicon_base64: Option<String> = row.get(14)?;

    Ok(Provider {
        id: ProviderId::new(id),
        name: name.into_boxed_str(),
        base_url: base_url.into_boxed_str(),
        auth_type,
        format,
        extra_headers_json: extra_headers_json.map(String::into_boxed_str),
        auto_activate_keyword: auto_activate_keyword.map(String::into_boxed_str),
        active,
        created_at: created_at.into_boxed_str(),
        use_proxies,
        current_proxy_id: current_proxy_id.map(String::into_boxed_str),
        proxy_rotation_errors: proxy_rotation_errors.into_boxed_str(),
        rate_limit_scope,
        proxy_rotation_mode: proxy_rotation_mode.into_boxed_str(),
        favicon_base64: favicon_base64.map(String::into_boxed_str),
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
    use openproxy_db::conn::DbPool;

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
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("openproxy-providers-test-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("providers.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            openproxy_db::migrations::run(&mut w).expect("migrations");
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
                rate_limit_scope: crate::providers::RateLimitScope::Account,
            },
        )
        .expect("create");

        let got = get(&conn, &id).expect("get").expect("present");
        assert_eq!(got.id, id);
        assert_eq!(&*got.name, "OpenRouter");
        assert_eq!(&*got.base_url, "https://openrouter.ai/api/v1");
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
                rate_limit_scope: crate::providers::RateLimitScope::Account,
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
                rate_limit_scope: crate::providers::RateLimitScope::Account,
            },
        )
        .expect_err("duplicate must fail");
        match err {
            CoreError::Validation(msg) => assert_eq!(msg, "provider id already exists"),
            other => panic!("expected Validation, got {other:?}"),
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
                    rate_limit_scope: crate::providers::RateLimitScope::Account,
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
                rate_limit_scope: crate::providers::RateLimitScope::Account,
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
                rate_limit_scope: crate::providers::RateLimitScope::Account,
            },
        )
        .expect("create");

        // Partial: only name.
        update(
            &conn,
            &id,
            UpdateProviderParams {
                name: Some("Renamed"),
                ..Default::default()
            },
        )
        .expect("update name");
        let p = get(&conn, &id).expect("get").expect("present");
        assert_eq!(&*p.name, "Renamed");
        assert_eq!(&*p.base_url, "https://original.example", "untouched");
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
            UpdateProviderParams {
                base_url: Some("https://new.example"),
                extra_headers_json: Some(Some(r#"{"new":true}"#)),
                auto_activate_keyword: Some(Some("claude")),
                ..Default::default()
            },
        )
        .expect("update url+headers+keyword");
        let p = get(&conn, &id).expect("get").expect("present");
        assert_eq!(&*p.name, "Renamed", "untouched");
        assert_eq!(&*p.base_url, "https://new.example");
        assert_eq!(p.extra_headers_json.as_deref(), Some(r#"{"new":true}"#));
        assert_eq!(p.auto_activate_keyword.as_deref(), Some("claude"));

        // Clear the keyword: Some(None) sets NULL.
        update(
            &conn,
            &id,
            UpdateProviderParams {
                auto_activate_keyword: Some(None),
                ..Default::default()
            },
        )
        .expect("clear keyword");
        let p = get(&conn, &id).expect("get").expect("present");
        assert_eq!(p.auto_activate_keyword, None);

        // No-op update on an existing id: should not error and not touch row.
        update(&conn, &id, UpdateProviderParams::default()).expect("no-op");
        let p = get(&conn, &id).expect("get").expect("present");
        assert_eq!(&*p.base_url, "https://new.example");

        // Update on a missing id: ProviderNotFound.
        let missing = ProviderId::new("nope");
        let err = update(
            &conn,
            &missing,
            UpdateProviderParams {
                name: Some("X"),
                ..Default::default()
            },
        )
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
                rate_limit_scope: crate::providers::RateLimitScope::Account,
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
                rate_limit_scope: crate::providers::RateLimitScope::Account,
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
                    rate_limit_scope: crate::providers::RateLimitScope::Account,
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
                    rate_limit_scope: crate::providers::RateLimitScope::Account,
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
