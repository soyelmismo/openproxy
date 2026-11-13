-- Per-combo pricing.
--
-- A combo can carry its own list price (USD per 1M tokens). When set, every
-- request routed through the combo is costed with THIS price, regardless of
-- what its target models cost — several upstreams have no pricing at all, and
-- the combo is the product the client actually pays for. When both columns are
-- NULL the request keeps falling back to the target model's price.
ALTER TABLE combos ADD COLUMN pricing_input_per_1m REAL;
ALTER TABLE combos ADD COLUMN pricing_output_per_1m REAL;
