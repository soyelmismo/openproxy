-- 000065_antigravity_project_id_wire_format.sql
--
-- One-shot in-place normalization: rows written by the pre-spec
-- `update_antigravity_project_id` (camelCase `projectId`) are
-- rewritten to snake_case `project_id`. After this migration runs,
-- every `oauth_provider_specific` row for Antigravity accounts uses
-- `project_id` exclusively.
--
-- Idempotency:
--   - The `WHERE json_extract(...) IS NOT NULL` guard skips rows that
--     never had `projectId` (only had `project_id` already, or had no
--     key at all).
--   - The `AND json_extract(..., '$.project_id') IS NULL` guard
--     prevents overwriting an existing snake_case value with the
--     camelCase one (idempotency + safety: do not lose data).
--   - Running the migration twice is safe: the second run finds no
--     rows satisfying both conditions and exits with `changed = 0`.
--
-- Scope:
--   - Restricted to `provider_id = 'antigravity'` because the
--     camelCase `projectId` key is only ever written by
--     `update_antigravity_project_id`, which is only reachable for
--     Antigravity accounts.
--   - Non-Antigravity providers (kiro, codex) never write `projectId`;
--     their `oauth_provider_specific` payloads have unrelated schemas.

UPDATE accounts
SET oauth_provider_specific = json_set(
    oauth_provider_specific,
    '$.project_id',
    json_extract(oauth_provider_specific, '$.projectId')
)
WHERE provider_id = 'antigravity'
  AND json_extract(oauth_provider_specific, '$.projectId') IS NOT NULL
  AND json_extract(oauth_provider_specific, '$.project_id') IS NULL;