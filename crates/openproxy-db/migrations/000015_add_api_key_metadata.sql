-- 000015_add_api_key_metadata.sql
-- Expand the existing `api_keys` table (created in 000001 with just
-- `id / key_hash / label / created_at`) and link `usage` rows to the
-- key that produced them.
--
-- Design notes:
-- * The `key_hash` column is already UNIQUE and stores the SHA-256 of
--   the plaintext. We add `key_prefix` (first 12 chars of plaintext)
--   so the dashboard can show "op_live_abc..." without storing the
--   secret.
-- * `scopes_json` defaults to `["chat"]`. The legacy 000001 row had
--   no scope, so the default is the least-privileged useful scope.
-- * `allowed_models_json` / `allowed_combos_json` are NULL by default
--   (= "all" — the absent allowlist means no restriction). Empty
--   arrays would mean "deny everything"; we never set those.
-- * `is_active` is a soft-disable flag (the user can re-enable). The
--   separate `revoked_at` timestamp is the audit stamp and is
--   non-nullable only when revoke() is called.
-- * `created_by` records the actor ("admin", "system", "openproxy-env")
--   for the audit trail. The spec on this column is loose, so we keep
--   it as a free-form TEXT.
-- * The FK on `usage.api_key_id` uses `ON DELETE SET NULL` so a hard
--   delete of an api_key preserves the historical usage rows but
--   drops the link (the `key_prefix` snapshot is still visible in
--   the row at the time of insert).
ALTER TABLE api_keys ADD COLUMN key_prefix TEXT;
ALTER TABLE api_keys ADD COLUMN last_used_at TEXT;
ALTER TABLE api_keys ADD COLUMN scopes_json TEXT NOT NULL DEFAULT '["chat"]';
ALTER TABLE api_keys ADD COLUMN allowed_models_json TEXT;
ALTER TABLE api_keys ADD COLUMN allowed_combos_json TEXT;
ALTER TABLE api_keys ADD COLUMN is_active INTEGER NOT NULL DEFAULT 1 CHECK (is_active IN (0, 1));
ALTER TABLE api_keys ADD COLUMN revoked_at TEXT;
ALTER TABLE api_keys ADD COLUMN expires_at TEXT;
ALTER TABLE api_keys ADD COLUMN created_by TEXT;

ALTER TABLE usage ADD COLUMN api_key_id INTEGER REFERENCES api_keys(id) ON DELETE SET NULL;
CREATE INDEX IF NOT EXISTS idx_usage_api_key ON usage(api_key_id, created_at);
