-- Receipt-bound anonymous delivery: legacy rows fail closed.
--
-- Orders created from now on persist `reveal_receipt_hash` at INSERT time and
-- only that receipt can reveal the minted key (see the credits delivery
-- module). Orders minted before the binding have NULL here; they are
-- intentionally NOT claimable by order code alone and cannot be backfilled —
-- the buyer's receipt was never recorded. Resolution is out of band (decrypt
-- with the master key, deliver to the buyer, then wipe) or a refund; never
-- expose a code-only reveal path.
--
-- Operator inventory of rows that need a manual decision:
--   SELECT id, order_code, api_key_id, paid_at FROM credit_orders
--    WHERE status = 'minted' AND reveal_receipt_hash IS NULL
--      AND minted_key_encrypted IS NOT NULL;
CREATE INDEX idx_credit_orders_legacy_unbound
    ON credit_orders(id)
    WHERE status = 'minted'
      AND reveal_receipt_hash IS NULL
      AND minted_key_encrypted IS NOT NULL;
