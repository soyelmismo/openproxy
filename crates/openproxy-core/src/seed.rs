//! Auto-seed built-in providers on first run.
//!
//! The providers in this list correspond 1:1 to the built-in adapters
//! registered in [`openproxy_adapters::adapters::builtin_adapters`]. Inserting a row for
//! each one on startup means the user can immediately see them in the
//! dashboard (and reference them by id in API calls) without having to
//! hand-create them.
//!
//! The seed is **idempotent**: each insert goes through
//! [`crate::providers::get`] first and is skipped when the row already
//! exists. This makes the function safe to call on every startup — it only
//! ever *adds* new rows, never updates or duplicates.
//!
//! [`openproxy_adapters::adapters::builtin_adapters`]: crate::adapters

use crate::capabilities;
use crate::error::Result;
use crate::ids::ProviderId;
use crate::providers::{self, AuthType, ProviderFormat};
use rusqlite::{Connection, params};

pub fn builtin_provider_ids() -> Vec<String> {
    openproxy_adapters::adapters::builtin_adapters()
        .iter()
        .map(|a| a.config().id.0.clone())
        .collect()
}

/// The id of the synthetic "combo" provider row used as a placeholder
/// `provider_id` on combo-in-combo (sub-combo) targets. The row has
/// `active = 1` and `format = 'openai'` so the `combo_targets` join
/// `p.active = 1` lets sub-combo rows through, but it has no adapter
/// registered against it — the pipeline never tries to dispatch a
/// chat call against this provider. Routing for a sub-combo target
/// is handled by flattening the sub-combo's children into the parent
/// combo's target list, not by hitting this id.
pub const VIRTUAL_COMBO_PROVIDER_ID: &str = "combo";

/// Convenience predicate: is `id` one of the built-in seeded
/// providers? Used by the admin handlers to reject delete attempts
/// on built-ins (see [`builtin_provider_ids`] for the rationale).
pub fn is_builtin(id: &str) -> bool {
    builtin_provider_ids().iter().any(|s| s == id)
}

/// Insert any missing built-in providers. Returns the number of rows
/// newly created; rows that already existed are silently skipped.
///
/// This is the entry point the server's `AppState::new` calls right
/// after the migrations have run.
///
/// # Errors
///
/// Propagates any [`CoreError::Validation`] (bad enum literal) or
/// [`CoreError::Database`] (insert failure) from the underlying
/// [`providers::create`]. The three enum strings in the constant table
/// above are all valid, so a `Validation` here would indicate
/// programmer error; a `Database` error would indicate a real I/O
/// problem the caller should surface.
///
/// [`CoreError::Validation`]: crate::error::CoreError::Validation
/// [`CoreError::Database`]: crate::error::CoreError::Database
fn serialize_extra_headers(headers: &[(String, String)]) -> Option<String> {
    if headers.is_empty() {
        return None;
    }
    let map: std::collections::HashMap<&str, &str> = headers
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    serde_json::to_string(&map).ok()
}

fn seed_single_adapter(
    conn: &Connection,
    adapter: &openproxy_adapters::adapters::ProviderAdapterEnum,
) -> Result<bool> {
    let conf = adapter.config();
    if providers::get(conn, &conf.id)?.is_some() {
        return Ok(false);
    }

    let auth = AuthType::parse(conf.auth_type.as_str()).expect("builtin auth_type is valid");
    let fmt = ProviderFormat::parse(conf.format.as_str()).expect("builtin format is valid");
    let extra_headers = serialize_extra_headers(&conf.extra_headers);
    let rate_limit_scope = providers::RateLimitScope::parse(&conf.rate_limit_scope)
        .expect("builtin scope is valid");

    providers::create(
        conn,
        providers::NewProvider {
            id: &conf.id,
            name: &conf.name,
            base_url: &conf.base_url,
            auth_type: auth,
            format: fmt,
            extra_headers_json: extra_headers.as_deref(),
            auto_activate_keyword: None,
            rate_limit_scope,
        },
    )?;
    Ok(true)
}

/// Seed the built-in providers (OpenAI, Anthropic, Gemini, Groq, OpenRouter, etc.).
pub fn seed_builtin_providers(conn: &Connection) -> Result<usize> {
    let mut seeded = 0;
    for adapter in openproxy_adapters::adapters::builtin_adapters() {
        if seed_single_adapter(conn, &adapter)? {
            seeded += 1;
        }
    }
    Ok(seeded)
}

