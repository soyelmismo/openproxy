-- Per-combo purge reasoning flag for response sanitization.
--
-- When `sanitize_response` is enabled on a combo, `purge_reasoning` controls whether
-- internal reasoning metadata (reasoning_content, reasoning_details, thought, thinking)
-- is purged from the response or passed through.
-- NULL / 0 (default): reasoning is preserved (passed through).
-- 1: reasoning is stripped.
ALTER TABLE combos ADD COLUMN purge_reasoning INTEGER;
