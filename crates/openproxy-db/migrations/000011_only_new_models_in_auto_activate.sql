-- 000011: conceptual marker for the auto-activation behavior change.
--
-- No schema change is needed: `models.discovered_at` already exists
-- (defaulting to `datetime('now')` on insert) and is refreshed on every
-- `upsert_many` conflict path. `apply_auto_activation` now restricts
-- its UPDATE to rows whose `discovered_at` is within the last 60s,
-- which exactly matches the rows touched by the most recent refresh
-- and preserves the operator's hand-set `active` bits on older rows.
--
-- This file is kept on disk so the migrations list has a 1:1 entry for
-- every behavior change, even ones that are pure code modifications.

SELECT 1;
