# LKGP and Exponential Cooldown

## Goal

Add multiple priority modes to combos so operators can choose how targets are selected and how cooldowns grow. Currently only `Priority`, `RoundRobin`, and `Shuffle` exist, and cooldowns are flat (always `cooldown_secs`).

## New Priority Modes

Extends the existing `Strategy` enum with a new `priority_mode` column on `combos` that selects the algorithm used to order targets at request time.

### Modes

| Mode | Behavior | Parameters |
|------|----------|------------|
| `strict` (default, NULL) | Current `priority_order` walk — targets tried in manual order. | none |
| `lkgp` (Least Known Good Provider) | Prefer the target with the most recent successful request. Falls back to `priority_order` for ties or never-tried targets. | `exploration_rate` (0.0–1.0, default 0.1) — probability of trying a random target instead of the best, to explore alternatives. |
| `weighted` | Weighted random selection — each target's probability is proportional to its `weight` column. | `weight` per target (default 1). |
| `round_robin` (existing) | Rotate through targets. Already exists as `Strategy::RoundRobin`. | none |
| `shuffle` (existing) | Random shuffle on each request. Already exists as `Strategy::Shuffle`. | none |
| `least_used` | Prefer the target with the fewest total requests in the recent window. | `window_secs` (default 3600). |
| `p2c` (Power of Two Choices) | Pick two random targets, choose the one with fewer recent failures. | `window_secs` (default 3600). |

### Selection algorithm

`resolve_target_order` is extended to accept the `priority_mode` and dispatch to the appropriate algorithm. All algorithms return an ordered `Vec<ComboTarget>` — the pipeline's existing sequential walk (or race) is unchanged.

For `lkgp` and `least_used`, an in-memory registry tracks recent success/failure counts per target. This mirrors the existing `rr_counters: Arc<Mutex<HashMap<ComboId, u64>>>` pattern — single-instance, lost on restart.

## Exponential Cooldown

### Current behavior

`cooldown::record_failure` always sets `cooldown_until = now + cooldown_secs` (flat, global config). `failure_count` increments but doesn't affect the duration.

### New behavior

Add a `cooldown_mode` column on `combos`:

| Mode | Behavior |
|------|----------|
| `flat` (default, NULL) | Current: `cooldown_until = now + base_secs` |
| `exponential` | `cooldown_until = now + min(base_secs * factor^(failure_count-1), max_secs)` |

Per-combo overrides (all nullable, NULL = use global config):
- `cooldown_base_secs` (default from `[cooldown] cooldown_secs`, currently 60)
- `cooldown_max_secs` (default 3600)
- `cooldown_factor` (default 2)

The `record_failure` function is extended to accept the mode + params. The existing `record_failure(target_id, reason, cooldown_secs)` becomes a wrapper that calls the new function with `mode=Flat`.

## Schema

Migration `000035_combo_priority_modes.sql`:

```sql
ALTER TABLE combos ADD COLUMN priority_mode TEXT;
ALTER TABLE combos ADD COLUMN cooldown_mode TEXT;
ALTER TABLE combos ADD COLUMN cooldown_base_secs INTEGER;
ALTER TABLE combos ADD COLUMN cooldown_max_secs INTEGER;
ALTER TABLE combos ADD COLUMN cooldown_factor INTEGER;
ALTER TABLE combos ADD COLUMN lkgp_exploration_rate REAL;
ALTER TABLE combos ADD COLUMN selection_window_secs INTEGER;

ALTER TABLE combo_targets ADD COLUMN weight INTEGER NOT NULL DEFAULT 1;
```

All nullable columns default to NULL → existing combos get legacy behavior.

## API

### Create combo

`POST /admin/combos` accepts the new optional fields:
```json
{
  "name": "my-combo",
  "strategy": "priority",
  "race_size": 1,
  "priority_mode": "lkgp",
  "cooldown_mode": "exponential",
  "cooldown_base_secs": 30,
  "cooldown_max_secs": 600,
  "cooldown_factor": 2,
  "lkgp_exploration_rate": 0.1,
  "selection_window_secs": 3600
}
```

### Update combo

`PATCH /admin/combos/:id` accepts the same fields for editing.

### Update target

`PATCH /admin/combos/:id/targets/:tid` accepts `weight` for the weighted mode.

## UI

### Combo detail view

Add a "Priority Mode" selector (dropdown) with tooltips explaining each mode. When a mode has parameters, show them in a collapsible section below the selector.

Add a "Cooldown" section with:
- Mode selector (Flat / Exponential)
- Base / Max / Factor inputs (only when Exponential is selected)

### Tooltips

Each priority mode and parameter gets a `<abbr title="...">` tooltip with an English explanation.
