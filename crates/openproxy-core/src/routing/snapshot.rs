//! In-memory routing snapshot: resolves the routing plan without hitting the
//! SQLite reader pool on the request hot path.
//!
//! The top submissions of the $12-server-challenge keep request-time data
//! resident in memory. Openproxy's equivalent is this snapshot:
//! `models`/`providers`/`accounts`/`combos`/`combo_targets` get materialised
//! once into `HashMap`s and the per-request `resolve` path becomes a few
//! hash lookups instead of 3-7 `SELECT`s plus a `spawn_blocking` hop.
//!
//! Freshness comes from SQLite's `PRAGMA data_version`, which increments on
//! every external commit while the snapshot's own connection stays open. A
//! version bump rebuilds the snapshot; an unchanged version means the cached
//! data is current. No TTL guessing.
//!
//! `list_targets`' cooldown predicates (`tc.cooldown_until <= now` and the
//! per-model `MAX(cooldown_until)` future aggregate) are evaluated at resolve
//! time against `datetime('now')` — same string format, lexicographic compare
//! is identical to SQLite's `datetime()` comparison.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};

use openproxy_db::DbPool;
use openproxy_types::combos::{Combo, ComboTarget};
use openproxy_types::ids::{AccountId, ModelRowId};
use openproxy_types::{CoreError, ProviderId, RateLimitScope, Result};

use super::{RoutingPlan, build_synthetic_combo};

/// One `combo_targets` row plus the fields `list_targets` needs for its
/// WHERE clause, kept per-row so resolve-time filtering stays pure.
struct TargetRow {
    target: ComboTarget,
    cooldown_until: Option<String>,
    model_cooldown_until: Option<String>,
    model_active: bool,
    provider_active: bool,
}

#[derive(Default)]
pub struct RoutingSnapshot {
    /// `model_id` -> lowest-`id` active model (`ORDER BY id ASC LIMIT 1`).
    models_by_name: HashMap<String, ModelRowIdProvider>,
    /// `(provider_id, model_id)` -> lowest-`id` active model.
    models_by_provider_name: HashMap<(String, String), ModelRowIdProvider>,
    /// provider id -> (active, rate_limit_scope).
    providers: HashMap<String, (bool, RateLimitScope)>,
    /// account id -> (provider_id, is_healthy).
    accounts: HashMap<i64, (String, bool)>,
    /// provider id -> healthy account ids ordered `priority ASC, id ASC`.
    healthy_accounts: HashMap<String, Vec<AccountId>>,
    /// combo name -> combo row.
    combos_by_name: HashMap<String, Combo>,
    /// combo id -> target rows sorted `priority_order, id`.
    combo_targets: HashMap<i64, Vec<TargetRow>>,
}

/// The two `Model` fields `try_resolve_direct_model` needs. Avoids hydrating
/// the full 20-column `Model` row per request.
#[derive(Clone)]
struct ModelRowIdProvider {
    row_id: ModelRowId,
    provider_id: ProviderId,
}

struct SnapshotState {
    /// Canonical DB path the snapshot was built against. Epochs are
    /// process-global, so the cache key is (path, config_epoch,
    /// cooldown_epoch): test pools and any reopened DB get their own
    /// snapshot instead of colliding on epoch values from a different file.
    db_key: String,
    config_version: u64,
    cooldown_version: u64,
    /// `(data.db mtime, data.db-wal mtime)` plus the pool's `write_epoch`
    /// observed at build time. Used only to detect commits made *outside*
    /// this pool (sqlite3 CLI, restores, other processes), which the writer
    /// hooks cannot see: when the file moved but `write_epoch` did not, an
    /// external writer committed. Internal writes to unrelated tables bump
    /// `write_epoch`, so they only refresh `file_version` — no rebuild.
    file_version: (u64, u64),
    write_epoch: u64,
    /// `PRAGMA data_version` snapshot for the ambiguous window where file +
    /// write_epoch both moved: settles whether a foreign commit hid among
    /// our own.
    external_version: Option<u64>,
    snap: Arc<RoutingSnapshot>,
}

static SNAP: RwLock<Option<SnapshotState>> = RwLock::new(None);
static REBUILD: Mutex<()> = Mutex::new(());

