use std::sync::Arc;
use rusqlite::Connection;
use openproxy_db::secrets::MasterKey;
use openproxy_types::ids::AccountId;
use openproxy_types::error::CoreError;
use openproxy_adapters::adapters::ProviderAdapterEnum;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    #[serde(rename = "access_token", alias = "accessToken")]
    pub access_token: String,
    #[serde(default, rename = "token_type", alias = "tokenType")]
    pub token_type: String,
    #[serde(default, rename = "expires_in", alias = "expiresIn")]
    pub expires_in: Option<u64>,
    #[serde(default, rename = "refresh_token", alias = "refreshToken")]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default, rename = "id_token", alias = "idToken")]
    pub id_token: Option<String>,
}

pub trait PipelineOAuthRegistry: Send + Sync {
    fn refresh_and_store<'a>(
        &'a self,
        provider_id: &'a str,
        refresh_token: &'a str,
        upstream_client: &'a Arc<openproxy_adapters::upstream::UpstreamClient>,
        account_id: AccountId,
        conn: &'a parking_lot::Mutex<Connection>,
        master_key: &'a MasterKey,
    ) -> futures_util::future::BoxFuture<'a, Result<TokenResponse, CoreError>>;
}

pub fn pipeline_token_needs_refresh(
    db_expires_at: Option<&str>,
    provider_id: &str,
    adapters: &[ProviderAdapterEnum],
) -> bool {
    let Some(ts) = db_expires_at else {
        return false;
    };
    let Ok(expires_at) = chrono::DateTime::parse_from_rfc3339(ts) else {
        return false;
    };
    let expires_at = expires_at.with_timezone(&chrono::Utc);
    let mut lead = 900;
    if let Some(adapter) = adapters.iter().find(|a| a.id().as_str() == provider_id)
        && let Some(l) = adapter.metadata().oauth_refresh_lead_seconds
    {
        lead = l;
    }
    let threshold = chrono::Utc::now() + chrono::Duration::seconds(lead as i64);
    expires_at <= threshold
}