/// Insert the virtual "combo" provider row used as a placeholder
/// `provider_id` on sub-combo targets. Idempotent: skipped if the
/// row already exists. This is intentionally a separate call from
/// [`seed_builtin_providers`] because the "combo" id is *not* a
/// built-in in the sense that admin deletion protection covers
/// (there is no adapter registered against it) — it lives in the
/// `providers` table only to satisfy the `combo_targets.provider_id`
/// NOT-NULL + FK constraint and the `list_targets` `p.active = 1`
/// join filter.
///
/// Returns `true` if a new row was inserted, `false` if it was
/// already there.
pub fn seed_virtual_combo_provider(conn: &Connection) -> Result<bool> {
    let id_typed = ProviderId::new(VIRTUAL_COMBO_PROVIDER_ID);
    if providers::get(conn, &id_typed)?.is_some() {
        return Ok(false);
    }
    providers::create(
        conn,
        providers::NewProvider {
            id: &id_typed,
            name: "Virtual provider for sub-combo targets",
            base_url: "https://invalid.local/combo",
            auth_type: AuthType::Bearer,
            format: ProviderFormat::Openai,
            extra_headers_json: None,
            auto_activate_keyword: None,
            rate_limit_scope: providers::RateLimitScope::Account,
        },
    )?;
    Ok(true)
}

fn needs_model_type_fix(current_type: &str, inferred_type: &str) -> bool {
    current_type.is_empty() || (current_type == "chat" && inferred_type != "chat")
}

fn update_single_model_metadata(conn: &Connection, m: &crate::models::Model) -> Result<usize> {
    let model_id = m.model_id.as_str();
    let inferred_model_type = capabilities::infer_model_type(model_id);
    let model_type_needs_fix = needs_model_type_fix(&m.model_type, inferred_model_type);

    if m.context_length.is_some() && m.capabilities_json.is_some() && !model_type_needs_fix {
        return Ok(0);
    }

    let context_length = capabilities::infer_context_length(model_id);
    let max_output_tokens = capabilities::infer_max_output_tokens(model_id);
    let caps = capabilities::infer_capabilities(model_id);
    let caps_json = caps.to_json();
    let input_mods = capabilities::infer_input_modalities_json(model_id);
    let output_mods = capabilities::infer_output_modalities_json(model_id);

    let model_type = if model_type_needs_fix {
        inferred_model_type
    } else {
        m.model_type.as_str()
    };
    let family = capabilities::infer_family(model_id);

    conn.execute(
        "UPDATE models SET
            context_length         = COALESCE(?1, context_length),
            max_output_tokens      = COALESCE(?2, max_output_tokens),
            capabilities_json      = COALESCE(?3, capabilities_json),
            model_type             = ?4,
            input_modalities_json  = COALESCE(?5, input_modalities_json),
            output_modalities_json = ?6,
            family                 = COALESCE(?7, family)
         WHERE id = ?8",
        params![
            context_length,
            max_output_tokens,
            caps_json,
            model_type,
            input_mods,
            output_mods,
            family,
            m.row_id.0,
        ],
    )
    .map_err(|e| crate::error::CoreError::Database {
        message: format!("backfill_model_metadata for {model_id}: {e}"),
        source: Some(std::sync::Arc::new(e)),
    })
}