fn map_err(ctx: &'static str) -> impl FnOnce(rusqlite::Error) -> CoreError {
    move |e| CoreError::Internal(format!("routing snapshot {ctx}: {e}"))
}

fn build(pool: &DbPool) -> Result<RoutingSnapshot> {
    let conn = pool.reader();
    let mut snap = RoutingSnapshot::default();

    let mut stmt = conn
        .prepare("SELECT id, model_id, provider_id FROM models WHERE active = 1 ORDER BY id ASC")
        .map_err(map_err("models"))?;
    {
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(map_err("models"))?;
        for row in rows {
            let (row_id, model_id, provider_id) = row.map_err(map_err("models"))?;
            let entry = ModelRowIdProvider {
                row_id: ModelRowId(row_id),
                provider_id: ProviderId::new(provider_id.clone()),
            };
            snap.models_by_provider_name
                .entry((provider_id, model_id.clone()))
                .or_insert_with(|| entry.clone());
            snap.models_by_name.entry(model_id).or_insert(entry);
        }
    }

    let mut stmt = conn
        .prepare("SELECT id, active, rate_limit_scope FROM providers")
        .map_err(map_err("providers"))?;
    {
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(map_err("providers"))?;
        for row in rows {
            let (id, active, scope) = row.map_err(map_err("providers"))?;
            snap.providers.insert(
                id,
                (
                    active != 0,
                    RateLimitScope::parse(&scope).unwrap_or_default(),
                ),
            );
        }
    }

    let mut stmt = conn
        .prepare("SELECT id, provider_id, health_status FROM accounts")
        .map_err(map_err("accounts"))?;
    {
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(map_err("accounts"))?;
        for row in rows {
            let (id, provider_id, health) = row.map_err(map_err("accounts"))?;
            snap.accounts.insert(id, (provider_id, health == "healthy"));
        }
    }

    let mut stmt = conn
        .prepare(
            "SELECT provider_id, id FROM accounts WHERE health_status = 'healthy' \
             ORDER BY provider_id, priority ASC, id ASC",
        )
        .map_err(map_err("healthy accounts"))?;
    {
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .map_err(map_err("healthy accounts"))?;
        for row in rows {
            let (provider_id, id) = row.map_err(map_err("healthy accounts"))?;
            snap.healthy_accounts
                .entry(provider_id)
                .or_default()
                .push(AccountId(id));
        }
    }

    for combo in openproxy_db::combos::list_combos(&conn)? {
        snap.combos_by_name.insert(combo.name.clone(), combo);
    }

    let mut stmt = conn
        .prepare(
            "SELECT ct.id, ct.combo_id, ct.provider_id, ct.account_id, ct.model_row_id, \
             ct.sub_combo_id, ct.priority_order, ct.weight, p.rate_limit_scope, ct.active, \
             ct.cooldown_mode, ct.cooldown_base_secs, ct.cooldown_max_secs, ct.cooldown_factor, \
             ct.thinking_effort, ct.description, ct.selected_accounts, \
             tc.cooldown_until, mc.model_cooldown_until, m.active \
             FROM combo_targets ct \
             INNER JOIN providers p ON p.id = ct.provider_id \
             LEFT JOIN target_cooldowns tc ON tc.combo_target_id = ct.id \
             LEFT JOIN models m ON m.id = ct.model_row_id \
             LEFT JOIN ( \
                 SELECT ct2.model_row_id, MAX(tc2.cooldown_until) AS model_cooldown_until \
                 FROM target_cooldowns tc2 \
                 INNER JOIN combo_targets ct2 ON ct2.id = tc2.combo_target_id \
                 WHERE ct2.model_row_id IS NOT NULL \
                   AND datetime(tc2.cooldown_until) > datetime('now') \
                 GROUP BY ct2.model_row_id \
             ) mc ON mc.model_row_id = ct.model_row_id \
             ORDER BY ct.combo_id, ct.priority_order ASC, ct.id ASC",
        )
        .map_err(map_err("targets"))?;
    {
        let rows = stmt
            .query_map([], |r| {
                let target = openproxy_db::combos::mapping::row_to_target(r)?;
                Ok((
                    target.combo_id.0,
                    TargetRow {
                        target,
                        cooldown_until: r.get(17)?,
                        model_cooldown_until: r.get(18)?,
                        model_active: r.get::<_, Option<i64>>(19)?.unwrap_or(0) != 0,
                        provider_active: true, // INNER JOIN p guarantees a row; `active` checked below
                    },
                ))
            })
            .map_err(map_err("targets"))?;
        // `provider_active` needs p.active: selected implicitly via providers map.
        for row in rows {
            let (combo_id, mut trow) = row.map_err(map_err("targets"))?;
            if let Some((p_active, scope)) = snap.providers.get(trow.target.provider_id.as_str()) {
                trow.provider_active = *p_active;
                // provider scope comes from the join column already; keep it.
                let _ = scope;
            } else {
                trow.provider_active = false;
            }
            snap.combo_targets.entry(combo_id).or_default().push(trow);
        }
    }

    Ok(snap)
}

