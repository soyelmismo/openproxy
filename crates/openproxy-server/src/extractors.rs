//! Custom Axum extractors for openproxy-server handlers.

use axum::{
    extract::{FromRef, FromRequestParts},
    http::request::Parts,
};
use openproxy_db as db;
use std::ops::{Deref, DerefMut};

use crate::{
    error::ApiError, middleware::auth::ValidatedApiToken, state::AppState,
};

/// Axum extractor that acquires a read connection from [`AppState`].
pub struct DbReader(pub db::ArcReaderGuard);

impl Deref for DbReader {
    type Target = rusqlite::Connection;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<S> FromRequestParts<S> for DbReader
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(_parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);
        let r = app_state.db_pool().reader_guard();
        Ok(DbReader(r))
    }
}

/// Axum extractor that acquires a write connection from [`AppState`].
pub struct DbWriter(pub db::ArcWriterGuard);

impl Deref for DbWriter {
    type Target = rusqlite::Connection;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for DbWriter {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl<S> FromRequestParts<S> for DbWriter
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(_parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let app_state = AppState::from_ref(state);
        let w = app_state.db_pool().writer_guard();
        Ok(DbWriter(w))
    }
}

/// Axum extractor to inject optional validated token [`ValidatedApiToken`] into handler signatures.
#[derive(Clone, Debug)]
pub struct ValidatedToken(pub Option<ValidatedApiToken>);

impl<S> FromRequestParts<S> for ValidatedToken
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(ValidatedToken(
            parts.extensions.get::<ValidatedApiToken>().cloned(),
        ))
    }
}
