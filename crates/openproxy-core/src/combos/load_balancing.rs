use super::*;
use crate::ids::*;
/// In-memory registry that tracks per-target recent success and
/// request counts for the LKGP / least_used / p2c priority modes.
///
/// Mirrors the existing `rr_counters: Arc<Mutex<HashMap<ComboId,
/// u64>>>` pattern: single-instance, in-memory, lost on restart.
/// Multi-instance deployments are out of scope for the MVP (same as
/// the round-robin counter).
///
/// Two maps are kept:
///
/// - `last_success`: `target_id → epoch-ms of the most recent
///   successful request`. Used by `Lkgp` to prefer the target whose
///   last success is the newest. A target that has never succeeded
///   (or whose entry was evicted on restart) is treated as
///   "infinitely old" and falls back to the `priority_order`
///   tiebreaker.
///
/// - `request_counts`: `target_id → total requests in the window`.
///   Used by `LeastUsed` to prefer the target with the fewest
///   recent requests and by `P2c` to break ties between the two
///   random picks.
///
/// The "window" is enforced lazily: `record_request` stamps the
/// current epoch-ms alongside the count, and readers compare against
/// `selection_window_secs` to decide whether to honor or ignore the
/// entry. Entries that fall outside the window are *not* eagerly
/// evicted — they're simply treated as zero on read. A periodic
/// sweeper could trim them, but the maps are bounded by the number
/// of distinct target ids in the DB (a few hundred at most) so the
/// memory cost of stale entries is negligible.
///
/// All methods are `&self` and lock internally, so the registry is
/// `Send + Sync` and cheap to share via `Arc<SelectionRegistry>`.
#[derive(Default)]
pub struct SelectionRegistry {
    /// `target_id → (last-success epoch-ms, request-count since
    /// that success)`. The two values are co-located in a single
    /// map so a single lock acquisition is enough to read both.
    /// The `last_success` field is `0` when the target has never
    /// succeeded (or its success was outside the window); the
    /// `request_count` field is monotonic within the window.
    inner: parking_lot::Mutex<std::collections::HashMap<i64, SelectionRegistryEntry>>,
}

#[derive(Debug, Clone, Copy, Default)]
struct SelectionRegistryEntry {
    /// Epoch-ms of the most recent successful request. `0` means
    /// "no success recorded".
    last_success_ms: u64,
    /// Total requests recorded since the entry was last reset.
    /// Used as a proxy for "recent usage" — the reader is
    /// responsible for honoring `selection_window_secs`.
    request_count: u64,
}

impl SelectionRegistry {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful request on `target_id`. Stamps the
    /// current epoch-ms as the `last_success` and increments the
    /// request count.
    pub fn record_success(&self, target_id: ComboTargetId) {
        let now = now_ms();
        let mut g = self.inner.lock();
        let e = g.entry(target_id.0).or_default();
        e.last_success_ms = now;
        e.request_count = e.request_count.saturating_add(1);
    }

    /// Record a request attempt (success or failure) on `target_id`.
    /// Used by `LeastUsed` / `P2c` to track recent load. Bumps the
    /// request count without touching `last_success_ms`.
    pub fn record_request(&self, target_id: ComboTargetId) {
        let mut g = self.inner.lock();
        let e = g.entry(target_id.0).or_default();
        e.request_count = e.request_count.saturating_add(1);
    }

