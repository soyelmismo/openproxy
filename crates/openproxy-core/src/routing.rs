//! Routing layer: model-first resolution.
//!
//! The chat endpoint looks at the `model` field of the incoming request
//! and decides which upstream to dispatch to. The decision matrix is
//! (in order):
//!
//! 1. **Direct model**: a row in the `models` table whose `model_id`
//!    matches the request and whose `provider_id` is active. Dispatched
//!    through a synthetic single-target combo so the existing pipeline
//!    (race, retries, circuit breaker, usage logging) is reused as-is.
//!
//! 2. **Combo alias**: a row in the `combos` table whose `name` matches
//!    the request (with or without the `combo:` prefix). Dispatched
//!    through the normal pipeline.
//!
//! 3. **Not found**: return a 404 to the client.
//!
//! This module is intentionally a pure function (`resolve`) over a
//! `&Connection`. Side effects (insert into the DB, mutate state) are
//! pushed to the chat handler, which is the only caller.

use crate::combos::{self, Combo, ComboTarget, Strategy};
use crate::error::Result;
use crate::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId};
use crate::models::{self, Model};
use rusqlite::{Connection, OptionalExtension};

/// Sentinel combo id used for synthetic, in-memory combos. The id is
/// negative so it can never collide with a real `combos.id` (which is
/// always a positive SQLite rowid) and so the usage row's `combo_id`
/// column carries a stable marker of "this came from a direct model
/// dispatch" that an analyst can grep for.
pub const SYNTHETIC_COMBO_ID: i64 = -1;

/// Display name used for synthetic combos. Lives in usage rows'
/// `error_msg` / log fields; never serialised to the public client.
pub const SYNTHETIC_COMBO_NAME: &str = "__direct__";

/// The result of resolving a model string into a routing plan.
#[derive(Debug, Clone)]
pub enum RoutingPlan {
    /// Direct hit on a model. The chat handler must wrap the target in
    /// a synthetic `Combo` and dispatch through the pipeline.
    Direct {
        provider_id: ProviderId,
        account_id: Option<AccountId>,
        model_row_id: ModelRowId,
        /// Upstream model id (the `models.model_id` value). The
        /// `upstream_model_id` column in `usage` carries this string
        /// for analytics.
        model_id: String,
        rate_limit_scope: crate::providers::RateLimitScope,
    },
    /// A combo. The chat handler dispatches through the normal
    /// pipeline path keyed on `combo_id`.
    Combo {
        combo_id: ComboId,
        combo_name: String,
        strategy: Strategy,
        race_size: u8,
        targets: Vec<ComboTarget>,
    },
    /// No model and no combo match. The handler returns 404.
    NotFound {
        model: String,
        /// Optional hint for the operator/client. e.g. when the
        /// client sent `combo:Nerd` (uppercase) but the combo is
        /// stored as `nerd`, the hint is `combo:nerd`.
        hint: Option<String>,
    },
}

/// Resolve a model string to a routing plan.
///
/// `model_str` is the raw `model` field from the chat request. The
/// resolver:
///
/// 1. Strips the proxy-level `<provider>/` prefix if one matches a
///    known provider id.
/// 2. Tries to match the result as a row in `models` (active + not
///    expired).
/// 3. Tries to match the result as a combo (after stripping an
///    optional `combo:` prefix).
/// 4. Returns `NotFound` otherwise.
///
/// The match is case-sensitive. A `model` value of `ComBo:nerd` will
/// not resolve to combo `nerd` — the same convention the rest of the
/// code uses for stored names.
pub fn resolve(conn: &Connection, model_str: &str) -> Result<RoutingPlan> {
    // 1. Strip proxy-level provider prefix if present.
    let (stripped, provider_prefix) = strip_proxy_prefix(conn, model_str);

    // 2. Try direct model resolution.
    if let Some(plan) = try_resolve_direct_model(conn, stripped, provider_prefix)? {
        return Ok(plan);
    }

    // 3. Try combo resolution: strip "combo:" if present, look up by
    //    name.
    let combo_name = stripped.strip_prefix("combo:").unwrap_or(stripped);
    if let Some(combo) = combos::get_combo_by_name(conn, combo_name)? {
        let targets = combos::list_targets(conn, combo.id)?;
        return Ok(RoutingPlan::Combo {
            combo_id: combo.id,
            combo_name: combo.name,
            strategy: combo.strategy,
            race_size: combo.race_size,
            targets,
        });
    }

    // 4. Not found.
    let hint = if combo_name != stripped {
        // The original input had a `combo:` prefix; include the
        // normalised name in the hint.
        Some(format!("combo:{}", combo_name))
    } else {
        None
    };
    Ok(RoutingPlan::NotFound {
        model: model_str.to_string(),
        hint,
    })
}

