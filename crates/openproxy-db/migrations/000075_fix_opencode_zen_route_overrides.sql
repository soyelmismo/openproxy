-- 000075_fix_opencode_zen_route_overrides.sql
-- 000073 routed every OpenCode Zen/Go model by substring, which misroutes the
-- aliases whose wire format differs from the rest of their family:
--   * `union-alpha` is only served on the Anthropic Messages API.
--   * Zen exposes the paid MiniMax tiers plus the coder/code extras on the
--     OpenAI-compatible endpoint; only the `-free` tiers use Messages.
-- Must stay in sync with `classify_opencode_target_format`
-- (crates/openproxy-adapters/src/adapters/opencode_common.rs).

UPDATE models
SET target_format = 'anthropic'
WHERE provider_id IN ('opencode-zen', 'opencode-go')
  AND model_id = 'union-alpha';

UPDATE models
SET target_format = 'openai'
WHERE provider_id = 'opencode-zen'
  AND model_id IN (
    'minimax-m2.1',
    'minimax-m2.5',
    'minimax-m2.7',
    'minimax-m3',
    'qwen3-coder',
    'grok-code'
  );
