use super::*;
use rusqlite::Connection;
use std::path::PathBuf;

mod crud;
mod matching;

pub(crate) fn fresh_pool() -> (Connection, PathBuf) {
    let conn = openproxy_db::testing::open_in_memory();
    (conn, PathBuf::from(":memory:"))
}

pub(crate) fn make_input(label: &str) -> CreateApiKeyInput {
    CreateApiKeyInput {
        label: Some(label.to_string()),
        scopes: vec!["chat".to_string()],
        ..Default::default()
    }
}