/// The writer's `update_hook` bumps the two epochs on every INSERT/UPDATE/
/// DELETE to the routing tables, catching every write path in the process —
/// including raw `conn.execute` calls that bypass helper functions. The
/// third component (`file_version`) catches commits made by other processes
/// on the same file, which the hooks cannot see.
fn probe_versions(pool: &DbPool) -> (u64, u64, (u64, u64), u64) {
    (
        openproxy_db::routing_config_epoch(),
        openproxy_db::routing_cooldown_epoch(),
        pool.file_version(),
        pool.write_epoch(),
    )
}

/// Cooldown inserts bump the routing epoch, which would otherwise rebuild
/// the whole 8k-model snapshot on every failed request. This interval keeps
/// rebuilds bounded while a cooldown write becomes visible within ~50ms.
/// Models/providers/combo changes see the same delay: acceptable, admin
/// operations are seconds-apart anyway.
const MIN_REBUILD: std::time::Duration = std::time::Duration::from_millis(50);

fn last_rebuilt() -> &'static std::sync::atomic::AtomicU64 {
    static T: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    &T
}

fn millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Current snapshot, rebuilding when `data_version` moved since the last
/// build. Uses the pool reader for both the probe and the rebuild: readers
/// are a small mutex pool, so the rebuild holds one reader briefly — same
/// cost class as the SELECTs this snapshot replaces.
fn db_key(pool: &DbPool) -> String {
    pool.path().to_string_lossy().into_owned()
}

fn get_snapshot(pool: &DbPool) -> Result<Arc<RoutingSnapshot>> {
    let key = db_key(pool);
    let (config_v, cooldown_v, file_v, write_v) = probe_versions(pool);
    {
        let mut guard = SNAP.write().unwrap_or_else(|e| e.into_inner());
        if let Some(st) = guard.as_mut()
            && st.db_key == key
        {
            // External-writer probe. The file moved; classify the change:
            //   - write_epoch also moved -> our own commit explains the
            //     mtime; refresh the baseline silently (no rebuild needed
            //     for non-routing tables).
            //   - write_epoch still -> nothing committed through us, so a
            //     foreign connection wrote: rebuild.
            //   - ambiguous both-moved case: `PRAGMA data_version` settles
            //     it — it only ticks on foreign commits.
            let file_moved = st.file_version != file_v;
            let external_write = file_moved
                && (st.write_epoch == write_v
                    // Internal + external commits raced in one window:
                    // data_version only ticks on foreign commits.
                    || st.external_version != pool.external_version());
            if external_write {
                // fall through to rebuild
            } else {
                // Internal commit to an unrelated table (usage records,
                // journal): refresh the file baseline without rebuilding.
                if st.file_version != file_v {
                    st.file_version = file_v;
                    st.write_epoch = write_v;
                }
                if st.config_version == config_v && st.cooldown_version == cooldown_v {
                    return Ok(Arc::clone(&st.snap));
                }
                // Cooldown-only drift within the debounce window: serve the
                // snapshot; its stored timestamps still filter correctly,
                // only a *newly written* cooldown row is invisible for up to
                // MIN_REBUILD.
                if st.config_version == config_v
                    && millis_now()
                        .saturating_sub(last_rebuilt().load(std::sync::atomic::Ordering::Acquire))
                        < MIN_REBUILD.as_millis() as u64
                {
                    return Ok(Arc::clone(&st.snap));
                }
            }
        }
    }
    let _build_guard = REBUILD.lock().unwrap_or_else(|e| e.into_inner());
    let (config_v, cooldown_v, file_v, write_v) = probe_versions(pool);
    {
        let mut guard = SNAP.write().unwrap_or_else(|e| e.into_inner());
        if let Some(st) = guard.as_mut()
            && st.db_key == key
            && st.file_version == file_v
            && st.write_epoch == write_v
            && st.config_version == config_v
            && st.cooldown_version == cooldown_v
        {
            return Ok(Arc::clone(&st.snap));
        }
    }
    let snap = Arc::new(build(pool)?);
    *SNAP.write().unwrap_or_else(|e| e.into_inner()) = Some(SnapshotState {
        db_key: key,
        config_version: config_v,
        cooldown_version: cooldown_v,
        file_version: file_v,
        write_epoch: write_v,
        external_version: pool.external_version(),
        snap: Arc::clone(&snap),
    });
    last_rebuilt().store(millis_now(), std::sync::atomic::Ordering::Release);
    Ok(snap)
}