/// Backfill the new model-metadata columns for rows that were inserted
/// before migration 000014 ran.
pub fn backfill_model_metadata(conn: &Connection) -> Result<u64> {
    let models = crate::models::list_all(conn)?;
    let mut updated = 0u64;

    for m in &models {
        updated += update_single_model_metadata(conn, m)? as u64;
    }

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_db::conn::DbPool;

    use std::path::PathBuf;

    /// Build an in-process pool: temp dir on disk, migrations applied.
    fn fresh_pool() -> (DbPool, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("openproxy-seed-test-{pid}-{nanos}-{n}"));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("seed.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            openproxy_db::migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    #[test]
    fn seeds_all_on_empty_db() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let n = seed_builtin_providers(&conn).expect("seed");
        assert_eq!(n, 18, "first call inserts all eighteen");

        // All eighteen are present and reachable by id.
        for id in [
            "atomesus",
            "openrouter",
            "minimax",
            "opencode-zen",
            "opencode-go",
            "ollama-cloud",
            "nous-research",
            "nvidia-nim",
            "kilocode",
            "gemini",
            "horde",
            "antigravity",
            "codex",
            "kiro",
            "cloudflare-workers-ai",
            "cline",
            "vercel-gateway",
            "fx",
        ] {
            let p = providers::get(&conn, &ProviderId::new(id))
                .expect("get")
                .unwrap_or_else(|| panic!("{id} not seeded"));
            assert_eq!(p.id.as_str(), id);
        }
    }

    #[test]
    fn second_call_is_a_no_op() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let first = seed_builtin_providers(&conn).expect("first");
        assert_eq!(first, 18);

        // Idempotent: running again must not insert more rows.
        let second = seed_builtin_providers(&conn).expect("second");
        assert_eq!(second, 0, "no new rows on second call");

        let count = providers::list(&conn).expect("list").len();
        assert_eq!(count, 18, "still exactly eighteen rows");
    }

    #[test]
    fn partial_state_only_seeds_missing() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        // Pre-seed one of the builtins manually.
        providers::create(
            &conn,
            providers::NewProvider {
                id: &ProviderId::new("openrouter"),
                name: "Custom name override",
                base_url: "https://example.test",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: crate::providers::RateLimitScope::Account,
            },
        )
        .expect("pre-seed");

        let n = seed_builtin_providers(&conn).expect("seed");
        assert_eq!(n, 17, "only the seventeen missing ones");

        // The pre-seeded row's name was *not* overwritten.
        let p = providers::get(&conn, &ProviderId::new("openrouter"))
            .expect("get")
            .unwrap();
        assert_eq!(p.name, "Custom name override", "existing row untouched");
    }

    #[test]
    fn auth_and_format_match_table() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_builtin_providers(&conn).expect("seed");

        let atomesus = providers::get(&conn, &ProviderId::new("atomesus"))
            .expect("get")
            .unwrap();
        assert_eq!(atomesus.auth_type, AuthType::Bearer);
        assert_eq!(atomesus.format, ProviderFormat::Atomesus);

        let openrouter = providers::get(&conn, &ProviderId::new("openrouter"))
            .expect("get")
            .unwrap();
        assert_eq!(openrouter.auth_type, AuthType::Bearer);
        assert_eq!(openrouter.format, ProviderFormat::Openai);

        let minimax = providers::get(&conn, &ProviderId::new("minimax"))
            .expect("get")
            .unwrap();
        assert_eq!(minimax.auth_type, AuthType::Bearer);
        assert_eq!(minimax.format, ProviderFormat::Anthropic);

        let zen = providers::get(&conn, &ProviderId::new("opencode-zen"))
            .expect("get")
            .unwrap();
        assert_eq!(zen.auth_type, AuthType::Bearer);
        assert_eq!(zen.format, ProviderFormat::Mixed);

        let vercel = providers::get(&conn, &ProviderId::new("vercel-gateway"))
            .expect("get")
            .unwrap();
        assert_eq!(vercel.auth_type, AuthType::Bearer);
        assert_eq!(vercel.format, ProviderFormat::Openai);
        assert_eq!(vercel.name, "Vercel Gateway");

        let ollama = providers::get(&conn, &ProviderId::new("ollama-cloud"))
            .expect("get")
            .unwrap();
        assert_eq!(ollama.auth_type, AuthType::Bearer);
        assert_eq!(ollama.format, ProviderFormat::Openai);

        let gemini = providers::get(&conn, &ProviderId::new("gemini"))
            .expect("get")
            .unwrap();
        assert_eq!(gemini.auth_type, AuthType::GoogApiKey);
        assert_eq!(gemini.format, ProviderFormat::Gemini);

        let antigravity = providers::get(&conn, &ProviderId::new("antigravity"))
            .expect("get")
            .unwrap();
        assert_eq!(antigravity.auth_type, AuthType::OAuth);
        assert_eq!(antigravity.format, ProviderFormat::Gemini);

        let codex = providers::get(&conn, &ProviderId::new("codex"))
            .expect("get")
            .unwrap();
        assert_eq!(codex.auth_type, AuthType::OAuth);
        assert_eq!(codex.format, ProviderFormat::Responses);
        let kiro = providers::get(&conn, &ProviderId::new("kiro"))
            .expect("get")
            .unwrap();
        assert_eq!(kiro.auth_type, AuthType::OAuth);
        assert_eq!(kiro.format, ProviderFormat::Openai);
    }

    #[test]
    fn builtin_provider_ids_lists_all() {
        let ids = builtin_provider_ids();
        assert_eq!(ids.len(), 18);
        assert!(ids.iter().any(|s| s == "atomesus"));
        assert!(ids.iter().any(|s| s == "openrouter"));
        assert!(ids.iter().any(|s| s == "minimax"));
        assert!(ids.iter().any(|s| s == "opencode-zen"));
        assert!(ids.iter().any(|s| s == "ollama-cloud"));
        assert!(ids.iter().any(|s| s == "nous-research"));
        assert!(ids.iter().any(|s| s == "nvidia-nim"));
        assert!(ids.iter().any(|s| s == "kilocode"));
        assert!(ids.iter().any(|s| s == "gemini"));
        assert!(ids.iter().any(|s| s == "horde"));
        assert!(ids.iter().any(|s| s == "antigravity"));
        assert!(ids.iter().any(|s| s == "codex"));
        assert!(ids.iter().any(|s| s == "kiro"));
        assert!(ids.iter().any(|s| s == "cloudflare-workers-ai"));
        assert!(ids.iter().any(|s| s == "cline"));
        assert!(ids.iter().any(|s| s == "vercel-gateway"));
        assert!(ids.iter().any(|s| s == "fx"));
    }

    #[test]
    fn is_builtin_matches_list() {
        for id in builtin_provider_ids() {
            assert!(is_builtin(&id), "{id} should be marked built-in");
        }
        // A handful of negative cases: built-in predicate must not
        // match custom ids (the same string used by `create_provider`)
        // and must not match a partial prefix (e.g. "openrouter-x").
        for not_builtin in ["my-custom", "OpenRouter", "OPENROUTER", "openrouter-x", ""] {
            assert!(
                !is_builtin(not_builtin),
                "{not_builtin} should NOT be marked built-in"
            );
        }
    }
}
