1. Add `#[cfg(test)] mod tests` block at the bottom of `crates/openproxy-core/src/combos/crud.rs`
2. Create a helper function `setup_db()` which uses `rusqlite::Connection::open_in_memory()` and runs `crate::db::migrations::run(&mut conn)`.
3. Add a test `test_create_combo_success` to verify a combo can be created successfully using `create_combo` function and verify the inserted combo row exists using `get_combo`.
4. Add a test `test_create_combo_validation_error` to verify creating a combo with invalid race size out of `1..=8` boundaries throws `CoreError::Validation`.
5. Add a test `test_create_combo_unique_name_error` to verify creating a combo with duplicate name throws `CoreError::Validation` mentioning "combo name already exists".
6. Run `cargo test -p openproxy-core -- tests::` to verify tests pass.
