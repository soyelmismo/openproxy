//! Auto-seed built-in providers on first run.
//!
//! The three providers in this list correspond 1:1 to the built-in adapters
//! registered in [`crate::adapters::builtin_adapters`]. Inserting a row for
//! each one on startup means the user can immediately see them in the
//! dashboard (and reference them by id in API calls) without having to
//! hand-create them.
//!
//! The seed is **idempotent**: each insert goes through
//! [`crate::providers::get`] first and is skipped when the row already
//! exists. This makes the function safe to call on every startup — it only
//! ever *adds* new rows, never updates or duplicates.
//!
//! [`crate::adapters::builtin_adapters`]: crate::adapters

use crate::capabilities;
use crate::error::Result;
use crate::ids::ProviderId;
use crate::providers::{self, AuthType, ProviderFormat};
use rusqlite::{params, Connection};

/// Row describing a built-in provider preset.
///
/// The fields mirror the columns of the `providers` table, in insertion
/// order. The list is computed once per call to [`seed_builtin_providers`]
/// rather than stored in a `static` so the compiler doesn't have to
/// parse a long tuple-typed array literal.
struct Builtin<'a> {
    id: &'a str,
    name: &'a str,
    base_url: &'a str,
    auth_type: &'a str,
    format: &'a str,
    extra_headers_json: Option<&'a str>,
    /// Optional substring matched against discovered `model_id`s to
    /// decide whether each new row is active after a refresh. `None`
    /// leaves all discovered models active.
    auto_activate_keyword: Option<&'a str>,
}

const BUILTINS: &[Builtin<'static>] = &[
    Builtin {
        id: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        auth_type: "bearer",
        format: "openai",
        extra_headers_json: Some(
            r#"{"HTTP-Referer":"https://openproxy.local","X-Title":"openproxy"}"#,
        ),
        auto_activate_keyword: None,
    },
    Builtin {
        id: "minimax",
        name: "MiniMax Coding",
        base_url: "https://api.minimax.io/anthropic/v1",
        auth_type: "bearer",
        format: "anthropic",
        extra_headers_json: Some(r#"{"Anthropic-Version":"2023-06-01"}"#),
        auto_activate_keyword: None,
    },
    Builtin {
        id: "opencode-zen",
        name: "OpenCode Zen",
        base_url: "https://opencode.ai/zen/v1",
        auth_type: "bearer",
        format: "mixed",
        extra_headers_json: None,
        auto_activate_keyword: None,
    },
    Builtin {
        id: "ollama-cloud",
        name: "Ollama Cloud",
        base_url: "https://ollama.com/v1",
        auth_type: "bearer",
        format: "openai",
        extra_headers_json: None,
        auto_activate_keyword: None,
    },
    Builtin {
        id: "nous-research",
        name: "Nous Research",
        base_url: "https://inference-api.nousresearch.com/v1",
        auth_type: "bearer",
        format: "openai",
        extra_headers_json: None,
        auto_activate_keyword: None,
    },
    Builtin {
        id: "nvidia-nim",
        name: "NVIDIA NIM",
        base_url: "https://integrate.api.nvidia.com/v1",
        auth_type: "bearer",
        format: "openai",
        extra_headers_json: None,
        auto_activate_keyword: None,
    },
    Builtin {
        id: "kilocode",
        name: "Kilocode",
        base_url: "https://api.kilo.ai/api/openrouter/v1",
        auth_type: "bearer",
        format: "openai",
        extra_headers_json: None,
        auto_activate_keyword: None,
    },
    Builtin {
        id: "gemini",
        name: "Gemini (Google AI Studio)",
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        auth_type: "goog-api-key",
        format: "gemini",
        extra_headers_json: None,
        auto_activate_keyword: None,
    },
    Builtin {
        id: "gemini-cli",
        name: "Gemini CLI (Google Cloud Code Assist)",
        base_url: "https://cloudcode-pa.googleapis.com",
        auth_type: "oauth",
        format: "gemini",
        extra_headers_json: None,
        auto_activate_keyword: None,
    },
    Builtin {
        id: "antigravity",
        name: "Antigravity (Cloud Code)",
        base_url: "https://daily-cloudcode-pa.googleapis.com",
        auth_type: "oauth",
        format: "gemini",
        extra_headers_json: None,
        auto_activate_keyword: None,
    },
    Builtin {
        id: "antigravity-cli",
        name: "Antigravity CLI (Cloud Code Assist)",
        base_url: "https://daily-cloudcode-pa.googleapis.com",
        auth_type: "oauth",
        format: "gemini",
        extra_headers_json: None,
        auto_activate_keyword: None,
    },
    Builtin {
        id: "kiro",
        name: "Kiro AI",
        base_url: "https://codewhisperer.us-east-1.amazonaws.com",
        auth_type: "oauth",
        format: "openai",
        extra_headers_json: None,
        auto_activate_keyword: None,
    },
    Builtin {
        id: "cloudflare-workers-ai",
        name: "CloudFlare Workers AI",
        base_url: "https://api.cloudflare.com/client/v4/accounts/${account_label}/ai/v1",
        auth_type: "bearer",
        format: "openai",
        extra_headers_json: None,
        auto_activate_keyword: None,
    },
];

/// Returns the IDs of the built-in providers.
///
/// These are the providers that ship with the binary and are seeded
/// into the `providers` table on first run. They are **inborrables**
/// (cannot be deleted) because removing the row would leave dangling
/// references in [`crate::adapters::builtin_adapters`] — the adapter
/// registry still holds a clone of the id and would panic on a
/// routing attempt for a provider that no longer exists. The
/// `delete_provider` admin path therefore rejects built-in ids with
/// a 400 (Validation) instructing the operator to use the
/// "Deactivate" endpoint instead, which is the reversible
/// alternative: it flips the `active` bit to `0` and combo-target
/// lookups skip the provider, but the row (and its accounts/models)
/// stays in the DB so it can be reactivated later.
///
/// Custom (operator-created) providers are not in this list and can
/// be deleted normally.
pub fn builtin_provider_ids() -> &'static [&'static str] {
    &["openrouter", "minimax", "opencode-zen", "ollama-cloud", "nous-research", "nvidia-nim", "kilocode", "gemini", "antigravity", "antigravity-cli", "kiro", "cloudflare-workers-ai"]
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
    builtin_provider_ids().contains(&id)
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
pub fn seed_builtin_providers(conn: &Connection) -> Result<usize> {
    let mut seeded = 0;

    for b in BUILTINS {
        let id_typed = ProviderId::new(b.id);

        // Skip if the row already exists. `providers::get` returns
        // `Ok(None)` for a missing id, so the `?` only fires on a real
        // database error.
        if providers::get(conn, &id_typed)?.is_some() {
            continue;
        }

        // The strings in `BUILTINS` are static and known-valid; a
        // `Validation` here would be a programmer error in this file,
        // not a user mistake, so we surface it unwrapped (the caller's
        // `?` will turn it into a 500 with a clear message).
        let auth = AuthType::parse(b.auth_type).expect("builtin auth_type is valid");
        let fmt = ProviderFormat::parse(b.format).expect("builtin format is valid");

        providers::create(
            conn,
            providers::NewProvider {
                id: &id_typed,
                name: b.name,
                base_url: b.base_url,
                auth_type: auth,
                format: fmt,
                extra_headers_json: b.extra_headers_json,
                auto_activate_keyword: b.auto_activate_keyword,
            },
        )?;
        seeded += 1;
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
        },
    )?;
    Ok(true)
}

