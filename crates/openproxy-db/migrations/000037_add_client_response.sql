-- 000037_add_client_response.sql
--
-- Adds a `client_response` boolean column to the `usage` table.
-- When true, this row's response was actually delivered to the HTTP
-- client (the winning attempt in a combo walk, or the final error).
-- When false, the row is an intermediate attempt that was retried
-- internally — its response never reached the client.
--
-- This lets the dashboard distinguish "this request succeeded and
-- the client got the response" from "this request failed internally
-- but the combo walk found another target that succeeded".
--
-- The column defaults to 0 (false) so existing rows — which are all
-- historical and can't be retroactively classified — show as
-- "intermediate" until new rows populate the flag. This is the
-- safe default: the operator's mental model is "old data didn't
-- track this, new data does".

ALTER TABLE usage ADD COLUMN client_response INTEGER NOT NULL DEFAULT 0;

-- Index for filtering "show only rows that reached the client" in
-- the dashboard. Partial index (WHERE client_response = 1) keeps it
-- small — only the winner rows are indexed.
CREATE INDEX idx_usage_client_response
  ON usage(client_response)
  WHERE client_response = 1;
