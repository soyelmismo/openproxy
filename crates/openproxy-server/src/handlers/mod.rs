//! HTTP handler modules.
//!
//! Each submodule is one cluster of axum handlers (`chat`, `models`, `admin`).
//! The router in [`crate::router`] wires them up; shared concerns like
//! error mapping and state extraction live in [`crate::error`] and
//! [`crate::state`].

pub mod chat;
pub mod models;
pub mod admin;
