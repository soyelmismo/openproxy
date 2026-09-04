use openproxy_types::SelectionRegistry;
use openproxy_types::combos::{Combo, ComboTarget, PriorityMode, Strategy};
use openproxy_types::ids::ComboId;
use rand::RngExt;
use rand::seq::SliceRandom;

/// Default selection window (1 hour) when the combo's
/// `selection_window_secs` column is `NULL`. Matches the spec's
/// documented default.
pub const DEFAULT_SELECTION_WINDOW_SECS: u64 = 3600;

/// Default LKGP exploration rate (10%) when the combo's
/// `lkgp_exploration_rate` column is `NULL`. Matches the spec's
/// documented default.
pub const DEFAULT_LKGP_EXPLORATION_RATE: f64 = 0.1;

fn execute_round_robin(
    mut targets: Vec<ComboTarget>,
    combo_id: ComboId,
    rr_counters: &std::sync::Arc<
        dashmap::DashMap<openproxy_types::ids::ComboId, std::sync::atomic::AtomicU64>,
    >,
) -> Vec<ComboTarget> {
    let n = targets.len();
    if n == 0 {
        return targets;
    }

    let val = if let Some(counter) = rr_counters.get(&combo_id) {
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    } else {
        let counter = rr_counters
            .entry(combo_id)
            .or_insert_with(|| std::sync::atomic::AtomicU64::new(0));
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    };

    let shift = (val % n as u64) as usize;
    targets.rotate_left(shift);
    targets
}

fn execute_priority_strategy(
    targets: Vec<ComboTarget>,
    combo: &Combo,
    selection_registry: &SelectionRegistry,
) -> Vec<ComboTarget> {
    let window_secs = combo
        .selection_window_secs
        .unwrap_or(DEFAULT_SELECTION_WINDOW_SECS);
    match combo.priority_mode {
        PriorityMode::Strict => targets,
        PriorityMode::Lkgp => resolve_lkgp(targets, combo, selection_registry),
        PriorityMode::Weighted => resolve_weighted(targets),
        PriorityMode::LeastUsed => resolve_least_used(targets, window_secs, selection_registry),
        PriorityMode::P2c => resolve_p2c(targets, window_secs, selection_registry),
    }
}

pub fn execute_load_balancing(
    targets: Vec<ComboTarget>,
    combo: &Combo,
    rr_counters: &std::sync::Arc<
        dashmap::DashMap<openproxy_types::ids::ComboId, std::sync::atomic::AtomicU64>,
    >,
    selection_registry: &SelectionRegistry,
) -> Vec<ComboTarget> {
    if targets.len() <= 1 {
        return targets;
    }

    match combo.strategy {
        Strategy::RoundRobin => execute_round_robin(targets, combo.id, rr_counters),
        Strategy::Shuffle => {
            let mut shuffled = targets;
            shuffled.shuffle(&mut rand::rng());
            shuffled
        }
        Strategy::Priority => execute_priority_strategy(targets, combo, selection_registry),
    }
}

fn sample_lkgp_exploration_target(
    targets: &[ComboTarget],
    window_secs: u64,
    registry: &SelectionRegistry,
    rng: &mut impl rand::Rng,
) -> usize {
    let untried_indices: Vec<usize> = targets
        .iter()
        .enumerate()
        .filter(|(_, t)| registry.request_count_within(t.id, window_secs) == 0)
        .map(|(i, _)| i)
        .collect();

    if !untried_indices.is_empty() {
        let pick = rng.random_range(0..untried_indices.len());
        return untried_indices[pick];
    }

    let cold_indices: Vec<usize> = targets
        .iter()
        .enumerate()
        .filter(|(_, t)| registry.last_success_within(t.id, window_secs) == 0)
        .map(|(i, _)| i)
        .collect();

    if !cold_indices.is_empty() {
        let pick = rng.random_range(0..cold_indices.len());
        cold_indices[pick]
    } else {
        rng.random_range(0..targets.len())
    }
}

fn compare_lkgp_targets(
    a: &ComboTarget,
    b: &ComboTarget,
    window_secs: u64,
    registry: &SelectionRegistry,
) -> std::cmp::Ordering {
    let la = registry.last_success_within(a.id, window_secs);
    let lb = registry.last_success_within(b.id, window_secs);

    if la != lb {
        return lb.cmp(&la);
    }

    let fa = registry.last_activity_within(a.id, window_secs);
    let fb = registry.last_activity_within(b.id, window_secs);
    let a_has_failed = fa > 0;
    let b_has_failed = fb > 0;

    match (a_has_failed, b_has_failed) {
        (false, true) => std::cmp::Ordering::Less,
        (true, false) => std::cmp::Ordering::Greater,
        _ => a.priority_order.cmp(&b.priority_order),
    }
}