/// Backfill the new model-metadata columns for rows that were inserted
/// before migration 000014 ran (so `context_length` and friends are
/// NULL on them). Idempotent: uses `COALESCE` on both sides so calling
/// it a second time is a no-op for any row that has already been
/// filled in. Custom rows are included — the operator can always
/// override the heuristic from the admin UI by editing the row.
///
/// The heuristic used here is the same one the `GET /v1/models` handler
/// applies as a runtime fallback, so a row this function has touched
/// is indistinguishable at the wire from a row that was filled in by
/// the fallback path. The difference is persistence: backfilled rows
/// don't have to re-derive the values on every `/v1/models` call.
///
/// Returns the number of rows whose column-set actually changed (rows
/// that were already complete are counted as `0`).
pub fn backfill_model_metadata(conn: &Connection) -> Result<u64> {
    let models = crate::models::list_all(conn)?;
    let mut updated = 0u64;

    for m in models {
        // Skip rows that already have a context_length AND capabilities
        // — the cheapest proxy for "fully backfilled" we have, since
        // every heuristic always sets `context_length` to a known
        // value or leaves it `None`. The same call is cheap to make
        // for every row, so we keep the predicate simple and don't
        // try to be clever.
        if m.context_length.is_some() && m.capabilities_json.is_some() {
            continue;
        }

        let model_id = m.model_id.as_str();
        let context_length = capabilities::infer_context_length(model_id);
        let max_output_tokens = capabilities::infer_max_output_tokens(model_id);
        let caps = capabilities::infer_capabilities(model_id);
        let caps_json = caps.to_json();
        let input_mods = capabilities::infer_input_modalities_json(model_id);
        let output_mods = capabilities::infer_output_modalities_json(model_id);
        // model_type: COALESCE(NULL, heuristic) — never clobber an
        // operator-set value.
        let model_type = if m.model_type.is_empty() {
            capabilities::infer_model_type(model_id).to_string()
        } else {
            m.model_type.clone()
        };
        let family = capabilities::infer_family(model_id);

        let changed = conn
            .execute(
                "UPDATE models SET
                    context_length         = COALESCE(?1, context_length),
                    max_output_tokens      = COALESCE(?2, max_output_tokens),
                    capabilities_json      = COALESCE(?3, capabilities_json),
                    model_type             = COALESCE(?4, model_type),
                    input_modalities_json  = COALESCE(?5, input_modalities_json),
                    output_modalities_json = COALESCE(?6, output_modalities_json),
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
                message: format!("backfill_model_metadata for {}: {}", model_id, e),
                source: Some(Box::new(e)),
            })?;

        updated += changed as u64;
    }

    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::conn::DbPool;
    use crate::db::migrations;
    use std::path::PathBuf;

    /// Build an in-process pool: temp dir on disk, migrations applied.
    fn fresh_pool() -> (DbPool, PathBuf) {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("openproxy-seed-test-{}-{}-{}", pid, nanos, n));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("seed.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    #[test]
    fn seeds_all_on_empty_db() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let n = seed_builtin_providers(&conn).expect("seed");
        assert_eq!(n, 13, "first call inserts all thirteen");

        // All thirteen are present and reachable by id.
        for id in ["openrouter", "minimax", "opencode-zen", "ollama-cloud", "nous-research", "nvidia-nim", "kilocode", "gemini", "gemini-cli", "antigravity", "antigravity-cli", "kiro", "cloudflare-workers-ai"] {
            let p = providers::get(&conn, &ProviderId::new(id))
                .expect("get")
                .unwrap_or_else(|| panic!("{} not seeded", id));
            assert_eq!(p.id.as_str(), id);
        }
    }

    #[test]
    fn second_call_is_a_no_op() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let first = seed_builtin_providers(&conn).expect("first");
        assert_eq!(first, 13);

        // Idempotent: running again must not insert more rows.
        let second = seed_builtin_providers(&conn).expect("second");
        assert_eq!(second, 0, "no new rows on second call");

        let count = providers::list(&conn).expect("list").len();
        assert_eq!(count, 13, "still exactly thirteen rows");
    }

    #[test]
    fn partial_state_only_seeds_missing() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        // Pre-seed one of the ten manually.
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
            },
        )
        .expect("pre-seed");

        let n = seed_builtin_providers(&conn).expect("seed");
        assert_eq!(n, 12, "only the twelve missing ones");

        // The pre-seeded row's name was *not* overwritten.
        let p = providers::get(&conn, &ProviderId::new("openrouter"))
            .expect("get")
            .unwrap();
        assert_eq!(
            p.name, "Custom name override",
            "existing row untouched"
        );
    }

    #[test]
    fn auth_and_format_match_table() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_builtin_providers(&conn).expect("seed");

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

        let antigravity_cli = providers::get(&conn, &ProviderId::new("antigravity-cli"))
            .expect("get")
            .unwrap();
        assert_eq!(antigravity_cli.auth_type, AuthType::OAuth);
        assert_eq!(antigravity_cli.format, ProviderFormat::Gemini);

        let kiro = providers::get(&conn, &ProviderId::new("kiro"))
            .expect("get")
            .unwrap();
        assert_eq!(kiro.auth_type, AuthType::OAuth);
        assert_eq!(kiro.format, ProviderFormat::Openai);
    }

    #[test]
    fn builtin_provider_ids_lists_twelve() {
        // The list is the source of truth for "is this provider
        // protected from delete?" — guard it with a test so a future
        // addition to `BUILTINS` (e.g. a new seeded provider) gets
        // remembered here.
        let ids = builtin_provider_ids();
        assert_eq!(ids.len(), 12);
        assert!(ids.contains(&"openrouter"));
        assert!(ids.contains(&"minimax"));
        assert!(ids.contains(&"opencode-zen"));
        assert!(ids.contains(&"ollama-cloud"));
        assert!(ids.contains(&"nous-research"));
        assert!(ids.contains(&"nvidia-nim"));
        assert!(ids.contains(&"kilocode"));
        assert!(ids.contains(&"gemini"));
        assert!(ids.contains(&"antigravity"));
        assert!(ids.contains(&"antigravity-cli"));
        assert!(ids.contains(&"kiro"));
        assert!(ids.contains(&"cloudflare-workers-ai"));
    }

    #[test]
    fn is_builtin_matches_list() {
        for id in builtin_provider_ids() {
            assert!(is_builtin(id), "{} should be marked built-in", id);
        }
        // A handful of negative cases: built-in predicate must not
        // match custom ids (the same string used by `create_provider`)
        // and must not match a partial prefix (e.g. "openrouter-x").
        for not_builtin in ["my-custom", "OpenRouter", "OPENROUTER", "openrouter-x", ""] {
            assert!(
                !is_builtin(not_builtin),
                "{} should NOT be marked built-in",
                not_builtin
            );
        }
    }
}
