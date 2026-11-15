-- 000134_oauth_device_tickets_sealed.sql
--
-- Seal the OAuth Device Code flow's `device_code` at rest (audit H-4).
--
-- The `device_code` is a bearer secret: whoever holds it can complete the
-- device authorization (poll the upstream /token endpoint with it) and bind
-- the resulting provider credentials onto this server. It used to sit in
-- `oauth_device_tickets` as PLAINTEXT for its whole (<= 10 min) lifetime —
-- anyone with read access to the SQLite file (a leaked backup sidecar, a
-- copied data volume, a forensic image) could redeem in-flight codes.
--
-- New shape:
--   - `device_code_hash` (sha256 hex) is the ONLY thing the SQL layer ever
--     compares. Lookups by device_code hash the caller-supplied value, so
--     the plaintext is never a query operand and never needs to be stored.
--   - `device_code_encrypted` keeps the value RECOVERABLE with the
--     MasterKey (AES-256-GCM, AAD-bound to this column by the v1 blob
--     format), so an operator can still reveal what a pending ticket was.
--
-- Why DROP+CREATE instead of in-place re-encryption: tickets live at most
-- 10 minutes (HARD_TTL_SECS) and the table is ephemeral by design (never
-- exported by backups — it is on the export blacklist — and wiped by every
-- restore). Re-sealing the old rows would require the MasterKey inside a
-- SQL migration, which is not where keys belong. In-flight device flows
-- at upgrade time restart instead; that is the documented trade-off.
--
-- The rest of the schema (user_code, account_id, expires_at, created_at,
-- consumed_at and their indexes) is unchanged from 000027.

DROP INDEX IF EXISTS idx_oauth_device_tickets_device_code;
DROP TABLE oauth_device_tickets;

CREATE TABLE oauth_device_tickets (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    provider        TEXT NOT NULL,
    device_code_hash     TEXT NOT NULL UNIQUE, -- sha256(device_code), lookup key
    device_code_encrypted TEXT NOT NULL,       -- MasterKey-sealed (recoverable)
    user_code       TEXT NOT NULL,
    account_id      INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
    expires_at      TEXT NOT NULL,    -- RFC3339 UTC
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    consumed_at     TEXT              -- NULL until poll succeeds
);

-- Lookup by device_code hash (the primary access pattern on the
-- /device-poll path).
CREATE UNIQUE INDEX idx_oauth_device_tickets_device_code_hash
    ON oauth_device_tickets(device_code_hash);

-- Lookup by user_code (the dashboard may lose the device_code but
-- still know the user_code it just displayed).
CREATE INDEX idx_oauth_device_tickets_user_code
    ON oauth_device_tickets(user_code);

-- Cleanup sweep.
CREATE INDEX idx_oauth_device_tickets_expires_at
    ON oauth_device_tickets(expires_at);