/// LKGP: prefer the target whose most recent success is the newest.
fn resolve_lkgp(
    mut targets: Vec<ComboTarget>,
    combo: &Combo,
    registry: &SelectionRegistry,
) -> Vec<ComboTarget> {
    let exploration_rate = combo
        .lkgp_exploration_rate
        .unwrap_or(DEFAULT_LKGP_EXPLORATION_RATE)
        .clamp(0.0, 1.0);

    let window_secs = combo
        .selection_window_secs
        .unwrap_or(DEFAULT_SELECTION_WINDOW_SECS);

    let mut rng = rand::rng();
    if exploration_rate > 0.0 && rng.random::<f64>() < exploration_rate && !targets.is_empty() {
        let idx = sample_lkgp_exploration_target(&targets, window_secs, registry, &mut rng);
        targets[..=idx].rotate_right(1);
        return targets;
    }

    targets.sort_by(|a, b| compare_lkgp_targets(a, b, window_secs, registry));
    targets
}

fn pick_weighted_index(weights: &[u32], mut pick: u64) -> usize {
    for (i, &w) in weights.iter().enumerate() {
        let weight = u64::from(w);
        if pick < weight {
            return i;
        }
        pick -= weight;
    }
    0
}

fn target_effective_weight(t: &ComboTarget) -> u32 {
    if t.weight <= 0 { 1 } else { t.weight as u32 }
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
    let weights: Vec<u32> = targets.iter().map(target_effective_weight).collect();
    let total: u64 = weights.iter().map(|w| u64::from(*w)).sum();
    if total == 0 {
        // All-zero weights (shouldn't happen given the `<= 0` → `1`
        // clamp above, but defense in depth). Fall back to strict
        // priority order.
        return targets;
    }
    let pick = rand::rng().random_range(0..total);
    let idx = pick_weighted_index(&weights, pick);
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
            preventive_rate_limit: false,
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
        assert_eq!(
            res[0].id.0, 3,
            "Target 3 should be at head after latest success"
        );
        assert_eq!(res[1].id.0, 2);

        // 4. Target 3 fails -> Target 3 drops its known-good state, Target 2 takes back #1
        registry.record_failure(ComboTargetId(3));
        let res = resolve_lkgp(targets, &combo, &registry);
        assert_eq!(
            res[0].id.0, 2,
            "Target 2 should reclaim #1 after Target 3 fails"
        );
        assert_eq!(
            res[1].id.0, 1,
            "Target 1 (priority 1) beats failed Target 3 (priority 3)"
        );
        assert_eq!(res[2].id.0, 3);
    }

    #[test]
    fn test_lkgp_untried_targets_beat_failed_favorites() {
        let registry = SelectionRegistry::new();
        let combo = make_combo(PriorityMode::Lkgp);

        let t1 = make_target(1, 10); // Favorite that failed
        let t2 = make_target(2, 20); // Untried target (e.g. fireworks)
        let t3 = make_target(3, 30); // Another favorite that failed

        registry.record_failure(ComboTargetId(1));
        registry.record_failure(ComboTargetId(3));

        let targets = vec![t1, t2, t3];
        let res = resolve_lkgp(targets, &combo, &registry);

        // Untried Target 2 (priority 20) MUST beat failing targets 1 and 3
        assert_eq!(
            res[0].id.0, 2,
            "Untried target 2 must be tried before failed targets 1 and 3"
        );
        assert_eq!(
            res[1].id.0, 1,
            "Target 1 (priority 10) before Target 3 (priority 30)"
        );
        assert_eq!(res[2].id.0, 3);
    }

    #[test]
    fn test_lkgp_exploration_picks_untried_targets() {
        let registry = SelectionRegistry::new();
        let mut combo = make_combo(PriorityMode::Lkgp);
        combo.lkgp_exploration_rate = Some(1.0); // 100% exploration

        let t1 = make_target(1, 1); // Tried
        let t2 = make_target(2, 2); // Untried
        let t3 = make_target(3, 3); // Tried

        registry.record_request(ComboTargetId(1));
        registry.record_request(ComboTargetId(3));

        let targets = vec![t1, t2, t3];
        let res = resolve_lkgp(targets, &combo, &registry);

        assert_eq!(
            res[0].id.0, 2,
            "100% exploration must rotate untried Target 2 to head"
        );
    }
}
