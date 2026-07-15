-- 000027_oauth_device_tickets.sql
-- LOW fix (#12): persist OAuth Device Code flow tickets in the DB so
-- they survive a server restart and so the dashboard doesn't have to
-- round-trip the `device_code` through its own session storage.
--
-- Background (see docs/pending/10-oauth-ticket.md):
--   The Device Code flow has two phases: (1) the server POSTs to the
--   upstream's `/device/code` endpoint and gets back
--   `{device_code, user_code, verification_uri, expires_in, interval}`;
--   (2) the server polls the upstream's `/token` endpoint with the
--   `device_code` until the user authorizes (or the code expires).
--   Before this migration, the server only held the `device_code` in
--   the HTTP response payload — the dashboard would echo it back in
--   the poll request. A page refresh, server restart, or
--   dashboard-side cache eviction between the two phases silently
--   aborted the flow with "device_code lost" — opaque to the
--   operator.
--
-- Schema:
--   - `provider` is the OAuth provider id (e.g. `kiro`, `antigravity`).
--     Text, not FK, because providers are a separate concern (the
--     provider list is loaded from the adapter registry, not the DB).
--   - `device_code` is the upstream-issued code (high-entropy random).
--     Stored as TEXT; SQLite has no native binary blob type.
--   - `user_code` is the human-readable short code the operator types
--     into the verification_uri. Indexed so the dashboard can
--     look up its own ticket by `user_code` if the device_code is lost.
--   - `account_id` is nullable: a fresh flow with no chosen account
--     starts with NULL and gets filled in when /device-poll succeeds.
--   - `expires_at` is RFC3339 UTC; we use the same string-comparison
--     pattern the rest of the codebase uses for token expiry, with
--     the fix from PR #15 (chrono::DateTime::parse_from_rfc3339) in
--     Rust. Wall-clock — `expires_at - now()` is the remaining
--     lifetime.
--   - `created_at` lets us enforce a hard upper bound on ticket age
--     (10 min, regardless of what the upstream said) so a leaked or
--     replayed ticket doesn't outlive the upstream's TTL by much.
--   - `consumed_at` is set when /device-poll returns success; this
--     makes single-use enforcement trivial (WHERE consumed_at IS NULL
--     in the UPDATE). NULL means still pending.
--
-- Cleanup: a background task in `oauth::start_ticket_cleanup` (added
-- in the same commit) deletes tickets older than 10 minutes every 5
-- minutes. The `idx_oauth_device_tickets_expires_at` index keeps that
-- sweep a single bounded scan.

CREATE TABLE oauth_device_tickets (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    provider        TEXT NOT NULL,
    device_code     TEXT NOT NULL UNIQUE,
    user_code       TEXT NOT NULL,
    account_id      INTEGER REFERENCES accounts(id) ON DELETE SET NULL,
    expires_at      TEXT NOT NULL,    -- RFC3339 UTC
    created_at      TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ','now')),
    consumed_at     TEXT              -- NULL until poll succeeds
);

-- Lookup by device_code (the primary access pattern on the
-- /device-poll path).
CREATE UNIQUE INDEX idx_oauth_device_tickets_device_code
    ON oauth_device_tickets(device_code);

-- Lookup by user_code (the dashboard may lose the device_code but
-- still know the user_code it just displayed).
CREATE INDEX idx_oauth_device_tickets_user_code
    ON oauth_device_tickets(user_code);

-- Cleanup sweep.
CREATE INDEX idx_oauth_device_tickets_expires_at
    ON oauth_device_tickets(expires_at);
