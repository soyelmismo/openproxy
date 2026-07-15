-- 000002_add_timing_to_usage.sql
ALTER TABLE usage ADD COLUMN connect_ms INTEGER;
ALTER TABLE usage ADD COLUMN ttft_ms INTEGER;
ALTER TABLE usage ADD COLUMN total_ms INTEGER NOT NULL DEFAULT 0;
ALTER TABLE usage ADD COLUMN tokens_per_sec REAL;
