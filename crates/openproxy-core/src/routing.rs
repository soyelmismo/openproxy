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
//! Side effects belong to the chat handler, the only caller: [`resolve`] is a
//! pure function over a `&Connection`.

use crate::error::Result;
use crate::ids::{AccountId, ComboId, ComboTargetId, ModelRowId, ProviderId};
use crate::models::{self, Model};
use openproxy_db::DbPool;
use openproxy_db::combos;
use openproxy_types::combos::{Combo, ComboTarget, Strategy};
use openproxy_types::error::CoreError;
use rusqlite::{Connection, OptionalExtension};

fn fetch_healthy_account_ids(
    conn: &Connection,
    provider_id: &ProviderId,
) -> Result<Vec<AccountId>> {
    let mut stmt = conn
        .prepare("SELECT id FROM accounts WHERE provider_id = ?1 AND health_status = 'healthy' ORDER BY priority ASC, id ASC")
        .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?;
    let ids = stmt
        .query_map(rusqlite::params![provider_id.as_str()], |r| {
            r.get::<_, i64>(0).map(AccountId)
        })
        .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| openproxy_types::error::CoreError::Internal(e.to_string()))?;
    Ok(ids)
}

fn expand_single_target(conn: &Connection, target: ComboTarget) -> Result<Vec<ComboTarget>> {
    if target.sub_combo_id.is_some() {
        return Ok(vec![target]);
    }
    if let Some(aid) = target.account_id {
        let is_healthy: bool = conn
            .query_row(
                "SELECT health_status = 'healthy' FROM accounts WHERE id = ?1",
                rusqlite::params![aid.0],
                |r| r.get(0),
            )
            .unwrap_or(true);
        if is_healthy {
            return Ok(vec![target]);
        }
        return Ok(vec![]);
    }
    let account_ids = fetch_healthy_account_ids(conn, &target.provider_id)?;
    if account_ids.is_empty() {
        return Ok(vec![target]);
    }
    let targets = account_ids
        .into_iter()
        .map(|aid| {
            let mut ct = target.clone();
            ct.account_id = Some(aid);
            ct
        })
        .collect();
    Ok(targets)
}

pub fn expand_account_rotation(
    conn: &Connection,
    targets: Vec<ComboTarget>,
) -> Result<Vec<ComboTarget>> {
    let mut out = Vec::with_capacity(targets.len());
    for t in targets {
        out.extend(expand_single_target(conn, t)?);
    }
    Ok(out)
}

pub fn flatten_targets(conn: &Connection, targets: Vec<ComboTarget>) -> Result<Vec<ComboTarget>> {
    if !targets.iter().any(|t| t.sub_combo_id.is_some()) {
        return Ok(targets);
    }
    let mut out = Vec::with_capacity(targets.len());
    let mut visited = std::collections::HashSet::new();
    for t in targets {
        if let Some(sub_id) = t.sub_combo_id {
            let sub_flat = resolve_combo_to_targets(conn, sub_id, &mut visited, 0)?;
            out.extend(sub_flat);
        } else {
            out.push(t);
        }
    }
    Ok(out)
}

fn resolve_combo_to_targets(
    conn: &Connection,
    combo_id: ComboId,
    visited: &mut std::collections::HashSet<ComboId>,
    depth: u32,
) -> Result<Vec<ComboTarget>> {
    if depth > 5 {
        return Err(openproxy_types::error::CoreError::Validation(format!(
            "max sub-combo depth ({}) exceeded",
            5
        )));
    }
    if visited.contains(&combo_id) {
        return Err(openproxy_types::error::CoreError::Validation(format!(
            "cyclic combo detected at id {}",
            combo_id.0
        )));
    }
    visited.insert(combo_id);

    // list_targets base, sin cooldown filter: pre-flight de audio/chat
    let targets = combos::list_targets(conn, combo_id)?;
    let mut flat = Vec::with_capacity(targets.len());
    for t in targets {
        if let Some(sub_id) = t.sub_combo_id {
            let sub = resolve_combo_to_targets(conn, sub_id, visited, depth + 1)?;
            flat.extend(sub);
        } else {
            flat.push(t);
        }
    }
    visited.remove(&combo_id);
    Ok(flat)
}

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

