## 2024-07-07 - Avoid Deep Cloning JSON ASTs in Middleware
**Learning:** The openproxy request pipeline previously extracted the JSON body as a `serde_json::Value` in `auth.rs` (via `ParsedChatRequest(Value)`) and later deeply cloned it in `routing.rs` (`serde_json::from_value(parsed.clone())`) to construct the `OpenAIRequest` struct. Cloning a `Value` DOM for large payloads (e.g. LLM prompts) is extremely expensive in terms of heap allocations.
**Action:** Always store the raw `bytes::Bytes` alongside the parsed JSON DOM in middleware extensions. Downstream stages can then use `serde_json::from_slice(&bytes)` to deserialize directly from the byte array, avoiding the deep clone entirely.
## 2024-05-18 - Minimize heap allocations in Axum middleware
**Learning:** We can reduce large `.clone()` allocations for large `serde_json::Value` structs by wrapping them in an `Arc`.
**Action:** Use `Arc<T>` for heavy JSON payloads passed across middleware.
## 2026-07-10 - Optimize SQLite Migrations Transaction
**Learning:** Applying individual SQLite migrations sequentially in a loop, each starting its own transaction (N+1 transaction issue), significantly slows down database initialization when the migration count grows. By grouping all pending migrations inside a single `Transaction` and moving schema enforcement pragmas outside the loop, database setup execution time during testing/cold-starts is noticeably reduced (by around ~25%).
**Action:** When executing batch schema updates or bulk sequential operations on SQLite startup, avoid individual transactions in a loop; wrap the loop in a single overarching `rusqlite::Transaction`.
## 2024-02-14 - Caching Hostname Resolution
**Learning:** Frequent calls to `std::fs::read_to_string` and `std::env::var` for static data like hostnames causes measurable blocking I/O overhead.
**Action:** Use `std::sync::OnceLock` to execute the file reads once and cache the result, turning a blocking I/O operation into a fast memory read.
## 2025-02-20 - Array Allocation and Clone Optimization in usage_filter_query
**Learning:** Constructing arrays containing `Option<String>` locally from a struct containing `Option<String>` requires deep clones of the strings, and iterating via `.collect::<Vec<_>>()` performs unnecessary vector allocation. Directly creating an array of `Option<&str>` via `.as_deref()` bypasses both the string cloning and intermediate array allocation.
**Action:** When constructing arrays of pairs for query builders from references to a source struct, prefer extracting properties as references (`Option<&str>`) with `.as_deref()` or local lifetime-bound string conversions rather than `.clone()`.
## 2024-03-20 - [N+1 SQLite Queries in sync loops]
**Learning:** Checking for row existence (`SELECT EXISTS`) iteratively within a rust loop creates massive single query overheads, and using `transaction()` is invalid on a `&Connection` borrowing context without refactoring to `&mut`.
**Action:** Move query statements ahead of loops using `IN` or fetching pre-filtered `HashSet`s. Manually execute `BEGIN` and `COMMIT` through SQL strings if you cannot mutate the connection structure directly. Use `vec!["?"; len].join(",")` to generate `IN` clauses without depending on external crates like `itertools`.
## 2026-07-10 - Avoid Cloning Large JSON Structs in PipelineRequest
**Learning:** Wrapping `request_body_json` in `Arc<serde_json::Value>` instead of `Option<serde_json::Value>` within `PipelineRequest` prevents expensive deep cloning of the JSON payload during the chat completions pipeline execution. This significantly reduces heap allocations, especially for large requests.
**Action:** Use `Arc<T>` to wrap large JSON payloads inside internal request structures when they only need to be shared across pipeline stages without mutation.