fn strip_prefix<'a>(snap: &RoutingSnapshot, model_str: &'a str) -> (&'a str, Option<&'a str>) {
    if model_str.starts_with("combo:") {
        return (model_str, None);
    }
    let Some((prefix, rest)) = model_str.split_once('/') else {
        return (model_str, None);
    };
    if prefix.is_empty() {
        return (model_str, None);
    }
    if snap.providers.contains_key(prefix) {
        (rest, Some(prefix))
    } else {
        (model_str, None)
    }
}

fn target_row_active(snap: &RoutingSnapshot, r: &TargetRow, now: &str) -> bool {
    if !r.target.active || !r.provider_active {
        return false;
    }
    // `ct.model_row_id IS NULL OR m.active = 1`; NULL model + NULL sub_combo
    // rows are dead config and excluded by list_targets too.
    match (r.target.model_row_id, r.target.sub_combo_id) {
        (None, None) => return false,
        (Some(_), _) if !r.model_active => return false,
        _ => {}
    }
    if let Some(c) = r.cooldown_until.as_deref()
        && c > now
    {
        return false;
    }
    if r.model_cooldown_until.is_some() {
        return false;
    }
    let _ = snap;
    true
}

/// `datetime('now')` in SQLite's `YYYY-MM-DD HH:MM:SS` format.
fn now_str(pool: &DbPool) -> String {
    let conn = pool.reader();
    conn.query_row("SELECT datetime('now')", [], |r| r.get::<_, String>(0))
        .unwrap_or_default()
}

/// Snapshot-based `resolve_with_options`. Falls back to `Err` only when the
/// snapshot itself fails to build (caller then uses the SQL path).
pub fn resolve_with_options_snap(
    pool: &DbPool,
    model_str: &str,
    provider_override: Option<&str>,
    account_override: Option<AccountId>,
) -> Result<RoutingPlan> {
    let snap = get_snapshot(pool)?;
    let (stripped, mut provider_prefix) = strip_prefix(&snap, model_str);
    if provider_prefix.is_none()
        && let Some(po) = provider_override
    {
        provider_prefix = Some(po);
    }

    if let Some(plan) = try_resolve_direct(&snap, stripped, provider_prefix, account_override) {
        return Ok(plan);
    }

    let combo_name = stripped.strip_prefix("combo:").unwrap_or(stripped);
    if let Some(combo) = snap.combos_by_name.get(combo_name) {
        let now = now_str(pool);
        let rows = snap
            .combo_targets
            .get(&combo.id.0)
            .map_or(&[][..], |v| v.as_slice());
        let mut targets: Vec<ComboTarget> = rows
            .iter()
            .filter(|r| target_row_active(&snap, r, &now))
            .map(|r| r.target.clone())
            .collect();
        if let Some(aid) = account_override {
            pin_account(&snap, &mut targets, aid);
        }
        return Ok(RoutingPlan::Combo {
            combo_id: combo.id,
            combo_name: combo.name.clone(),
            strategy: combo.strategy,
            race_size: combo.race_size,
            targets,
        });
    }

    let hint = if combo_name == stripped {
        None
    } else {
        Some(format!("combo:{combo_name}"))
    };
    Ok(RoutingPlan::NotFound {
        model: model_str.to_string(),
        hint,
    })
}

