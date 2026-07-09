## 2024-07-07 - Avoid Deep Cloning JSON ASTs in Middleware
**Learning:** The openproxy request pipeline previously extracted the JSON body as a `serde_json::Value` in `auth.rs` (via `ParsedChatRequest(Value)`) and later deeply cloned it in `routing.rs` (`serde_json::from_value(parsed.clone())`) to construct the `OpenAIRequest` struct. Cloning a `Value` DOM for large payloads (e.g. LLM prompts) is extremely expensive in terms of heap allocations.
**Action:** Always store the raw `bytes::Bytes` alongside the parsed JSON DOM in middleware extensions. Downstream stages can then use `serde_json::from_slice(&bytes)` to deserialize directly from the byte array, avoiding the deep clone entirely.
## 2024-05-18 - Minimize heap allocations in Axum middleware
**Learning:** We can reduce large `.clone()` allocations for large `serde_json::Value` structs by wrapping them in an `Arc`.
**Action:** Use `Arc<T>` for heavy JSON payloads passed across middleware.
## 2025-02-20 - Array Allocation and Clone Optimization in usage_filter_query
**Learning:** Constructing arrays containing `Option<String>` locally from a struct containing `Option<String>` requires deep clones of the strings, and iterating via `.collect::<Vec<_>>()` performs unnecessary vector allocation. Directly creating an array of `Option<&str>` via `.as_deref()` bypasses both the string cloning and intermediate array allocation.
**Action:** When constructing arrays of pairs for query builders from references to a source struct, prefer extracting properties as references (`Option<&str>`) with `.as_deref()` or local lifetime-bound string conversions rather than `.clone()`.
