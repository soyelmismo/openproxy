use openproxy_adapters::adapters::ProviderAdapterEnum;
use openproxy_db::secrets::MasterKey;
use openproxy_types::error::CoreError;
use openproxy_types::ids::AccountId;
pub use openproxy_types::oauth::TokenResponse;
use std::sync::Arc;

pub trait PipelineOAuthRegistry: Send + Sync {
    /// Refresh and persist tokens for `account_id`.
    ///
    /// `db_pool` is `None` only for pipeline configurations built without a pool
    /// (legacy test harnesses); those configurations cannot serve an OAuth
    /// refresh and return an error rather than blocking the runtime on a
    /// connection lock.
    fn refresh_and_store<'a>(
        &'a self,
        provider_id: &'a str,
        refresh_token: &'a str,
        upstream_client: &'a Arc<openproxy_adapters::upstream::UpstreamClient>,
        account_id: AccountId,
        db_pool: Option<&'a openproxy_db::DbPool>,
        master_key: &'a MasterKey,
    ) -> futures_util::future::BoxFuture<'a, Result<TokenResponse, CoreError>>;

    /// Refresh and persist tokens for `account_id` using a shared connection.
    ///
    /// Provided for backward compatibility with legacy pipelines built without
    /// a database pool (`Pipeline::new`). Default implementation returns an
    /// explicit error for external custom implementations.
    fn refresh_and_store_shared<'a>(
        &'a self,
        provider_id: &'a str,
        refresh_token: &'a str,
        upstream_client: &'a Arc<openproxy_adapters::upstream::UpstreamClient>,
        account_id: AccountId,
        conn: &'a Arc<parking_lot::Mutex<rusqlite::Connection>>,
        master_key: &'a MasterKey,
    ) -> futures_util::future::BoxFuture<'a, Result<TokenResponse, CoreError>> {
        let _ = (
            provider_id,
            refresh_token,
            upstream_client,
            account_id,
            conn,
            master_key,
        );
        Box::pin(async move {
            Err(CoreError::Internal(
                "refresh_and_store_shared not implemented for this OAuth registry".to_string(),
            ))
        })
    }
}

/// Decides whether a target's OAuth access token needs renewing before dispatch.
///
/// `access_token` is the second, independent source for the deadline. Some
/// portals omit `expires_in` from the token response (RFC 6749 makes it
/// optional), so a row stored by an older build can carry `expires_at = NULL`
/// even though the token expires in an hour. Reading the JWT `exp` claim keeps
/// such an account visible to the refresh stage instead of letting it read as
/// "fresh forever" and lapse into a 401.
pub fn pipeline_token_needs_refresh(
    db_expires_at: Option<&str>,
    access_token: Option<&str>,
    provider_id: &str,
    adapters: &[ProviderAdapterEnum],
) -> bool {
    let expires_at = db_expires_at
        .and_then(|ts| openproxy_types::timestamp::parse_timestamp(ts).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
        .or_else(|| access_token.and_then(openproxy_adapters::jwt::exp_timestamp));
    let Some(expires_at) = expires_at else {
        // No deadline anywhere: nothing can say the token is stale, so leave
        // the request alone rather than burning a rotating refresh token.
        return false;
    };
    let mut lead = 900;
    if let Some(adapter) = adapters.iter().find(|a| a.id().as_str() == provider_id)
        && let Some(l) = adapter.metadata().oauth_refresh_lead_seconds
    {
        lead = l;
    }
    let threshold = chrono::Utc::now() + chrono::Duration::seconds(lead as i64);
    expires_at <= threshold
}
