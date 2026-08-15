-- 000057_add_cooldown_to_combo_targets.sql
-- Adds per-target cooldown override columns to combo_targets.
-- NULL values inherit from the parent combo.
ALTER TABLE combo_targets ADD COLUMN cooldown_mode TEXT;
ALTER TABLE combo_targets ADD COLUMN cooldown_base_secs INTEGER;
ALTER TABLE combo_targets ADD COLUMN cooldown_max_secs INTEGER;
ALTER TABLE combo_targets ADD COLUMN cooldown_factor INTEGER;
