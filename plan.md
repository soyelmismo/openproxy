1. **Pre-allocate capacity in `crates/openproxy-adapters/src/upstream/client.rs`**
   - Update `post_json` to pre-allocate `HeaderMap` with `HeaderMap::with_capacity(1)`.
   - Update `post_multipart` to pre-allocate `HeaderMap` with `HeaderMap::with_capacity(1)`.
   - Update `format_hyper_error` to pre-allocate `Vec` for `parts` using `Vec::with_capacity`.
2. **Pre-allocate capacity in `crates/openproxy-pipeline/src/dispatcher/unary.rs`**
   - In `populate_upstream_headers`, reserve capacity in `upstream_request.headers` before the loop using `upstream_request.headers.reserve(headers.len())`.
3. Complete pre-commit steps to ensure proper testing, verification, review, and reflection are done.
4. Run `cargo test` and `cargo fmt`.
5. Submit PR with title "⬆️ Bump: Update pre-allocate capacity to minimize reallocations".
