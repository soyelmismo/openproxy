-- 000035: Add priority mode + exponential cooldown columns to combos.
--
-- See docs/specs/lkgp-and-exponential-cooldown.md for the full spec.
--
-- All new columns are nullable so existing combos get NULL → legacy
-- behavior (strict priority, flat cooldown). The pipeline interprets
-- NULL as the default mode.

-- Priority mode: controls how targets are ordered at request time.
-- NULL or 'strict' = current priority_order walk.
-- 'lkgp' = least-known-good-provider (prefer most-recent-success).
-- 'weighted' = weighted random by target weight.
-- 'least_used' = prefer fewest recent requests.
-- 'p2c' = power of two choices.
ALTER TABLE combos ADD COLUMN priority_mode TEXT;

-- Cooldown mode: controls how cooldown grows with failures.
-- NULL or 'flat' = current behavior (always base_secs).
-- 'exponential' = base * factor^(failure_count-1), capped at max.
ALTER TABLE combos ADD COLUMN cooldown_mode TEXT;
ALTER TABLE combos ADD COLUMN cooldown_base_secs INTEGER;
ALTER TABLE combos ADD COLUMN cooldown_max_secs INTEGER;
ALTER TABLE combos ADD COLUMN cooldown_factor INTEGER;

-- LKGP exploration rate: probability (0.0–1.0) of trying a random
-- target instead of the best-known one, to explore alternatives.
-- Default 0.1 (10% exploration).
ALTER TABLE combos ADD COLUMN lkgp_exploration_rate REAL;

-- Selection window for least_used / p2c modes: how far back to look
-- at usage data for the "least used" / "fewest failures" signal.
-- Default 3600 (1 hour).
ALTER TABLE combos ADD COLUMN selection_window_secs INTEGER;

-- Per-target weight for the 'weighted' priority mode. Default 1.
ALTER TABLE combo_targets ADD COLUMN weight INTEGER NOT NULL DEFAULT 1;
