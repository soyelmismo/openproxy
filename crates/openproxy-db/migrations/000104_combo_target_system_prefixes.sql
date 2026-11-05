-- Multi-criteria system prefix routing per combo target.
--
-- Enables semantic persona/prefix dispatch:
-- When a combo target is selected, its active system prefixes are evaluated.
-- If exactly 1 active prefix exists, it is injected directly.
-- If >=2 active prefixes exist, the combo's System One decision engine (Laya/Jev)
-- evaluates each prefix's `criteria` against the user's prompt to select the
-- optimal persona (e.g. trading specialist, strict evaluation, nsfw persona).
-- The injected prefix tokens are subtracted from `usage.prompt_tokens` so
-- downstream clients receive exact, untampered token accounting.

CREATE TABLE combo_target_system_prefixes (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    target_id INTEGER NOT NULL REFERENCES combo_targets(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    criteria TEXT NOT NULL,
    system_text TEXT NOT NULL,
    active INTEGER NOT NULL DEFAULT 1,
    priority_order INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX idx_ctsp_target ON combo_target_system_prefixes(target_id);
