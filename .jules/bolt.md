## 2024-07-07 - Avoid Deep Cloning JSON ASTs in Middleware
**Learning:** The openproxy request pipeline previously extracted the JSON body as a `serde_json::Value` in `auth.rs` (via `ParsedChatRequest(Value)`) and later deeply cloned it in `routing.rs` (`serde_json::from_value(parsed.clone())`) to construct the `OpenAIRequest` struct. Cloning a `Value` DOM for large payloads (e.g. LLM prompts) is extremely expensive in terms of heap allocations.
**Action:** Always store the raw `bytes::Bytes` alongside the parsed JSON DOM in middleware extensions. Downstream stages can then use `serde_json::from_slice(&bytes)` to deserialize directly from the byte array, avoiding the deep clone entirely.
## 2024-05-18 - Minimize heap allocations in Axum middleware
**Learning:** We can reduce large `.clone()` allocations for large `serde_json::Value` structs by wrapping them in an `Arc`.
**Action:** Use `Arc<T>` for heavy JSON payloads passed across middleware.
## $(date +%Y-%m-%d) - Optimize SQLite Migrations Transaction
**Learning:** Applying individual SQLite migrations sequentially in a loop, each starting its own transaction (N+1 transaction issue), significantly slows down database initialization when the migration count grows. By grouping all pending migrations inside a single `Transaction` and moving schema enforcement pragmas outside the loop, database setup execution time during testing/cold-starts is noticeably reduced (by around ~25%).
**Action:** When executing batch schema updates or bulk sequential operations on SQLite startup, avoid individual transactions in a loop; wrap the loop in a single overarching `rusqlite::Transaction`.
