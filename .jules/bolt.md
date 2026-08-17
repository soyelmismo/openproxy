## 2026-08-10 - Fix memory inefficiencies across crates
**Learning:** Found several places where we used `serde_json::to_string(...).unwrap_or_default()` in HTTP response paths which could lead to returning empty/malformed error responses or silent serialization failures. Also avoided deep-cloning JSON ASTs via `serde_json::from_value(val.clone())` by using `Deserialize::deserialize(val)` for memory footprint reduction.
**Action:** Always prefer `unwrap_or_else` over `unwrap_or_default` on string serializations. Use trait-based deserialization directly from `&Value` instead of `from_value` for reference types when reducing allocations.

## 2024-05-24 - Serde JSON Deserialization Optimizations
**Learning:** When deserializing an owned `serde_json::Value` into a struct, `serde_json::from_value(owned_value)` is much faster than `<T as serde::Deserialize>::deserialize(&owned_value)`. `from_value` takes ownership and avoids cloning strings/allocations when mapping strings from the AST to the target struct.
**Action:** Replace `<T as serde::Deserialize>::deserialize(&value)` with `serde_json::from_value(value)` where `value` is owned and can be consumed, such as in `adapters` or `dispatcher`.