/// Resolve a model string to a routing plan with optional provider and account pinning.
pub fn resolve_with_options(
    conn: &Connection,
    model_str: &str,
    provider_override: Option<&str>,
    account_override: Option<AccountId>,
) -> Result<RoutingPlan> {
    let (stripped, mut provider_prefix) = strip_proxy_prefix(conn, model_str);
    if provider_prefix.is_none()
        && let Some(po) = provider_override
    {
        provider_prefix = Some(po);
    }

    if let Some(plan) = try_resolve_direct_model(conn, stripped, provider_prefix, account_override)?
    {
        return Ok(plan);
    }

    let combo_name = stripped.strip_prefix("combo:").unwrap_or(stripped);
    if let Some(combo) = combos::get_combo_by_name(conn, combo_name)? {
        let mut targets = combos::list_targets(conn, combo.id)?;
        if let Some(aid) = account_override {
            pin_account_to_targets(conn, &mut targets, aid)?;
        }
        return Ok(RoutingPlan::Combo {
            combo_id: combo.id,
            combo_name: combo.name,
            strategy: combo.strategy,
            race_size: combo.race_size,
            targets,
        });
    }

    let hint = if combo_name == stripped {
        None
    } else {
        // input carried a `combo:` prefix: hand back the normalised name
        Some(format!("combo:{combo_name}"))
    };
    Ok(RoutingPlan::NotFound {
        model: model_str.to_string(),
        hint,
    })
}

/// Resolve a model string to a routing plan.
///
/// `model_str` is the raw `model` field from the chat request: strip a matching
/// `<provider>/` prefix, try a `models` row (active, not expired), then a combo
/// (after an optional `combo:` prefix), then `NotFound`.
///
/// Matching is case-sensitive. `ComBo:nerd` does not resolve to combo `nerd`,
/// the same convention stored names use.
pub fn resolve(conn: &Connection, model_str: &str) -> Result<RoutingPlan> {
    resolve_with_options(conn, model_str, None, None)
}

/// [`resolve`] on the blocking pool: its `SELECT`s take the [`DbPool`] reader
/// mutex, a blocking `parking_lot` lock that must not be held on a Tokio
/// worker (AGENTS §4.3).
///
/// The pool and `model` are cloned into the closure by value so the blocking
/// thread borrows no caller frame. A `JoinError` means the task was cancelled
/// or panicked, which maps to [`CoreError::Internal`].
pub async fn resolve_routing(db_pool: &DbPool, model: &str) -> Result<RoutingPlan> {
    let pool = db_pool.clone();
    let model = model.to_owned();
    tokio::task::spawn_blocking(move || {
        let conn = pool.reader();
        resolve(&conn, &model)
    })
    .await
    .map_err(|e| CoreError::Internal(format!("resolve_routing join error: {e}")))?
}

pub async fn resolve_routing_with_options(
    db_pool: &DbPool,
    model: &str,
    provider_override: Option<String>,
    account_override: Option<AccountId>,
) -> Result<RoutingPlan> {
    let pool = db_pool.clone();
    let model = model.to_owned();
    tokio::task::spawn_blocking(move || {
        let conn = pool.reader();
        resolve_with_options(
            &conn,
            &model,
            provider_override.as_deref(),
            account_override,
        )
    })
    .await
    .map_err(|e| CoreError::Internal(format!("resolve_routing join error: {e}")))?
}

fn pin_account_to_targets(
    conn: &Connection,
    targets: &mut Vec<ComboTarget>,
    aid: AccountId,
) -> Result<()> {
    let prov_row: Option<String> = conn
        .query_row(
            "SELECT provider_id FROM accounts WHERE id = ?1",
            rusqlite::params![aid.0],
            |row| row.get(0),
        )
        .optional()
        .map_err(openproxy_db::error::map_db_error_ctx(
            "pin_account_to_targets",
        ))?;

    let Some(prov) = prov_row else {
        return Ok(());
    };

    let has_matching = targets.iter().any(|t| t.provider_id.as_str() == prov);
    if has_matching {
        for t in &mut *targets {
            if t.provider_id.as_str() == prov {
                t.account_id = Some(aid);
            }
        }
        targets.retain(|t| t.provider_id.as_str() == prov || t.sub_combo_id.is_some());
    }
    Ok(())
}

