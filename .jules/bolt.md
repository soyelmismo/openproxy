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
## 2024-05-18 - Repeated Regex Compilation in Kiro Adapter Hot Paths
**Learning:** Found repeated `regex::Regex::new(r"[a-z]{2}-[a-z]+-[0-9]")` in `KiroAdapter::build_chat_url_for_account` and `fetch_models_for_account`. These are in the hot path for every AWS CodeWhisperer proxy request, causing unnecessary allocation and compilation overhead.
**Action:** Use `once_cell::sync::Lazy` to compile regex once globally, reducing allocation and latency on hot paths.
## 2025-02-23 - Sort and Dedup key allocations
**Learning:** Using `sort_unstable_by_key(|id| id.0.clone())` and `dedup_by_key(|id| id.0.clone())` allocates a new `String` for every single comparison during the sorting/deduping process. This creates a massive number of temporary heap allocations on hot paths.
**Action:** Replace `by_key` with `.clone()` closures with the more explicit `sort_unstable_by(|a, b| a.0.cmp(&b.0))` and `dedup_by(|a, b| a.0 == b.0)`. This performs the exact same operation without any allocations.
## 2024-05-18 - OAuth Refresh Batching
**Learning:** The OAuth token refresh scheduler spawned a blocking task per account to decrypt tokens inside a loop, causing unnecessary scheduling overhead and DB locks.
**Action:** Batch DB reads with `WHERE id IN (...)` using chunks (e.g. 900) to fetch data efficiently before looping.

## 2026-07-22 - Use reader connections for read-only quota sync ops
**Learning:** Read-only SQLite DB calls inside a blocking thread should use `db_pool.reader()` instead of `db_pool.writer()` to prevent lock contention and executor thread blocking.
**Action:** Use reader locks whenever possible, especially in high-throughput synchronization loops.

## 2026-07-22 - Antigravity OAuth onboarding loop sequential block
**Learning:** Optimizing retry loops for operations where the caller depends on the side-effect (such as OAuth `post_exchange` completing to proceed safely to subsequent steps) by using background tasks (`tokio::spawn`) will introduce a race condition where the caller proceeds without the necessary completed data.
**Action:** Use exponential backoff (e.g. `std::time::Duration::from_millis(500)` doubling on retry) instead of background tasks to safely and functionally reduce latency on expected fast path responses without breaking the synchronous flow.

## 2026-07-22 - [Optimized Batch Inserts in Migrations]
**Learning:** When executing a series of SQLite migrations tracking metadata (version), batching the `INSERT` operations into a single `execute_batch` query eliminates the N parameterization round-trip overhead of cached prepared statements resulting in reduced context switching without risking parameterized data since `version` is integer-primitive.
**Action:** For sequential metadata insertion, favor concatenated bulk batch statements via `execute_batch` over running multiple statements in a `prepare_cached` loop, but always remember to test if the string builder received at least 1 record prior to executing the batch.

## 2026-07-24 - Avoid serde_json::from_value clone overhead
**Learning:** Calling `serde_json::from_value(value.clone())` deeply clones the entire JSON AST just to immediately deserialize it into a struct, causing heavy allocation overhead. `Deserialize` traits can usually deserialize directly from `&serde_json::Value` avoiding this clone altogether.
**Action:** Replace `serde_json::from_value::<T>(val.clone())` with `<T as serde::Deserialize>::deserialize(val)` to avoid the expensive `.clone()` on the JSON AST.