    /// Snapshot the `last_success_ms` for `target_id`. Returns `0`
    /// when the target has no entry (never tried) or its entry is
    /// older than `window_secs` (treated as "no recent success").
    fn last_success_within(&self, target_id: ComboTargetId, window_secs: u64) -> u64 {
        let g = self.inner.lock();
        match g.get(&target_id.0) {
            Some(e) if e.last_success_ms > 0 => {
                let now = now_ms();
                let window_ms = window_secs.saturating_mul(1000);
                if now.saturating_sub(e.last_success_ms) <= window_ms {
                    e.last_success_ms
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    /// Snapshot the `request_count` for `target_id` within the
    /// window. Returns `0` when the target has no entry or its
    /// entry is older than the window (treated as "no recent
    /// load"). For the purposes of `LeastUsed` / `P2c`, a target
    /// with no recent activity is preferable to one that's been
    /// hammered.
    fn request_count_within(&self, target_id: ComboTargetId, window_secs: u64) -> u64 {
        let g = self.inner.lock();
        match g.get(&target_id.0) {
            Some(e) if e.request_count > 0 => {
                // We don't track the timestamp of the *last* request
                // separately (only the last *success*). The window
                // check here is best-effort: if `last_success_ms`
                // is within the window we honor the count; if not,
                // we treat the entry as stale. A target that has
                // been failing repeatedly will have `last_success_ms
                // == 0` (or stale) and read back as 0 here, which
                // is the right thing for `LeastUsed` (prefer it
                // less) and `P2c` (no signal).
                if e.last_success_ms == 0 {
                    // Never succeeded — but the request_count
                    // still reflects recent attempts. We surface
                    // it as-is so `LeastUsed` can see targets
                    // that are being retried often.
                    return e.request_count;
                }
                let now = now_ms();
                let window_ms = window_secs.saturating_mul(1000);
                if now.saturating_sub(e.last_success_ms) <= window_ms {
                    e.request_count
                } else {
                    0
                }
            }
            _ => 0,
        }
    }

    /// Evict entries whose `last_success_ms` is older than
    /// `max_age` AND whose `request_count` is zero or was last
    /// bumped outside the window. Used by a background sweep to
    /// prevent the registry from growing unbounded as combo
    /// targets are created and deleted over the process lifetime.
    ///
    /// Entries with `last_success_ms == 0` (never succeeded) are
    /// kept only if they were requested recently — a target that's
    /// been failing but is still being tried shouldn't be evicted
    /// mid-retry. We approximate "requested recently" by checking
    /// `last_success_ms` against the window (a target that hasn't
    /// succeeded in `max_age` AND has no recent success is either
    /// deleted or permanently broken — either way, evicting it is
    /// safe; the next `record_*` call re-creates it).
    ///
    /// Returns the number of entries evicted.
    pub fn prune_stale(&self, max_age: std::time::Duration) -> usize {
        let mut g = self.inner.lock();
        let now = now_ms();
        let cutoff = now.saturating_sub(max_age.as_millis() as u64);
        let before = g.len();
        g.retain(|_, e| {
            // Keep entries with a recent success.
            if e.last_success_ms > 0 && e.last_success_ms >= cutoff {
                return true;
            }
            // Keep entries that have never succeeded but were
            // requested recently (request_count > 0 but no success
            // yet — the target is being retried). We don't have a
            // "last request" timestamp, so we use the heuristic:
            // if `last_success_ms == 0` and `request_count > 0`,
            // keep it (it's an active failure case). If
            // `last_success_ms == 0` and `request_count == 0`,
            // it's a stale entry from a deleted target.
            if e.last_success_ms == 0 && e.request_count > 0 {
                return true;
            }
            false
        });
        before - g.len()
    }

    /// Current number of tracked targets. Diagnostic only.
    pub fn len(&self) -> usize {
        self.inner.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock().is_empty()
    }
}

/// Helper: current wall-clock epoch-ms.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Default selection window (1 hour) when the combo's
/// `selection_window_secs` column is `NULL`. Matches the spec's
/// documented default.
pub const DEFAULT_SELECTION_WINDOW_SECS: u64 = 3600;

/// Default LKGP exploration rate (10%) when the combo's
/// `lkgp_exploration_rate` column is `NULL`. Matches the spec's
/// documented default.
pub const DEFAULT_LKGP_EXPLORATION_RATE: f64 = 0.1;

/// Resolve the targets to actually use for a request, in execution
/// order, dispatching to the appropriate algorithm based on the
/// combo's `strategy` and `priority_mode`.
///
/// For [`Strategy::RoundRobin`] and [`Strategy::Shuffle`], the
/// `priority_mode` is ignored (the strategy already pins the order
/// — round-robin rotates by a per-combo counter, shuffle
/// randomizes on every call).
///
/// For [`Strategy::Priority`], the `priority_mode` selects the
/// algorithm:
///
/// - [`PriorityMode::Strict`]: walk `priority_order` ASC (the
///   legacy behavior; the same as the pre-migration-000035
///   `resolve_target_order` call).
/// - [`PriorityMode::Lkgp`]: sort by `last_success` (most recent
///   first), with `lkgp_exploration_rate` chance of picking a
///   random target instead. Ties and never-tried targets fall
///   back to `priority_order`.
/// - [`PriorityMode::Weighted`]: weighted random by the `weight`
///   column. The single picked target is placed first; the
///   remaining targets are appended in `priority_order` so the
///   pipeline's sequential walk still has a fallback if the
///   weighted pick is in cooldown.
/// - [`PriorityMode::LeastUsed`]: sort by `request_count` (fewest
///   first); ties broken by `priority_order`.
/// - [`PriorityMode::P2c`]: pick two random targets, choose the
///   one with fewer recent requests. The winner is placed first;
///   the rest are appended in `priority_order`.
///
/// All algorithms return the *full* `Vec<ComboTarget>` (just
/// reordered) — the pipeline's sequential walk + race logic is
/// unchanged. The `priority_order` column is always the final
/// tiebreaker so an operator's manual ordering is never silently
/// discarded.
pub fn resolve_target_order_with_mode(
    conn: &Connection,
    combo: &Combo,
    rr_counters: &Arc<parking_lot::Mutex<std::collections::HashMap<ComboId, u64>>>,
    selection_registry: &SelectionRegistry,
) -> Result<Vec<ComboTarget>> {
    let targets = list_targets(conn, combo.id)?;
    if targets.len() <= 1 {
        // 0 or 1 target — no algorithm has anything useful to do.
        // Skip the registry reads / RNG calls entirely.
        return Ok(targets);
    }

    match combo.strategy {
        Strategy::RoundRobin => {
            let n = targets.len();
            let shift = {
                let mut counters = rr_counters.lock();
                let counter = counters.entry(combo.id).or_insert(0);
                let s = (*counter % n as u64) as usize;
                *counter = counter.wrapping_add(1);
                s
            };
            let mut rotated = Vec::with_capacity(n);
            rotated.extend_from_slice(&targets[shift..]);
            rotated.extend_from_slice(&targets[..shift]);
            Ok(rotated)
        }
        Strategy::Shuffle => {
            let mut shuffled = targets;
            shuffled.shuffle(&mut rand::rng());
            Ok(shuffled)
        }
        Strategy::Priority => {
            let window_secs = combo
                .selection_window_secs
                .unwrap_or(DEFAULT_SELECTION_WINDOW_SECS);
            match combo.priority_mode {
                PriorityMode::Strict => Ok(targets),
                PriorityMode::Lkgp => Ok(resolve_lkgp(targets, combo, selection_registry)),
                PriorityMode::Weighted => Ok(resolve_weighted(targets)),
                PriorityMode::LeastUsed => {
                    Ok(resolve_least_used(targets, window_secs, selection_registry))
                }
                PriorityMode::P2c => Ok(resolve_p2c(targets, window_secs, selection_registry)),
            }
        }
    }
}

/// LKGP: prefer the target whose most recent success is the newest.
/// Ties (and never-tried targets, which read back as `0`) are
/// broken by `priority_order`. With probability
/// `lkgp_exploration_rate` we pick a random target as the head.
///
/// **Priority-aware exploration**: the random pick is NOT uniform —
/// it's weighted by `priority_order` so that targets the operator
/// positioned first (lower `priority_order`) have a higher chance of
/// being explored. This matches the user's intent: the first models
/// in the combo are there because they're preferred for speed or
/// intelligence, and the last ones are fallbacks that should get less
/// traffic. A uniform random exploration would ignore this signal.
///
/// The weighting is inverse-linear: the target at position 0 gets
/// weight `N`, position 1 gets `N-1`, ..., position N-1 gets `1`.
/// This gives a smooth decay — the first target is N× more likely
/// to be explored than the last, but the last still has a chance.
fn resolve_lkgp(
    mut targets: Vec<ComboTarget>,
    combo: &Combo,
    registry: &SelectionRegistry,
) -> Vec<ComboTarget> {
    let exploration_rate = combo
        .lkgp_exploration_rate
        .unwrap_or(DEFAULT_LKGP_EXPLORATION_RATE);
    // Clamp to [0.0, 1.0] defensively; the admin handler validates
    // on write, but a hand-edited row could still slip through.
    let exploration_rate = exploration_rate.clamp(0.0, 1.0);

    // Exploration branch: with probability `exploration_rate`, pick
    // a target weighted by its position (priority_order). Targets
    // earlier in the list (lower priority_order) get higher weight.
    let mut rng = rand::rng();
    if exploration_rate > 0.0 && rng.random::<f64>() < exploration_rate && !targets.is_empty() {
        // Sort by priority_order first so the position-based weights
        // are assigned correctly regardless of the input order.
        targets.sort_by_key(|t| t.priority_order);
        let n = targets.len() as u64;
        // Inverse-linear weights: position 0 → N, 1 → N-1, ..., N-1 → 1.
        // Total weight = N + (N-1) + ... + 1 = N*(N+1)/2.
        let total: u64 = n * (n + 1) / 2;
        let mut pick = rng.random_range(0..total);
        let mut idx = 0;
        for i in 0..targets.len() {
            // Weight for position i (0-indexed) = N - i.
            let w = n - i as u64;
            if pick < w {
                idx = i;
                break;
            }
            pick -= w;
        }
        let picked = targets.remove(idx);
        let mut out = Vec::with_capacity(targets.len() + 1);
        out.push(picked);
        out.extend(targets);
        return out;
    }

    // Exploitation branch: sort by `last_success` DESC, with
    // `priority_order` ASC as the tiebreaker. `last_success == 0`
    // (never tried) sorts last so a fresh target doesn't displace
    // a known-good one.
    let window_secs = combo
        .selection_window_secs
        .unwrap_or(DEFAULT_SELECTION_WINDOW_SECS);
    targets.sort_by(|a, b| {
        let la = registry.last_success_within(a.id, window_secs);
        let lb = registry.last_success_within(b.id, window_secs);
        lb.cmp(&la)
            .then_with(|| a.priority_order.cmp(&b.priority_order))
    });
    targets
}

/// Weighted random: each target's probability is proportional to
/// its `weight` column. We treat weights `<= 0` as `1` defensively
/// (the admin handler rejects `<= 0` on write, but a hand-edited
/// row could still slip through and a negative weight would
/// divide-by-zero the sum). The single picked target is moved to
/// the head; the rest stay in `priority_order`.
fn resolve_weighted(mut targets: Vec<ComboTarget>) -> Vec<ComboTarget> {
    if targets.is_empty() {
        return targets;
    }
    let weights: Vec<u32> = targets
        .iter()
        .map(|t| if t.weight <= 0 { 1 } else { t.weight as u32 })
        .collect();
    let total: u64 = weights.iter().map(|w| *w as u64).sum();
    if total == 0 {
        // All-zero weights (shouldn't happen given the `<= 0` → `1`
        // clamp above, but defense in depth). Fall back to strict
        // priority order.
        return targets;
    }
    let mut rng = rand::rng();
    let mut pick = rng.random_range(0..total);
    let mut idx = 0;
    for (i, w) in weights.iter().enumerate() {
        if pick < *w as u64 {
            idx = i;
            break;
        }
        pick -= *w as u64;
    }
    let picked = targets.remove(idx);
    let mut out = Vec::with_capacity(targets.len() + 1);
    out.push(picked);
    out.extend(targets);
    out
}

/// Least-used: sort by `request_count` ASC (fewest first). Ties
/// broken by `priority_order` ASC. A target with no recent
/// activity reads back as `0` and is preferred over one that's
/// been hammered — which is the point.
fn resolve_least_used(
    mut targets: Vec<ComboTarget>,
    window_secs: u64,
    registry: &SelectionRegistry,
) -> Vec<ComboTarget> {
    targets.sort_by(|a, b| {
        let ca = registry.request_count_within(a.id, window_secs);
        let cb = registry.request_count_within(b.id, window_secs);
        ca.cmp(&cb)
            .then_with(|| a.priority_order.cmp(&b.priority_order))
    });
    targets
}

/// P2C (Power of Two Choices): pick two random targets, choose
/// the one with fewer recent requests. The winner goes to the
/// head; the rest stay in `priority_order`. With fewer than two
/// targets the function is a no-op (the caller already short-
/// circuits on `len() <= 1`, but we defend here too).
fn resolve_p2c(
    mut targets: Vec<ComboTarget>,
    window_secs: u64,
    registry: &SelectionRegistry,
) -> Vec<ComboTarget> {
    if targets.len() < 2 {
        return targets;
    }
    let mut rng = rand::rng();
    let i = rng.random_range(0..targets.len());
    let mut j = rng.random_range(0..targets.len());
    if i == j {
        // Re-roll to guarantee two distinct picks when there are
        // at least two targets. Wrapping is fine because `len >= 2`.
        j = (j + 1) % targets.len();
    }
    let ci = registry.request_count_within(targets[i].id, window_secs);
    let cj = registry.request_count_within(targets[j].id, window_secs);
    let winner = if ci <= cj { i } else { j };
    let picked = targets.remove(winner);
    let mut out = Vec::with_capacity(targets.len() + 1);
    out.push(picked);
    out.extend(targets);
    out
}

/// Resolve the targets to actually use for a request, in execution order.
/// - priority: ordered by priority_order ASC.
/// - round_robin: rotates target order using a per-combo counter (in-memory, persisted across calls within the same process).
///
/// The counter is held in a global Mutex<HashMap<ComboId, u64>>.
/// On round_robin, the order is shifted by `counter[combo_id] % N` and the counter is incremented.
/// This is per-process; multi-instance deployments are out of scope (single-instance MVP).
///
/// Legacy entry point kept for callers that have a `combo_id` +
/// `strategy` in hand but not a full `Combo` struct (e.g. tests).
/// Delegates to [`resolve_target_order_with_mode`] with a synthetic
/// `Combo` whose `priority_mode = Strict` — i.e. the pre-migration-
/// 000035 behavior. Production code paths go through
/// [`resolve_target_order_with_mode`] directly so the combo-level
/// mode settings are honored.
pub fn resolve_target_order(
    conn: &Connection,
    combo_id: ComboId,
    strategy: Strategy,
    rr_counters: &Arc<parking_lot::Mutex<std::collections::HashMap<ComboId, u64>>>,
) -> Result<Vec<ComboTarget>> {
    // Build a minimal synthetic Combo with `Strict` priority mode
    // and `Flat` cooldown so the new dispatcher produces the legacy
    // behavior. The fields the dispatcher actually reads are `id`,
    // `strategy`, `priority_mode`, `lkgp_exploration_rate`, and
    // `selection_window_secs`; the rest are zeroed-out defaults.
    let combo = Combo {
        id: combo_id,
        name: String::new(),
        strategy,
        race_size: 1,
        created_at: String::new(),
        context_window: None,
        priority_mode: PriorityMode::Strict,
        cooldown_mode: CooldownMode::Flat,
        cooldown_base_secs: None,
        cooldown_max_secs: None,
        cooldown_factor: None,
        lkgp_exploration_rate: None,
        selection_window_secs: None,
    };
    // A throw-away registry is fine here: the Strict mode never
    // reads from it, so the per-call allocation cost is the only
    // overhead. Callers that want LKGP / LeastUsed / P2c must go
    // through `resolve_target_order_with_mode` with a shared
    // registry.
    let registry = SelectionRegistry::default();
    resolve_target_order_with_mode(conn, &combo, rr_counters, &registry)
}

