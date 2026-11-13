-- Cached-input rate for a combo's own list price (USD per 1M cached input
-- tokens).
--
-- NULL means "no explicit rate": cache reads bill at the combo's input rate,
-- in dollars, with no percentage derived from it. A value only exists
-- alongside an explicit input/output pair (see combos::update_combo_pricing),
-- so a cached rate can never be orphaned from the base price it belongs to.
ALTER TABLE combos ADD COLUMN pricing_cached_per_1m REAL;