fn try_resolve_direct_model(
    conn: &Connection,
    model_id: &str,
    provider_prefix: Option<&str>,
    account_override: Option<AccountId>,
) -> Result<Option<RoutingPlan>> {
    let model: Option<Model> = if let Some(prefix) = provider_prefix {
        models::find_active_by_provider_and_name(conn, &ProviderId::new(prefix), model_id)?
    } else {
        models::find_active_by_name(conn, model_id)?
    };
    let Some(model) = model else {
        return Ok(None);
    };

    // a deactivated provider makes the model unroutable. Reported as "no
    // match" rather than 5xx: the operator can re-enable the provider.
    let (active, rate_limit_scope) = provider_active_and_scope(conn, &model.provider_id)?;
    if !active {
        return Ok(None);
    }

    let account_id = if let Some(aid) = account_override {
        let belongs_to_provider: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM accounts WHERE id = ?1 AND provider_id = ?2)",
                rusqlite::params![aid.0, model.provider_id.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .is_ok_and(|v| v != 0);
        if belongs_to_provider {
            Some(aid)
        } else {
            tracing::warn!(
                account_id = aid.0,
                provider_id = model.provider_id.as_str(),
                "pinned account does not belong to provider, falling back to auto-rotation"
            );
            None
        }
    } else {
        None
    };

    let (combo, targets) = build_synthetic_combo(
        model.provider_id.clone(),
        account_id,
        model.row_id,
        rate_limit_scope,
    );

    Ok(Some(RoutingPlan::Combo {
        combo_id: combo.id,
        combo_name: combo.name,
        strategy: combo.strategy,
        race_size: combo.race_size,
        targets,
    }))
}

/// "Is this provider active?" probe plus its rate limit scope. A missing row
/// reads as inactive so a provider deleted mid-race with the admin UI does not
/// route.
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
        .map_err(openproxy_db::error::map_db_error_ctx(format!(
            "provider_active_and_scope({provider_id})"
        )))?;
    match row {
        Some((active, scope_str)) => {
            let scope = crate::providers::RateLimitScope::parse(&scope_str).unwrap_or_default();
            Ok((active != 0, scope))
        }
        None => Ok((false, crate::providers::RateLimitScope::default())),
    }
}

pub use openproxy_db::models::strip_proxy_prefix;

