## 2024-07-07 - Avoid Deep Cloning JSON ASTs in Middleware
**Learning:** The openproxy request pipeline previously extracted the JSON body as a `serde_json::Value` in `auth.rs` (via `ParsedChatRequest(Value)`) and later deeply cloned it in `routing.rs` (`serde_json::from_value(parsed.clone())`) to construct the `OpenAIRequest` struct. Cloning a `Value` DOM for large payloads (e.g. LLM prompts) is extremely expensive in terms of heap allocations.
**Action:** Always store the raw `bytes::Bytes` alongside the parsed JSON DOM in middleware extensions. Downstream stages can then use `serde_json::from_slice(&bytes)` to deserialize directly from the byte array, avoiding the deep clone entirely.
## 2024-05-18 - Minimize heap allocations in Axum middleware
**Learning:** We can reduce large `.clone()` allocations for large `serde_json::Value` structs by wrapping them in an `Arc`.
**Action:** Use `Arc<T>` for heavy JSON payloads passed across middleware.
## 2024-02-14 - Caching Hostname Resolution
**Learning:** Frequent calls to `std::fs::read_to_string` and `std::env::var` for static data like hostnames causes measurable blocking I/O overhead.
**Action:** Use `std::sync::OnceLock` to execute the file reads once and cache the result, turning a blocking I/O operation into a fast memory read.
