//! Kiro AI (AWS SSO OIDC) OAuth provider.

pub mod profiles;
pub mod registration;

#[cfg(test)]
mod tests;

pub use profiles::*;
pub use registration::*;

use rusqlite::{OptionalExtension, params};
use std::sync::Arc;

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::{
    DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse, map_upstream_err,
};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_db::secrets::MasterKey;

#[derive(Clone)]
pub struct KiroOAuthProvider;

impl KiroOAuthProvider {
    pub fn new() -> Self {
        Self
    }
}

impl Default for KiroOAuthProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl OAuthProvider for KiroOAuthProvider {
    fn name(&self) -> &'static str {
        "kiro"
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::DeviceCode
    }

    fn build_auth_url(
        &self,
        _redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<(String, String, String, String)>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "kiro uses device code flow, not PKCE".into(),
        )))
    }

    fn exchange_code(
        &self,
        _code: &str,
        _code_verifier: &str,
        _upstream_client: &Arc<UpstreamClient>,
        _redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<TokenResponse>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "kiro uses device code flow, not authorization code".into(),
        )))
    }

    async fn request_device_code(
        &self,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<DeviceAuthorizationResponse> {
        let register_body = serde_json::to_vec(&RegisterClientRequest {
            client_name: "openproxy-kiro".into(),
            client_type: "public".into(),
            scopes: SCOPES
                .iter()
                .map(std::string::ToString::to_string)
                .collect(),
            grant_types: vec![
                "urn:ietf:params:oauth:grant-type:device_code".into(),
                "refresh_token".into(),
            ],
        })
        .map_err(|e| CoreError::Parse(format!("kiro register serialize: {e}")))?;
        let register_req =
            UpstreamRequest::post_json(REGISTER_URL, bytes::Bytes::from(register_body));

        let cancel = CancellationToken::new();
        let register_response = upstream_client
            .call(register_req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "kiro client register"))?;

        let register_status = register_response.status;
        let register_body = register_response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "kiro register body read"))?;
        if !register_status.is_success() {
            let body_str = String::from_utf8_lossy(&register_body).to_string();
            return Err(CoreError::upstream_error(
                register_status.as_u16(),
                "kiro",
                "<oauth>",
                body_str,
                false,
            ));
        }

        let client: RegisterClientResponse = serde_json::from_slice(&register_body)
            .map_err(|e| CoreError::Parse(format!("kiro register response parse: {e}")))?;

        let auth_body = serde_json::json!({
            "clientId": client.client_id,
            "clientSecret": client.client_secret,
            "startUrl": "https://view.awsapps.com/start",
        });
        let auth_body_bytes = serde_json::to_vec(&auth_body)
            .map_err(|e| CoreError::Parse(format!("kiro device auth serialize: {e}")))?;
        let device_auth_req =
            UpstreamRequest::post_json(DEVICE_AUTH_URL, bytes::Bytes::from(auth_body_bytes));

        let device_auth_response = upstream_client
            .call(
                device_auth_req,
                TimeoutProfile::OAuth,
                CancellationToken::new(),
            )
            .await
            .map_err(|e| map_upstream_err(e, "kiro device authorization"))?;

        let device_auth_status = device_auth_response.status;
        let device_auth_body = device_auth_response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "kiro device auth body read"))?;
        if !device_auth_status.is_success() {
            let body_str = String::from_utf8_lossy(&device_auth_body).to_string();
            return Err(CoreError::upstream_error(
                device_auth_status.as_u16(),
                "kiro",
                "<oauth>",
                body_str,
                false,
            ));
        }

        let dar: DeviceAuthorizationResponse = serde_json::from_slice(&device_auth_body)
            .map_err(|e| CoreError::Parse(format!("kiro device auth response parse: {e}")))?;

        let cid = client.client_id;
        let csec = client.client_secret;
        tokio::task::spawn_blocking(move || {
            if let Ok(mut slot) = LAST_KIRO_CLIENT.lock() {
                *slot = Some(LastKiroClient {
                    client_id: cid,
                    client_secret: csec,
                    stored_at: std::time::Instant::now(),
                });
            }
        })
        .await
        .ok();

        Ok(dar)
    }

    async fn poll_device_token(
        &self,
        device_code: &str,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<Option<TokenResponse>> {
        let (cid, csec) = peek_last_client().unwrap_or_default();
        let body = serde_json::json!({
            "clientId": cid,
            "clientSecret": csec,
            "deviceCode": device_code,
            "grantType": "urn:ietf:params:oauth:grant-type:device_code",
        });
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| CoreError::Parse(format!("kiro device poll serialize: {e}")))?;
        let req = UpstreamRequest::post_json(TOKEN_URL, bytes::Bytes::from(body_bytes));

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "kiro device poll"))?;

        let status = response.status;
        let body = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "kiro device poll body read"))?;

        if status.as_u16() == 400 || status.as_u16() == 428 {
            return Ok(None);
        }

        if !status.is_success() {
            let body_str = String::from_utf8_lossy(&body).to_string();
            return Err(CoreError::upstream_error(
                status.as_u16(),
                "kiro",
                "<oauth>",
                body_str,
                false,
            ));
        }

        serde_json::from_slice::<TokenResponse>(&body)
            .map(Some)
            .map_err(|e| CoreError::Parse(format!("kiro token parse: {e}")))
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        account_id: AccountId,
        db: crate::oauth::DbRef<'_>,
    ) -> Result<TokenResponse> {
        let meta = db
            .with_conn(|conn| read_profile_meta(conn, account_id))?
            .unwrap_or_else(KiroProviderMeta::default);

        let region = if meta.region.is_empty() {
            DEFAULT_REGION
        } else {
            meta.region.as_str()
        };
        let token_url = format!("https://oidc.{region}.amazonaws.com/token");

        if meta.auth_method.as_deref() == Some("imported")
            || (meta.client_id.is_empty() && meta.client_secret.is_empty())
        {
            let social_token_url = "https://prod.us-east-1.auth.desktop.kiro.dev/refreshToken";
            let body = serde_json::json!({
                "refreshToken": refresh_token,
            });
            let body_bytes = serde_json::to_vec(&body).map_err(|e| {
                CoreError::Parse(format!("kiro social token refresh serialize: {e}"))
            })?;
            let req = UpstreamRequest::post_json(social_token_url, bytes::Bytes::from(body_bytes));

            let cancel = CancellationToken::new();
            let response = upstream_client
                .call(req, TimeoutProfile::OAuth, cancel)
                .await
                .map_err(|e| map_upstream_err(e, "kiro social token refresh"))?;

            let status = response.status;
            let body_bytes = response
                .collect()
                .await
                .map_err(|e| map_upstream_err(e, "kiro social token refresh body read"))?;

            if !status.is_success() {
                let body_str = String::from_utf8_lossy(&body_bytes).to_string();
                return Err(CoreError::upstream_error(
                    status.as_u16(),
                    "kiro",
                    "<oauth_social>",
                    body_str,
                    false,
                ));
            }

            let mut data: serde_json::Value = serde_json::from_slice(&body_bytes)
                .map_err(|e| CoreError::Parse(format!("kiro social token refresh parse: {e}")))?;

            if data.get("refreshToken").is_none()
                && data.get("refresh_token").is_none()
                && let Some(obj) = data.as_object_mut()
            {
                obj.insert(
                    "refresh_token".to_string(),
                    serde_json::json!(refresh_token),
                );
            }
            if data.get("token_type").is_none()
                && data.get("tokenType").is_none()
                && let Some(obj) = data.as_object_mut()
            {
                obj.insert("token_type".to_string(), serde_json::json!("Bearer"));
            }
            if data.get("expiresIn").is_none()
                && data.get("expires_in").is_none()
                && let Some(obj) = data.as_object_mut()
            {
                obj.insert("expires_in".to_string(), serde_json::json!(3600));
            }

            return <TokenResponse as serde::Deserialize>::deserialize(&data)
                .map_err(|e| CoreError::Parse(format!("kiro social token refresh map: {e}")));
        }

        let body = serde_json::json!({
            "clientId": meta.client_id,
            "clientSecret": meta.client_secret,
            "refreshToken": refresh_token,
            "grantType": "refresh_token",
        });
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| CoreError::Parse(format!("kiro token refresh serialize: {e}")))?;
        let req = UpstreamRequest::post_json(&token_url, bytes::Bytes::from(body_bytes));

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "kiro token refresh"))?;

        let status = response.status;
        let body_bytes = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "kiro token refresh body read"))?;

        let mut success_body = None;
        if status.is_success() {
            success_body = Some(bytes::Bytes::clone(&body_bytes));
        } else {
            tracing::warn!(
                account = account_id.0,
                status = status.as_u16(),
                "kiro token refresh failed; attempting dynamic client re-registration..."
            );
            if let Ok((new_cid, new_csec)) = register_oidc_client(upstream_client, region).await {
                let retry_body = serde_json::json!({
                    "clientId": new_cid,
                    "clientSecret": new_csec,
                    "refreshToken": refresh_token,
                    "grantType": "refresh_token",
                });
                let retry_body_bytes = serde_json::to_vec(&retry_body).map_err(|e| {
                    CoreError::Parse(format!("kiro token refresh retry serialize: {e}"))
                })?;
                let retry_req =
                    UpstreamRequest::post_json(&token_url, bytes::Bytes::from(retry_body_bytes));
                let retry_cancel = CancellationToken::new();
                if let Ok(retry_resp) = upstream_client
                    .call(retry_req, TimeoutProfile::OAuth, retry_cancel)
                    .await
                {
                    let retry_status = retry_resp.status;
                    if let Ok(retry_bytes) = retry_resp.collect().await
                        && retry_status.is_success()
                    {
                        let mut updated_meta = meta;
                        updated_meta.client_id = new_cid;
                        updated_meta.client_secret = new_csec;
                        let meta_json = serde_json::to_string(&updated_meta).map_err(|e| {
                            CoreError::Internal(format!("kiro meta serialize: {e}"))
                        })?;
                        db.with_conn(|conn| {
                            conn.execute(
                                "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
                                rusqlite::params![meta_json, account_id.0],
                            )
                            .map_err(openproxy_db::error::map_db_error)
                        })?;
                        success_body = Some(retry_bytes);
                    }
                }
            }
        }

        let Some(final_body) = success_body else {
            let body_str = String::from_utf8_lossy(&body_bytes).to_string();
            return Err(CoreError::upstream_error(
                status.as_u16(),
                "kiro",
                "<oauth>",
                body_str,
                false,
            ));
        };

        let mut data: serde_json::Value = serde_json::from_slice(&final_body)
            .map_err(|e| CoreError::Parse(format!("kiro token refresh parse: {e}")))?;
        if data.get("refresh_token").is_none()
            && data.get("refreshToken").is_none()
            && let Some(obj) = data.as_object_mut()
        {
            obj.insert(
                "refresh_token".to_string(),
                serde_json::json!(refresh_token),
            );
        }
        if data.get("expires_in").is_none()
            && data.get("expiresIn").is_none()
            && let Some(obj) = data.as_object_mut()
        {
            obj.insert("expires_in".to_string(), serde_json::json!(3600));
        }
        if data.get("token_type").is_none()
            && data.get("tokenType").is_none()
            && let Some(obj) = data.as_object_mut()
        {
            obj.insert("token_type".to_string(), serde_json::json!("Bearer"));
        }

        <TokenResponse as serde::Deserialize>::deserialize(&data)
            .map_err(|e| CoreError::Parse(format!("kiro token refresh map: {e}")))
    }

    async fn post_exchange(
        &self,
        account_id: AccountId,
        db_pool: &std::sync::Arc<openproxy_db::DbPool>,
        master_key: &MasterKey,
        upstream: &Arc<UpstreamClient>,
    ) -> Result<()> {
        let (access_token, mut meta) = {
            let db_pool = Arc::clone(db_pool);
            let master_key = master_key.clone();

            tokio::task::spawn_blocking(move || {
                let conn = db_pool.writer();
                let access_token =
                    crate::accounts::decrypt_access_token(&conn, account_id, &master_key)?;

                let raw: Option<Option<String>> = conn
                    .query_row(
                        "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
                        params![account_id.0],
                        |r| r.get::<_, Option<String>>(0),
                    )
                    .optional()
                    .map_err(|e| CoreError::Database {
                        message: format!(
                            "kiro post_exchange read meta for account {}: {}",
                            account_id.0, e
                        ),
                        source: Some(std::sync::Arc::new(e)),
                    })?;
                let raw = raw.flatten();

                let meta: KiroProviderMeta = match raw {
                    Some(s) => serde_json::from_str(&s)
                        .map_err(|e| CoreError::Parse(format!("kiro meta parse: {e}")))?,
                    None => KiroProviderMeta::default(),
                };

                Ok::<_, CoreError>((access_token, meta))
            })
            .await??
        };

        match list_available_profiles(upstream, &access_token, &meta.region).await {
            Ok(Some(arn)) => {
                meta.profile_arn = Some(arn);
            }
            Ok(None) => {
                tracing::info!(
                    account = account_id.0,
                    "kiro post_exchange: no profiles available; profileArn left empty"
                );
            }
            Err(e) => {
                tracing::warn!(
                    account = account_id.0,
                    error = %e,
                    "kiro post_exchange: list_available_profiles failed (likely restricted account access); proceeding without profileArn"
                );
            }
        }

        let meta_json = serde_json::to_string(&meta)
            .map_err(|e| CoreError::Internal(format!("kiro meta serialize: {e}")))?;
        let db_pool = Arc::clone(db_pool);
        tokio::task::spawn_blocking(move || -> std::result::Result<(), CoreError> {
            let conn = db_pool.writer();
            conn.execute(
                "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
                params![meta_json, account_id.0],
            )
            .map_err(|e| CoreError::Database {
                message: format!(
                    "kiro post_exchange update meta for account {}: {}",
                    account_id.0, e
                ),
                source: Some(std::sync::Arc::new(e)),
            })?;
            Ok(())
        })
        .await??;

        Ok(())
    }
}
