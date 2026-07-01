-- 000038_add_estimated_tokens.sql
--
-- Adds columns to track whether token counts are real (reported by
-- the upstream) or estimated (heuristic from char count). This lets
-- the dashboard distinguish "the upstream said 1000 tokens" from
-- "we estimated 1000 tokens because the upstream didn't report usage".
--
-- Both columns default to 0 (not estimated) so existing rows are
-- unaffected — they all carry real upstream counts (or NULL).

ALTER TABLE usage ADD COLUMN prompt_tokens_estimated INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage ADD COLUMN completion_tokens_estimated INTEGER NOT NULL DEFAULT 0;
