-- 000017_add_target_cooldowns.sql
-- Per-target cooldown registry. A target that fails with a retryable
-- error (5xx, 429, timeout, connection) is parked here for
-- `cooldown_secs` seconds; the pipeline filters rows with a
-- `cooldown_until > now` out of the eligible-targets set so the next
-- request to the same combo skips them. Survives restarts because the
-- state lives in the DB (the in-memory circuit breaker is per-process
-- only and resets on restart; this table persists).
--
-- The cooldown propagates naturally to sub-combo targets: sub-combo
-- flattening happens upstream of the cooldown check (see
-- `Pipeline::flatten_targets`), so each child target of a sub-combo
-- can independently enter cooldown. The "exhaust the sub-combo
-- before marking the parent" semantic is implemented by the flattening
-- step itself — a parent combo whose only sub-combo has 3 children
-- will keep trying until all 3 are in cooldown; only then does the
-- parent's NoHealthyTargets path engage.

CREATE TABLE target_cooldowns (
  combo_target_id  INTEGER PRIMARY KEY REFERENCES combo_targets(id) ON DELETE CASCADE,
  cooldown_until   TEXT NOT NULL,            -- ISO 8601 UTC (RFC 3339)
  reason           TEXT,                     -- last error that fired the cooldown
  failure_count    INTEGER NOT NULL DEFAULT 1,
  created_at       TEXT NOT NULL DEFAULT (datetime('now')),
  updated_at       TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_target_cooldowns_until ON target_cooldowns(cooldown_until);

-- Trigger to auto-update `updated_at` on every UPDATE. Cheap to keep
-- the column honest: a single statement per write, and the table is
-- tiny (one row per failed target).
CREATE TRIGGER target_cooldowns_touch_updated
AFTER UPDATE ON target_cooldowns
FOR EACH ROW
BEGIN
  UPDATE target_cooldowns SET updated_at = datetime('now') WHERE combo_target_id = OLD.combo_target_id;
END;
