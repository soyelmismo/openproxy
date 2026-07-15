-- 000034_combo_context_window.sql
-- Add a `context_window` column to `combos` so the /v1/models endpoint
-- can report the effective context window for a combo (the minimum
-- across all its targets, including sub-combo targets recursively).
--
-- NULL means "auto-compute from targets". The operator can override
-- it via the dashboard to a fixed value if they want to cap it lower
-- than the natural minimum.
ALTER TABLE combos ADD COLUMN context_window INTEGER;
