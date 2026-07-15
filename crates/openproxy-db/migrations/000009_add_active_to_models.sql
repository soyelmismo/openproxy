-- 000009_add_active_to_models.sql
-- Adds a soft-disable flag to the `models` table. Rows default to active so
-- existing rows and freshly-discovered ones are visible by default; an admin
-- can flip the bit to hide a model from routing without losing the row.
ALTER TABLE models ADD COLUMN active INTEGER NOT NULL DEFAULT 1
  CHECK (active IN (0, 1));
