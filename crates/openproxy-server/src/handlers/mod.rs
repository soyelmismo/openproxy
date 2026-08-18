//! HTTP handler modules.
//!
//! Each submodule is one cluster of axum handlers (`chat`, `models`, `admin`, `audio`).
//! The router in [`crate::router`] wires them up; shared concerns like
//! error mapping and state extraction live in [`crate::error`] and
//! [`crate::state`].

macro_rules! call_unary_executor {
    ($executor:path, $state:expr, $req:expr, $api_key_id:expr) => {
        $executor(
            $state.db_pool().as_ref(),
            $state.adapters().as_slice(),
            $state.upstream_client(),
            &$state.circuit_breaker(),
            $state.master_key().as_ref(),
            $req,
            $api_key_id,
        )
        .await
        .map_err(crate::error::ApiError)?
    };
}

pub mod admin;
pub mod audio;
pub mod chat;
pub mod embeddings;
pub mod images;
pub mod messages;
pub mod models;
