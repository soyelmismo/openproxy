-- 000094_combo_max_output_tokens.sql
-- Add a `max_output_tokens` column to `combos` so the /v1/models endpoint
-- and formatting pipeline can report and enforce the effective max output tokens
-- for a combo (the minimum across all its active targets, including sub-combo
-- targets recursively).
--
-- NULL means "auto-compute from targets". The operator can override
-- it via the dashboard/API to a fixed value if they want to explicitly set/cap it.
ALTER TABLE combos ADD COLUMN max_output_tokens INTEGER;
