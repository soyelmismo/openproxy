-- Delivery receipt supports retrying a lost reveal response until the buyer
-- acknowledges durable receipt. No plaintext is stored.
ALTER TABLE credit_orders ADD COLUMN reveal_receipt_hash TEXT;

-- Historical anonymous settlements omitted the key link on the order.
UPDATE credit_orders SET api_key_id = (
    SELECT api_key_id FROM credit_transactions t
    WHERE t.order_id = credit_orders.id AND t.kind = 'topup'
) WHERE status = 'minted' AND api_key_id IS NULL
  AND (SELECT COUNT(*) FROM credit_transactions t
       WHERE t.order_id = credit_orders.id AND t.kind = 'topup') = 1;

-- Paid orders must not reserve an amount forever. The allocator reserves the
-- complete payment + confirmation window transactionally, including expired
-- and already-paid orders; matching also checks the on-chain block timestamp.
DROP INDEX idx_credit_orders_live_amount;
CREATE UNIQUE INDEX idx_credit_orders_live_amount
    ON credit_orders(amount_units) WHERE status = 'pending';
CREATE INDEX idx_credit_orders_amount_window
    ON credit_orders(amount_units, created_at, expires_at);

-- Mark terminal/unmatched deposits explicitly for operator investigation.
ALTER TABLE matched_payments ADD COLUMN order_id INTEGER;
ALTER TABLE matched_payments ADD COLUMN outcome TEXT NOT NULL DEFAULT 'legacy';

-- A usage row is debited exactly once, independently of winner callbacks.
-- Historical billable rows must not be charged again by a delayed callback.
ALTER TABLE usage ADD COLUMN credits_debited INTEGER NOT NULL DEFAULT 0;
UPDATE usage SET credits_debited = 1 WHERE client_response = 1;
