use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
use openproxy_types::ids::ComboId;
use openproxy_types::SelectionRegistry;
use rand::seq::SliceRandom;
use rand::RngExt;
use std::sync::Arc;

/// Default selection window (1 hour) when the combo's
/// `selection_window_secs` column is `NULL`. Matches the spec's
/// documented default.
pub const DEFAULT_SELECTION_WINDOW_SECS: u64 = 3600;

/// Default LKGP exploration rate (10%) when the combo's
/// `lkgp_exploration_rate` column is `NULL`. Matches the spec's
/// documented default.
pub const DEFAULT_LKGP_EXPLORATION_RATE: f64 = 0.1;

pub fn execute_load_balancing(
    targets: Vec<ComboTarget>,
    combo: &Combo,
    rr_counters: &Arc<parking_lot::Mutex<std::collections::HashMap<ComboId, u64>>>,
    selection_registry: &SelectionRegistry,
) -> Vec<ComboTarget> {
    if targets.len() <= 1 {
        return targets;
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
            rotated
        }
        Strategy::Shuffle => {
            let mut shuffled = targets;
            shuffled.shuffle(&mut rand::rng());
            shuffled
        }
        Strategy::Priority => {
            let window_secs = combo
                .selection_window_secs
                .unwrap_or(DEFAULT_SELECTION_WINDOW_SECS);
            match combo.priority_mode {
                PriorityMode::Strict => targets,
                PriorityMode::Lkgp => resolve_lkgp(targets, combo, selection_registry),
                PriorityMode::Weighted => resolve_weighted(targets),
                PriorityMode::LeastUsed => {
                    resolve_least_used(targets, window_secs, selection_registry)
                }
                PriorityMode::P2c => resolve_p2c(targets, window_secs, selection_registry),
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