fn try_resolve_direct(
    snap: &RoutingSnapshot,
    model_id: &str,
    provider_prefix: Option<&str>,
    account_override: Option<AccountId>,
) -> Option<RoutingPlan> {
    let model = if let Some(prefix) = provider_prefix {
        snap.models_by_provider_name
            .get(&(prefix.to_string(), model_id.to_string()))
    } else {
        snap.models_by_name.get(model_id)
    }?;
    let (active, scope) = snap
        .providers
        .get(model.provider_id.as_str())
        .copied()
        .unwrap_or((false, RateLimitScope::default()));
    if !active {
        return None;
    }
    let account_id = if let Some(aid) = account_override {
        match snap.accounts.get(&aid.0) {
            Some((prov, _)) if prov == model.provider_id.as_str() => Some(aid),
            Some(_) => {
                tracing::warn!(
                    account_id = aid.0,
                    provider_id = model.provider_id.as_str(),
                    "pinned account does not belong to provider, falling back to auto-rotation"
                );
                None
            }
            None => None,
        }
    } else {
        None
    };
    let (combo, targets) =
        build_synthetic_combo(model.provider_id.clone(), account_id, model.row_id, scope);
    Some(RoutingPlan::Combo {
        combo_id: combo.id,
        combo_name: combo.name,
        strategy: combo.strategy,
        race_size: combo.race_size,
        targets,
    })
}

fn pin_account(snap: &RoutingSnapshot, targets: &mut Vec<ComboTarget>, aid: AccountId) {
    let Some((prov, _)) = snap.accounts.get(&aid.0) else {
        return;
    };
    if targets.iter().any(|t| t.provider_id.as_str() == prov) {
        for t in targets.iter_mut() {
            if t.provider_id.as_str() == prov {
                t.account_id = Some(aid);
            }
        }
        targets.retain(|t| t.provider_id.as_str() == prov || t.sub_combo_id.is_some());
    }
}

#[cfg(test)]
mod tests {
    //! Parity: the snapshot path must return the same `RoutingPlan` as the
    //! SQL path for the same DB state. Every test seeds through the writer,
    //! then resolves both ways and compares structurally (debug format is
    //! enough — the plan carries no timestamps).
    use super::*;
    use crate::providers::{self, AuthType, ProviderFormat};
    use openproxy_db::combos;
    use openproxy_types::combos::Strategy;

    fn fresh_pool() -> DbPool {
        DbPool::test_pool_with_prefix("openproxy-snap-test").expect("open pool")
    }

