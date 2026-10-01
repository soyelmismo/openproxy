-- Durable admission and atomic acknowledgement for usage jobs. No FK to mutable
-- provider/target tables: deleting a target must not discard pending accounting.
CREATE TABLE usage_journal (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);
