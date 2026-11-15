-- 000087_provider_prune_models.sql
-- Add prune_models column to providers table:
-- 1: default behavior (upstream models not listed during refresh are deleted, unless pinned in combo_targets)
-- 0: retain discovered models even if temporarily absent from upstream /models (ideal for dynamic/P2P providers)
ALTER TABLE providers ADD COLUMN prune_models INTEGER NOT NULL DEFAULT 1 CHECK(prune_models IN (0, 1));