/// Synthetic in-memory `Combo` plus its single `ComboTarget` for a
/// direct-model dispatch. The `Combo` is shaped so the pipeline's `load_combo`
/// accepts it. Targets go into the `PipelineRequest::targets_override` slot.
pub fn build_synthetic_combo(
    provider_id: ProviderId,
    account_id: Option<AccountId>,
    model_row_id: ModelRowId,
    rate_limit_scope: crate::providers::RateLimitScope,
) -> (Combo, Vec<ComboTarget>) {
    // id 0 never collides with a real `combo_targets.id`, and usage rows carry
    // `target.id` in `combo_target_id`, a free-form INTEGER with no FK.
    let target = ComboTarget {
        id: ComboTargetId(0),
        combo_id: ComboId(SYNTHETIC_COMBO_ID),
        provider_id,
        account_id,
        model_row_id: Some(model_row_id),
        sub_combo_id: None,
        priority_order: 0,
        weight: 1,
        active: true,
        rate_limit_scope,
        cooldown_mode: None,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        thinking_effort: None,
        description: None,
    };
    let combo = Combo {
        id: ComboId(SYNTHETIC_COMBO_ID),
        name: SYNTHETIC_COMBO_NAME.to_string(),
        strategy: Strategy::Priority,
        race_size: 1,
        created_at: String::new(),
        context_window: None,
        // strict priority + flat cooldown: a direct-model dispatch must behave
        // like a single-target user combo
        priority_mode: openproxy_types::combos::PriorityMode::Strict,
        cooldown_mode: openproxy_types::config::CooldownMode::Flat,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
        preventive_rate_limit: false,
        decision_model: None,
        decision_timeout_ms: None,
    };
    (combo, vec![target])
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_db::conn::DbPool;

    use crate::providers::{self, AuthType, ProviderFormat};
    use std::path::PathBuf;

    /// Fresh on-disk pool with migrations applied. Unique tempdir per
    /// test to avoid `WAL`-file collisions in parallel runs.
    fn fresh_pool() -> (DbPool, PathBuf) {
        let pool = DbPool::test_pool_with_prefix("openproxy-routing-test").expect("open pool");
        let path = pool.path().to_path_buf();
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

    #[test]
    fn resolve_direct_model_returns_plan() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_healthy_account(&conn, "openrouter");
        let model_row = seed_model(&conn, "openrouter", "anthropic/claude-3.5");

        let plan = resolve(&conn, "anthropic/claude-3.5").expect("resolve");
        let RoutingPlan::Combo { targets, .. } = plan else {
            panic!("expected Combo, got {plan:?}");
        };
        assert_eq!(targets[0].provider_id, ProviderId::new("openrouter"));
        assert_eq!(targets[0].model_row_id, Some(model_row));
        assert!(
            targets[0].account_id.is_none(),
            "resolver always uses auto-rotation"
        );
    }

    #[test]
    fn resolve_combo_with_prefix() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let combo_id = combos::create_combo(&conn, "smart", Strategy::Priority, 1).expect("create");

        let plan = resolve(&conn, "combo:smart").expect("resolve");
        let RoutingPlan::Combo {
            combo_id: got_id,
            combo_name,
            ..
        } = plan
        else {
            panic!("expected Combo, got {plan:?}");
        };
        assert_eq!(got_id, combo_id);
        assert_eq!(combo_name, "smart");
    }

    #[test]
    fn resolve_combo_without_prefix() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        let combo_id = combos::create_combo(&conn, "smart", Strategy::Priority, 1).expect("create");

        let plan = resolve(&conn, "smart").expect("resolve");
        let RoutingPlan::Combo {
            combo_id: got_id,
            combo_name,
            ..
        } = plan
        else {
            panic!("expected Combo, got {plan:?}");
        };
        assert_eq!(got_id, combo_id);
        assert_eq!(combo_name, "smart");
    }

    #[test]
    fn resolve_not_found() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_model(&conn, "openrouter", "real-model");
        let plan = resolve(&conn, "ghost").expect("resolve");
        let RoutingPlan::NotFound { model, hint } = plan else {
            panic!("expected NotFound, got {plan:?}");
        };
        assert_eq!(model, "ghost");
        assert!(hint.is_none(), "no combo: prefix → no hint");
    }

    #[test]
    fn resolve_inactive_provider_returns_not_found() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_healthy_account(&conn, "openrouter");
        seed_model(&conn, "openrouter", "anthropic/claude-3.5");
        // the model row is still active, the provider gate is what fails
        providers::set_active(&conn, &ProviderId::new("openrouter"), false).expect("deactivate");

        let plan = resolve(&conn, "anthropic/claude-3.5").expect("resolve");
        let RoutingPlan::NotFound { .. } = plan else {
            panic!("expected NotFound for inactive provider, got {plan:?}");
        };
    }

    #[test]
    fn resolve_unhealthy_account_returns_direct_with_none() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        // no healthy account: the plan still resolves, rotation happens at
        // request time
        seed_model(&conn, "openrouter", "anthropic/claude-3.5");

        let plan = resolve(&conn, "anthropic/claude-3.5").expect("resolve");
        let RoutingPlan::Combo { targets, .. } = plan else {
            panic!("expected Combo, got {plan:?}");
        };
        assert!(
            targets[0].account_id.is_none(),
            "no healthy account → account_id is None for auto-rotation"
        );
    }

    #[test]
    fn resolve_strips_known_provider_prefix() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_healthy_account(&conn, "openrouter");
        // upstream ids may carry their own `/`: "openrouter/foo/bar" must
        // resolve to model_id "foo/bar"
        seed_model(&conn, "openrouter", "foo/bar");

        let plan = resolve(&conn, "openrouter/foo/bar").expect("resolve");
        let RoutingPlan::Combo { .. } = plan else {
            panic!("expected Combo, got {plan:?}");
        };
    }

    #[test]
    fn resolve_does_not_strip_unknown_provider_prefix() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_healthy_account(&conn, "openrouter");
        // no provider named "anthropic": `anthropic/` stays part of the
        // model id
        seed_model(&conn, "openrouter", "anthropic/claude-3.5");

        let plan = resolve(&conn, "anthropic/claude-3.5").expect("resolve");
        let RoutingPlan::Combo { .. } = plan else {
            panic!("expected Combo, got {plan:?}");
        };
    }

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

    #[test]
    fn resolve_with_options_pins_provider_and_account() {
        let (pool, _path) = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "justwoker");
        let account_id = seed_healthy_account(&conn, "justwoker");
        let model_row = seed_model(&conn, "justwoker", "claude-opus-4-8");

        let plan = resolve_with_options(
            &conn,
            "claude-opus-4-8",
            Some("justwoker"),
            Some(account_id),
        )
        .expect("resolve_with_options");

        let RoutingPlan::Combo { targets, .. } = plan else {
            panic!("expected Combo, got {plan:?}");
        };
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].provider_id, ProviderId::new("justwoker"));
        assert_eq!(targets[0].model_row_id, Some(model_row));
        assert_eq!(targets[0].account_id, Some(account_id));
    }
}
