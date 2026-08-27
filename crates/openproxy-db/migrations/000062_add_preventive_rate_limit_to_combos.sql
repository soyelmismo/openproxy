-- Add preventive_rate_limit column to combos table.
-- Default 0 (disabled). When 1 (enabled), target resolution actively predicts
-- and short-circuits targets that are about to hit upstream rate limits.
ALTER TABLE combos ADD COLUMN preventive_rate_limit INTEGER NOT NULL DEFAULT 0;