fn try_resolve_direct_model(
    conn: &Connection,
    model_id: &str,
    provider_prefix: Option<&str>,
) -> Result<Option<RoutingPlan>> {
    // If a provider prefix was given, look for the model specifically
    // under that provider. Otherwise, find any active model by name.
    let model: Option<Model> = if let Some(prefix) = provider_prefix {
        models::find_active_by_provider_and_name(conn, &ProviderId::new(prefix), model_id)?
    } else {
        models::find_active_by_name(conn, model_id)?
    };
    let Some(model) = model else {
        return Ok(None);
    };

    // The provider must be active; a deactivated provider means the
    // model is not routable today. We surface this as "no match"
    // rather than a 5xx — the operator can re-enable the provider.
    let (active, rate_limit_scope) = provider_active_and_scope(conn, &model.provider_id)?;
    if !active {
        return Ok(None);
    }

    // `account_id = None` on the synthetic target tells the pipeline
    // to fall back to the auto-rotation path, which is the documented
    // behaviour for a provider with no pinned account.
    let account_id = None;

    Ok(Some(RoutingPlan::Direct {
        provider_id: model.provider_id.clone(),
        account_id,
        model_row_id: model.row_id,
        model_id: model.model_id.as_str().to_string(),
        rate_limit_scope,
    }))
}

/// Cheap "is this provider active?" probe plus its rate limit scope.
/// Returns `Ok((false, RateLimitScope::Account))` when
/// the row is missing so a deleted provider (e.g. after a race with
/// the admin UI) is treated as inactive.
fn provider_active_and_scope(
    conn: &Connection,
    provider_id: &ProviderId,
) -> Result<(bool, crate::providers::RateLimitScope)> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT active, rate_limit_scope FROM providers WHERE id = ?1",
            rusqlite::params![provider_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(crate::error::map_db_error_ctx(format!(
            "provider_active_and_scope({})",
            provider_id
        )))?;
    match row {
        Some((active, scope_str)) => {
            let scope = crate::providers::RateLimitScope::parse(&scope_str).unwrap_or_default();
            Ok((active != 0, scope))
        }
        None => Ok((false, crate::providers::RateLimitScope::default())),
    }
}

/// Strip the proxy-level `<provider>/` prefix from `model_str` if the
/// segment before the first `/` matches a known provider id.
///
/// Why the provider lookup is necessary: upstream model ids can
/// themselves contain `/` (e.g. `openai/gpt-4o` is a valid model id
/// from the upstream's perspective, and is what the OpenRouter
/// adapter surfaces for `openai/gpt-4o`). Stripping on the *first*
/// `/` would mangle that into a different (and likely missing)
/// upstream id.
///
/// The lookup is intentionally cheap: a single indexed `SELECT id`
/// against the small `providers` table. We also short-circuit when
/// the input does not contain a `/` (it cannot be a prefixed id).
///
/// The `combo:` prefix is preserved verbatim; combo names are not
/// provider-prefixed.
fn strip_proxy_prefix<'a>(conn: &Connection, model_str: &'a str) -> (&'a str, Option<&'a str>) {
    if model_str.starts_with("combo:") {
        return (model_str, None);
    }
    let Some(slash_idx) = model_str.find('/') else {
        return (model_str, None);
    };
    let prefix = &model_str[..slash_idx];
    if prefix.is_empty() {
        return (model_str, None);
    }
    let candidate = ProviderId::new(prefix);
    let exists: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM providers WHERE id = ?1)",
            rusqlite::params![candidate.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map(|v| v.unwrap_or(0) != 0)
        .unwrap_or(false);
    if exists {
        (&model_str[slash_idx + 1..], Some(prefix))
    } else {
        (model_str, None)
    }
}

