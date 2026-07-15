-- 000004_add_race_size_to_combos.sql
ALTER TABLE combos ADD COLUMN race_size INTEGER NOT NULL DEFAULT 1
  CHECK (race_size >= 1 AND race_size <= 8);
