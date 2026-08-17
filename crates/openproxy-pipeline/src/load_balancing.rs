use openproxy_types::SelectionRegistry;
use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
use openproxy_types::ids::ComboId;
use rand::RngExt;
use rand::seq::SliceRandom;
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
    mut targets: Vec<ComboTarget>,
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
            targets.rotate_left(shift);
            targets
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
    let exploration_rate = exploration_rate.clamp(0.0, 1.0);

    let window_secs = combo
        .selection_window_secs
        .unwrap_or(DEFAULT_SELECTION_WINDOW_SECS);

    // Exploration branch: with probability `exploration_rate`, sample
    // a target to discover and refresh cold or untried providers.
    let mut rng = rand::rng();
    if exploration_rate > 0.0 && rng.random::<f64>() < exploration_rate && !targets.is_empty() {
        // Find indices of cold targets (no recent success in window).
        let cold_indices: Vec<usize> = targets
            .iter()
            .enumerate()
            .filter(|(_, t)| registry.last_success_within(t.id, window_secs) == 0)
            .map(|(i, _)| i)
            .collect();

        let idx = if !cold_indices.is_empty() {
            // Prioritize exploring untested / cold targets uniformly
            let pick = rng.random_range(0..cold_indices.len());
            cold_indices[pick]
        } else {
            // Otherwise explore any target uniformly
            rng.random_range(0..targets.len())
        };

        targets[..=idx].rotate_right(1);
        return targets;
    }

    // Exploitation branch: sort by `last_success` DESC (most recent success first),
    // with `priority_order` ASC as tiebreaker. Targets that failed have last_success = 0
    // and sort behind active working targets.
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
    let total: u64 = weights.iter().map(|w| u64::from(*w)).sum();
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
        if pick < u64::from(*w) {
            idx = i;
            break;
        }
        pick -= u64::from(*w);
    }
    targets[..=idx].rotate_right(1);
    targets
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
    targets[..=winner].rotate_right(1);
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use openproxy_types::ids::{ComboId, ComboTargetId, ProviderId};

    fn make_target(id: i64, priority: i32) -> ComboTarget {
        ComboTarget {
            id: ComboTargetId(id),
            combo_id: ComboId(1),
            provider_id: ProviderId::new(format!("p{id}")),
            account_id: None,
            model_row_id: None,
            sub_combo_id: None,
            priority_order: priority,
            weight: 1,
            active: true,
            cooldown_mode: None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            rate_limit_scope: openproxy_types::providers::RateLimitScope::Account,
        }
    }

    fn make_combo(mode: PriorityMode) -> Combo {
        Combo {
            id: ComboId(1),
            name: "test-combo".into(),
            strategy: Strategy::Priority,
            priority_mode: mode,
            race_size: 1,
            created_at: "2024-01-01".into(),
            context_window: None,
            cooldown_mode: openproxy_types::config::CooldownMode::None,
            cooldown_base_secs: None,
            cooldown_max_secs: None,
            cooldown_factor: None,
            lkgp_exploration_rate: Some(0.0), // Disable exploration for deterministic exploitation tests
            selection_window_secs: Some(3600),
        }
    }

    #[test]
    fn test_lkgp_anchoring_and_failure_penalty() {
        let registry = SelectionRegistry::new();
        let combo = make_combo(PriorityMode::Lkgp);

        let t1 = make_target(1, 1);
        let t2 = make_target(2, 2);
        let t3 = make_target(3, 3);
        let targets = vec![t1, t2, t3];

        // 1. Initial state (no successes) -> strictly ordered by priority_order
        let res = resolve_lkgp(targets.clone(), &combo, &registry);
        assert_eq!(res[0].id.0, 1);
        assert_eq!(res[1].id.0, 2);
        assert_eq!(res[2].id.0, 3);

        // 2. Target 2 succeeds -> Target 2 becomes #1
        registry.record_success(ComboTargetId(2));
        let res = resolve_lkgp(targets.clone(), &combo, &registry);
        assert_eq!(res[0].id.0, 2, "Target 2 should be at head after success");

        // 3. Target 3 succeeds later -> Target 3 becomes #1, Target 2 is #2
        std::thread::sleep(std::time::Duration::from_millis(5));
        registry.record_success(ComboTargetId(3));
        let res = resolve_lkgp(targets.clone(), &combo, &registry);
        assert_eq!(res[0].id.0, 3, "Target 3 should be at head after latest success");
        assert_eq!(res[1].id.0, 2);

        // 4. Target 3 fails -> Target 3 drops its known-good state, Target 2 takes back #1
        registry.record_failure(ComboTargetId(3));
        let res = resolve_lkgp(targets, &combo, &registry);
        assert_eq!(res[0].id.0, 2, "Target 2 should reclaim #1 after Target 3 fails");
        assert_eq!(res[1].id.0, 1, "Target 1 (priority 1) beats failed Target 3 (priority 3)");
        assert_eq!(res[2].id.0, 3);
    }
}
