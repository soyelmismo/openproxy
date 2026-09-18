-- 000079_fix_minimax_coding_base_url.sql
-- Fix MiniMax Coding provider base URL to use the managed agent endpoint
-- (https://agent.minimax.io) matching upstream MiniMax-AI/minimax-code managed-login contract.

UPDATE providers
SET base_url = 'https://agent.minimax.io'
WHERE id = 'minimax'
  AND (base_url LIKE '%api.minimax.io%' OR base_url = '');
