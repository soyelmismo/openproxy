-- 000131: minted-key delivery TTL.
--
-- `minted_key_encrypted` (000111) kept the key re-revealable for ever until
-- the buyer acknowledged. A leaked recovery link (?receipt=) could therefore
-- re-show the plaintext weeks later when the buyer never completed the reveal
-- (closed the tab, storage failure — the ack is the only eraser today).
-- A mint timestamp bounds that exposure: a background sweep nulls the blob
-- after the TTL, after which only the order's metadata stays readable.
--
-- Legacy rows (minted before this column existed) get `now`: they keep their
-- full TTL window instead of being swept immediately.

ALTER TABLE credit_orders ADD COLUMN minted_at TEXT;

UPDATE credit_orders
   SET minted_at = datetime('now')
 WHERE status = 'minted'
   AND minted_key_encrypted IS NOT NULL
   AND minted_at IS NULL;

CREATE INDEX idx_credit_orders_minted_at
    ON credit_orders(minted_at)
 WHERE minted_key_encrypted IS NOT NULL;
