-- 000053_add_active_to_combo_targets.sql
-- Adds an active flag to combo_targets so models can be temporarily disabled
-- without being fully deleted from a combo.
ALTER TABLE combo_targets ADD COLUMN active INTEGER NOT NULL DEFAULT 1
  CHECK (active IN (0, 1));