    fn seed_provider(conn: &rusqlite::Connection, id: &str) {
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
                rate_limit_scope: RateLimitScope::Account,
            },
        )
        .expect("seed provider");
    }

    fn seed_model(conn: &rusqlite::Connection, provider: &str, model_id: &str) {
        conn.execute(
            "INSERT INTO models(provider_id, model_id, target_format) \
             VALUES (?1, ?2, 'openai')",
            rusqlite::params![provider, model_id],
        )
        .expect("seed model");
    }

    fn seed_healthy_account(conn: &rusqlite::Connection, provider_id: &str) {
        conn.execute(
            "INSERT INTO accounts(provider_id, api_key_encrypted, health_status) \
             VALUES (?1, X'00', 'healthy')",
            rusqlite::params![provider_id],
        )
        .expect("seed account");
    }

    /// The snapshot is process-global; each test gets its own DbPool but the
    /// snapshot only rebuilds when *its* pool's data_version moves. To keep
    /// tests hermetic we resolve through `resolve_with_options_snap(pool,…)`
    /// which always probes the passed pool — a test that wrote rows sees
    /// them because the writer's commits bump data_version on any reader.
    #[test]
    fn snapshot_direct_model_matches_sql() {
        let pool = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_healthy_account(&conn, "openrouter");
        seed_model(&conn, "openrouter", "m1");

        let sql = super::super::resolve(&conn, "m1").expect("sql resolve");
        let snap = resolve_with_options_snap(&pool, "m1", None, None).expect("snap resolve");
        assert_eq!(format!("{sql:?}"), format!("{snap:?}"));
    }

    #[test]
    fn snapshot_provider_prefix_matches_sql() {
        let pool = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "p1");
        seed_provider(&conn, "p2");
        seed_healthy_account(&conn, "p2");
        seed_model(&conn, "p2", "x");

        let sql = super::super::resolve(&conn, "p2/x").expect("sql resolve");
        let snap = resolve_with_options_snap(&pool, "p2/x", None, None).expect("snap resolve");
        assert_eq!(format!("{sql:?}"), format!("{snap:?}"));
    }

    #[test]
    fn snapshot_not_found_matches_sql() {
        let pool = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_model(&conn, "openrouter", "real");

        let sql = super::super::resolve(&conn, "ghost").expect("sql resolve");
        let snap = resolve_with_options_snap(&pool, "ghost", None, None).expect("snap resolve");
        assert_eq!(format!("{sql:?}"), format!("{snap:?}"));
    }

    #[test]
    fn snapshot_inactive_provider_returns_not_found() {
        let pool = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_model(&conn, "openrouter", "m1");
        providers::set_active(&conn, &ProviderId::new("openrouter"), false).expect("deactivate");

        let snap = resolve_with_options_snap(&pool, "m1", None, None).expect("snap resolve");
        assert!(matches!(snap, RoutingPlan::NotFound { .. }));
    }

    #[test]
    fn snapshot_rebuild_picks_up_new_rows() {
        let pool = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        // First resolve builds the snapshot without the model.
        let first = resolve_with_options_snap(&pool, "late-model", None, None).expect("snap");
        assert!(matches!(first, RoutingPlan::NotFound { .. }));
        // Commit a row on the writer: data_version moves, next probe rebuilds.
        seed_model(&conn, "openrouter", "late-model");
        let second = resolve_with_options_snap(&pool, "late-model", None, None).expect("snap");
        assert!(matches!(second, RoutingPlan::Combo { .. }));
    }

    #[test]
    fn snapshot_combo_targets_respect_cooldown() {
        let pool = fresh_pool();
        let conn = pool.writer();
        seed_provider(&conn, "openrouter");
        seed_healthy_account(&conn, "openrouter");
        seed_model(&conn, "openrouter", "m1");
        let combo_id = combos::create_combo(&conn, "c", Strategy::Priority, 1).expect("combo");
        let model_row: i64 = conn
            .query_row("SELECT id FROM models WHERE model_id = 'm1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO combo_targets(combo_id, provider_id, model_row_id, priority_order) \
             VALUES (?1, 'openrouter', ?2, 0)",
            rusqlite::params![combo_id.0, model_row],
        )
        .expect("target");
        // Cool the target far in the future.
        let tid: i64 = conn
            .query_row("SELECT id FROM combo_targets LIMIT 1", [], |r| r.get(0))
            .unwrap();
        conn.execute(
            "INSERT INTO target_cooldowns(combo_target_id, cooldown_until) \
             VALUES (?1, '2999-01-01 00:00:00')",
            rusqlite::params![tid],
        )
        .expect("cooldown");

        let snap = resolve_with_options_snap(&pool, "combo:c", None, None).expect("snap");
        let RoutingPlan::Combo { targets, .. } = snap else {
            panic!("expected combo plan");
        };
        assert!(
            targets.is_empty(),
            "cooled-down target must be filtered like list_targets"
        );
    }

    #[test]
    fn snapshot_detects_external_writer() {
        // A commit through a foreign connection — the sqlite3 CLI case —
        // cannot touch the writer hooks or epochs. The file-version probe
        // must still force a rebuild.
        let pool = fresh_pool();
        {
            let conn = pool.writer();
            seed_provider(&conn, "openrouter");
            seed_healthy_account(&conn, "openrouter");
        }

        let first = resolve_with_options_snap(&pool, "ext-model", None, None).expect("snap");
        assert!(matches!(first, RoutingPlan::NotFound { .. }));

        // Foreign handle on the same file: no hooks, no epoch bumps.
        let external = rusqlite::Connection::open(pool.path()).expect("external conn");
        external
            .execute_batch("PRAGMA busy_timeout = 5000;")
            .expect("busy timeout");
        external
            .execute(
                "INSERT INTO models(provider_id, model_id, target_format) \
                 VALUES ('openrouter', 'ext-model', 'openai')",
                [],
            )
            .expect("external insert");
        drop(external);

        let second = resolve_with_options_snap(&pool, "ext-model", None, None).expect("snap");
        assert!(
            matches!(second, RoutingPlan::Combo { .. }),
            "external commit must invalidate the snapshot"
        );
    }
}