/// Build a synthetic in-memory `Combo` plus its single `ComboTarget`
/// for a direct-model dispatch.
///
/// The combo has `combo.id = SYNTHETIC_COMBO_ID` (a negative sentinel
/// that can never collide with a real `combos.id`) and
/// `combo.name = SYNTHETIC_COMBO_NAME`. The `Combo` is shaped so the
/// pipeline's `load_combo` accepts it; the targets are returned
/// alongside in a parallel vec that the chat handler threads into the
/// `PipelineRequest::targets_override` slot.
pub fn build_synthetic_combo(
    provider_id: ProviderId,
    account_id: Option<AccountId>,
    model_row_id: ModelRowId,
    rate_limit_scope: crate::providers::RateLimitScope,
) -> (Combo, Vec<ComboTarget>) {
    // The synthetic target uses an all-zero id so the row never
    // collides with a real `combo_targets.id`. The pipeline
    // serialises `target.id` into the `usage` table's
    // `combo_target_id` column; the FK is not enforced there
    // (combo_target_id is a free-form INTEGER), so a sentinel is
    // safe.
    let target = ComboTarget {
        id: ComboTargetId(0),
        combo_id: ComboId(SYNTHETIC_COMBO_ID),
        provider_id,
        account_id,
        model_row_id: Some(model_row_id),
        sub_combo_id: None,
        priority_order: 0,
        weight: 1,
        rate_limit_scope,
    };
    let combo = Combo {
        id: ComboId(SYNTHETIC_COMBO_ID),
        name: SYNTHETIC_COMBO_NAME.to_string(),
        strategy: Strategy::Priority,
        race_size: 1,
        created_at: String::new(),
        context_window: None,
        // Synthetic combos use the legacy defaults (strict priority,
        // flat cooldown) so a direct-model dispatch behaves exactly
        // like a single-target user combo.
        priority_mode: crate::combos::PriorityMode::Strict,
        cooldown_mode: crate::combos::CooldownMode::Flat,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
    };
    (combo, vec![target])
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::conn::DbPool;
    use crate::db::migrations;
    use crate::providers::{self, AuthType, ProviderFormat};
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    /// Fresh on-disk pool with migrations applied. Unique tempdir per
    /// test to avoid `WAL`-file collisions in parallel runs.
    fn fresh_pool() -> (DbPool, PathBuf) {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pid = std::process::id();
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir =
            std::env::temp_dir().join(format!("openproxy-routing-test-{}-{}-{}", pid, nanos, n));
        std::fs::create_dir_all(&dir).expect("mkdir tempdir");
        let path = dir.join("routing.db");
        let pool = DbPool::open(&path).expect("open pool");
        {
            let mut w = pool.writer();
            migrations::run(&mut w).expect("migrations");
        }
        (pool, path)
    }

    fn seed_provider(conn: &Connection, id: &str) {
        providers::create(
            conn,
            providers::NewProvider {
                id: &ProviderId::new(id),
                name: id,
                base_url: "https://example.com",
                auth_type: AuthType::Bearer,
                format: ProviderFormat::Openai,
                extra_headers_json: None,
                auto_activate_keyword: None,
                rate_limit_scope: crate::providers::RateLimitScope::Account,
            },
        )
        .expect("seed provider");
    }

    /// Insert a model row directly (the helper in the combos test
    /// module is private). Returns the row id.
    fn seed_model(conn: &Connection, provider: &str, model_id: &str) -> ModelRowId {
        conn.execute(
            "INSERT INTO models(provider_id, model_id, target_format) \
             VALUES (?1, ?2, 'openai')",
            rusqlite::params![provider, model_id],
        )
        .expect("seed model");
        let id: i64 = conn
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("last_insert_rowid");
        ModelRowId(id)
    }

    /// Insert a healthy account directly so the resolver can pick one.
    fn seed_healthy_account(conn: &Connection, provider_id: &str) -> AccountId {
        conn.execute(
            "INSERT INTO accounts(provider_id, api_key_encrypted, health_status) \
             VALUES (?1, X'00', 'healthy')",
            rusqlite::params![provider_id],
        )
        .expect("seed account");
        let id: i64 = conn
            .query_row("SELECT last_insert_rowid()", [], |r| r.get(0))
            .expect("last_insert_rowid");
        AccountId(id)
    }

    // -----------------------------------------------------------------------
    // resolve → Direct
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_direct_model_returns_plan() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_healthy_account(&conn, "openrouter");
        let model_row = seed_model(&conn, "openrouter", "anthropic/claude-3.5");

        let plan = resolve(&conn, "anthropic/claude-3.5").expect("resolve");
        match plan {
            RoutingPlan::Direct {
                provider_id,
                account_id,
                model_row_id,
                model_id,
                ..
            } => {
                assert_eq!(provider_id, ProviderId::new("openrouter"));
                assert_eq!(model_row_id, model_row);
                assert_eq!(model_id, "anthropic/claude-3.5");
                assert!(account_id.is_none(), "resolver always uses auto-rotation");
            }
            other => panic!("expected Direct, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // resolve → Combo (with and without the `combo:` prefix)
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_combo_with_prefix() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let combo_id = combos::create_combo(&conn, "smart", Strategy::Priority, 1).expect("create");

        let plan = resolve(&conn, "combo:smart").expect("resolve");
        match plan {
            RoutingPlan::Combo {
                combo_id: got_id,
                combo_name,
                ..
            } => {
                assert_eq!(got_id, combo_id);
                assert_eq!(combo_name, "smart");
            }
            other => panic!("expected Combo, got {:?}", other),
        }
    }

    #[test]
    fn resolve_combo_without_prefix() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let combo_id = combos::create_combo(&conn, "smart", Strategy::Priority, 1).expect("create");

        let plan = resolve(&conn, "smart").expect("resolve");
        match plan {
            RoutingPlan::Combo {
                combo_id: got_id,
                combo_name,
                ..
            } => {
                assert_eq!(got_id, combo_id);
                assert_eq!(combo_name, "smart");
            }
            other => panic!("expected Combo, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // resolve → NotFound
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_not_found() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_model(&conn, "openrouter", "real-model");
        // The probe name is neither a model nor a combo.
        let plan = resolve(&conn, "ghost").expect("resolve");
        match plan {
            RoutingPlan::NotFound { model, hint } => {
                assert_eq!(model, "ghost");
                assert!(hint.is_none(), "no combo: prefix → no hint");
            }
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // resolve → NotFound when the provider is inactive
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_inactive_provider_returns_not_found() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_healthy_account(&conn, "openrouter");
        seed_model(&conn, "openrouter", "anthropic/claude-3.5");
        // Deactivate the provider. The model row is still active in
        // the table, but the provider gate fails and we treat the
        // model as not routable.
        providers::set_active(&conn, &ProviderId::new("openrouter"), false).expect("deactivate");

        let plan = resolve(&conn, "anthropic/claude-3.5").expect("resolve");
        match plan {
            RoutingPlan::NotFound { .. } => {}
            other => panic!("expected NotFound for inactive provider, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // resolve → Direct with account_id = None when no healthy account exists
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_unhealthy_account_returns_direct_with_none() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        // No healthy account seeded. The resolver should still
        // produce a `Direct` plan; the pipeline's account-rotation
        // path is the one that will either find a healthy account
        // at request time (race-losing accounts are filtered by the
        // circuit breaker) or drop the target entirely.
        seed_model(&conn, "openrouter", "anthropic/claude-3.5");

        let plan = resolve(&conn, "anthropic/claude-3.5").expect("resolve");
        match plan {
            RoutingPlan::Direct { account_id, .. } => {
                assert!(
                    account_id.is_none(),
                    "no healthy account → account_id is None for auto-rotation"
                );
            }
            other => panic!("expected Direct, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // resolve honours the `<provider>/<upstream_id>` proxy-level id
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_strips_known_provider_prefix() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_healthy_account(&conn, "openrouter");
        // The upstream model id happens to contain another `/`:
        // "openrouter/foo/bar" must resolve to the model whose
        // model_id is "foo/bar" (not to a different one).
        seed_model(&conn, "openrouter", "foo/bar");

        let plan = resolve(&conn, "openrouter/foo/bar").expect("resolve");
        match plan {
            RoutingPlan::Direct { model_id, .. } => {
                assert_eq!(model_id, "foo/bar");
            }
            other => panic!("expected Direct, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // resolve does NOT strip an unknown provider prefix
    // -----------------------------------------------------------------------

    #[test]
    fn resolve_does_not_strip_unknown_provider_prefix() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        // No provider named "anthropic" in the table.
        seed_provider(&conn, "openrouter");
        seed_healthy_account(&conn, "openrouter");
        // The model id is stored verbatim with the `anthropic/`
        // prefix because that's what the upstream surfaces.
        seed_model(&conn, "openrouter", "anthropic/claude-3.5");

        let plan = resolve(&conn, "anthropic/claude-3.5").expect("resolve");
        match plan {
            RoutingPlan::Direct { model_id, .. } => {
                assert_eq!(model_id, "anthropic/claude-3.5");
            }
            other => panic!("expected Direct, got {:?}", other),
        }
    }

    // -----------------------------------------------------------------------
    // build_synthetic_combo produces a well-formed Combo
    // -----------------------------------------------------------------------

    #[test]
    fn build_synthetic_combo_is_well_formed() {
        let (combo, targets) = build_synthetic_combo(
            ProviderId::new("openrouter"),
            Some(AccountId(42)),
            ModelRowId(7),
            crate::providers::RateLimitScope::Account,
        );
        assert_eq!(combo.id, ComboId(SYNTHETIC_COMBO_ID));
        assert_eq!(combo.name, SYNTHETIC_COMBO_NAME);
        assert_eq!(combo.race_size, 1);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].provider_id, ProviderId::new("openrouter"));
        assert_eq!(targets[0].account_id, Some(AccountId(42)));
        assert_eq!(targets[0].model_row_id, Some(ModelRowId(7)));
    }
}
