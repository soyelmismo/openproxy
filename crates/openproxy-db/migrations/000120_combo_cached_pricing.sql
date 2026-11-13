-- Cached-input rate for a combo's own list price (USD per 1M cached input
-- tokens).
--
-- NULL means "no explicit rate": billing keeps the historical default of 50%
-- of the combo's input rate. A value only exists alongside an explicit
-- input/output pair (see combos::update_combo_pricing), so a cached rate can
-- never be orphaned from the base price it discounts.
ALTER TABLE combos ADD COLUMN pricing_cached_per_1m REAL;
