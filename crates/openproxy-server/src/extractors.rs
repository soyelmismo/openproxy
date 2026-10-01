//! Custom Axum extractors for openproxy-server handlers.

use axum::{
    extract::{FromRef, FromRequestParts},
    http::request::Parts,
};
use openproxy_db as db;
use std::sync::Arc;

use crate::{error::ApiError, middleware::auth::ValidatedApiToken, state::AppState};

/// Pool handle only: connection acquisition and queries stay on blocking threads.
pub struct DbReader(pub Arc<db::DbPool>);

impl DbReader {
    pub async fn run<F, R>(self, query: F) -> Result<R, ApiError>
    where
        F: FnOnce(&rusqlite::Connection) -> Result<R, ApiError> + Send + 'static,
        R: Send + 'static,
    {
        tokio::task::spawn_blocking(move || {
            let reader = self.0.reader();
            query(&reader)
        })
        .await
        .map_err(|error| {
            ApiError(openproxy_types::CoreError::Internal(format!(
                "reader spawn failed: {error}"
            )))
        })?
    }
}

impl<S> FromRequestParts<S> for DbReader
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = ApiError;

    fn from_request_parts(
        _parts: &mut Parts,
        state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let app_state = AppState::from_ref(state);
        let r = Arc::clone(app_state.db_pool());
        std::future::ready(Ok(DbReader(r)))
    }
}

/// Writer pool handle; never exposes a connection guard to async code.
pub struct DbWriter(pub Arc<db::DbPool>);

impl DbWriter {
    pub async fn run<F, R>(self, query: F) -> Result<R, ApiError>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<R, ApiError> + Send + 'static,
        R: Send + 'static,
    {
        tokio::task::spawn_blocking(move || {
            let mut writer = self
                .0
                .try_writer_for(db::conn::ADMIN_LOCK_TIMEOUT)
                .ok_or_else(|| {
                    ApiError(openproxy_types::CoreError::Internal(
                        "writer lock timeout (5s)".into(),
                    ))
                })?;
            query(&mut writer)
        })
        .await
        .map_err(|error| {
            ApiError(openproxy_types::CoreError::Internal(format!(
                "writer spawn failed: {error}"
            )))
        })?
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
        Ok(DbWriter(Arc::clone(app_state.db_pool())))
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

    fn from_request_parts(
        parts: &mut Parts,
        _state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        std::future::ready(Ok(ValidatedToken(
            parts.extensions.get::<ValidatedApiToken>().cloned(),
        )))
    }
}

/// Axum extractor that resolves the true client IP address.
///
/// If the request comes from a trusted reverse proxy (loopback by default, or
/// listed in `config.server.trusted_proxies`), checks `X-Real-IP`,
/// `X-Forwarded-For`, and RFC 7239 `Forwarded`. Otherwise, falls back to the
/// peer address from [`axum::extract::ConnectInfo`] to prevent IP spoofing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClientIp(pub std::net::IpAddr);

impl<S> FromRequestParts<S> for ClientIp
where
    AppState: FromRef<S>,
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    fn from_request_parts(
        parts: &mut Parts,
        state: &S,
    ) -> impl std::future::Future<Output = Result<Self, Self::Rejection>> + Send {
        let app_state = AppState::from_ref(state);
        let peer_addr = parts
            .extensions
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>();
        let ip = crate::client_ip::resolve_client_ip(
            &parts.headers,
            peer_addr.map(|ci| &ci.0),
            &app_state.config().server.trusted_proxies,
        )
        .or_else(|| peer_addr.map(|ci| ci.0.ip()))
        .unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        std::future::ready(Ok(ClientIp(ip)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn writer_contention_does_not_block_runtime() {
        let pool = Arc::new(db::DbPool::test_pool().unwrap());
        let locked_pool = Arc::clone(&pool);
        let (locked_tx, locked_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let lock_thread = std::thread::spawn(move || {
            let _writer = locked_pool.writer();
            locked_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
        });
        locked_rx.await.unwrap();
        let query = tokio::spawn(DbWriter(pool).run(|conn| Ok(conn.is_autocommit())));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        })
        .await
        .unwrap();
        assert!(!query.is_finished());
        release_tx.send(()).unwrap();
        assert!(query.await.unwrap().unwrap());
        lock_thread.join().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_query_runs_off_runtime_and_propagates_errors() {
        let runtime_thread = std::thread::current().id();
        let pool = Arc::new(db::DbPool::test_pool().unwrap());
        let error = DbReader(pool)
            .run(move |_| -> Result<(), ApiError> {
                assert_ne!(std::thread::current().id(), runtime_thread);
                Err(openproxy_types::CoreError::Validation("read failed".into()).into())
            })
            .await
            .unwrap_err();
        assert!(matches!(error.0, openproxy_types::CoreError::Validation(_)));
    }
}
