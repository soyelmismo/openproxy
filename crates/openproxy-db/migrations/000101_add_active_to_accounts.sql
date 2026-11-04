-- 000101_add_active_to_accounts.sql
-- Adds an active flag (1=active, 0=paused/deactivated) to the accounts table.
-- When active=0, the account is ignored for request routing and combo target resolution,
-- but continues to participate normally in OAuth token refreshes and model discovery.
ALTER TABLE accounts ADD COLUMN active INTEGER NOT NULL DEFAULT 1
  CHECK (active IN (0, 1));
