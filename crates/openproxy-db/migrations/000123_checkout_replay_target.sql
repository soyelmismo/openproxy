-- Delivery changes api_key_id after an anonymous mint. Preserve the ORIGINAL
-- purchase target so a retry cannot become a renewal or create a second order.
-- No FK: this is historical identity, retained even after key deletion.
ALTER TABLE credit_orders ADD COLUMN checkout_key_id INTEGER;
UPDATE credit_orders SET checkout_key_id = api_key_id WHERE status != 'minted';
CREATE INDEX idx_credit_orders_receipt ON credit_orders(reveal_receipt_hash)
    WHERE reveal_receipt_hash IS NOT NULL;
