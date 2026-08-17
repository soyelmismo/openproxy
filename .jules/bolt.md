## 2024-05-24 - Serde JSON Deserialization Optimizations
**Learning:** When deserializing an owned `serde_json::Value` into a struct, `serde_json::from_value(owned_value)` is much faster than `<T as serde::Deserialize>::deserialize(&owned_value)`. `from_value` takes ownership and avoids cloning strings/allocations when mapping strings from the AST to the target struct.
**Action:** Replace `<T as serde::Deserialize>::deserialize(&value)` with `serde_json::from_value(value)` where `value` is owned and can be consumed, such as in `adapters` or `dispatcher`.
## 2024-05-24 - Serde JSON Deserialization Optimizations
**Learning:** When deserializing an owned `serde_json::Value` into a struct, `serde_json::from_value(owned_value)` is much faster than `<T as serde::Deserialize>::deserialize(&owned_value)`. `from_value` takes ownership and avoids cloning strings/allocations when mapping strings from the AST to the target struct.
**Action:** Replace `<T as serde::Deserialize>::deserialize(&value)` with `serde_json::from_value(value)` where `value` is owned and can be consumed, such as in `adapters` or `dispatcher`.
