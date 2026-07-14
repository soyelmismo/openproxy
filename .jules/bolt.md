## 2023-10-24 - [Avoid Regex::new in hot paths]
**Learning:** regex::Regex::new is expensive and compiles the regex every time it is called. In `kiro_ai.rs` it was called twice per `fetch_models_for_account` and `build_chat_url_for_account`.
**Action:** Extract repeated regex creation to static `Lazy<Regex>` using `once_cell::sync::Lazy`.
