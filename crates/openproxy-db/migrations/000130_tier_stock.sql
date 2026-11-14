-- 000130: per-plan stock (seats) — Miguel, 2026-11-14.
-- `stock` = maximum seats SOLD of this plan (0 = unlimited). A seat is taken
-- by a live customer key on the tier; a pending anonymous mint order ALSO
-- reserves its seat until it settles (pays / expires / cancels), so two
-- buyers holding QRs at the same time cannot both take the last seat.
-- Renewing the plan a key already occupies takes no new seat (the key is one
-- of the taken ones). The checkout advertises "(X seats available)" and the
-- order gate rejects mints and plan changes into a full plan.
ALTER TABLE credit_tiers ADD COLUMN stock INTEGER NOT NULL DEFAULT 0;
CREATE INDEX idx_credit_orders_tier_pending ON credit_orders(tier_id) WHERE status = 'pending';
