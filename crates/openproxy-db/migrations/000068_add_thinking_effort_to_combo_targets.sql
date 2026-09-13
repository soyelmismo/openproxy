-- 000068_add_thinking_effort_to_combo_targets.sql
-- Adds per-target thinking_effort override column to combo_targets.
-- NULL indicates passthrough mode (use client request / model default).
ALTER TABLE combo_targets ADD COLUMN thinking_effort TEXT;
