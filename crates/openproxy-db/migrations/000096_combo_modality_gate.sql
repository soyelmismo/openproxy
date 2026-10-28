-- Per-combo override for the input-modality gate.
--
-- NULL  → inherit the global `[modality_routing] enabled` setting.
-- 0/1   → force the gate off/on for this combo's walk.
--
-- Stored as an integer rather than TEXT so the read path stays a plain
-- `Option<bool>` and no enum parsing is needed at row-mapping time.
ALTER TABLE combos ADD COLUMN modality_gate INTEGER;
