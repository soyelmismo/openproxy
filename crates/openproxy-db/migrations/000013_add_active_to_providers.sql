-- 000013_add_active_to_providers.sql
-- Adds a soft-disable flag to the `providers` table. Rows default to active
-- (1) so existing rows stay routable and freshly-seeded built-ins come up
-- active without any extra work. A deactivated provider stays in the DB
-- (so its accounts and models are preserved) but stops being usable in
-- combos; the combo-targets query joins on this flag and skips inactive
-- providers. The provider can be reactivated later by flipping the bit
-- back to 1.
ALTER TABLE providers ADD COLUMN active INTEGER NOT NULL DEFAULT 1
  CHECK (active IN (0, 1));
