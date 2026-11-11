//! Z.ai (ZCode / GLM) OAuth 2.0 provider.
//!
//! Implements Authorization Code flow against ZCode public endpoints:
//! - Authorize: `https://chat.z.ai/auth/oauth/authorize`
//! - Token Exchange: `https://zcode.z.ai/api/v1/oauth/token`
//! - Business Login: `https://api.z.ai/api/auth/z/login`
//! - User Info: `https://chat.z.ai/api/oauth/userinfo`

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::zai::exchange::{apply_autoclaw_identity_headers, strip_bearer};
use crate::oauth::{DbRef, DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse};
use openproxy_adapters::upstream::{UpstreamClient, UpstreamRequest};
use openproxy_db::secrets::MasterKey;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const ZAI_CLIENT_ID: &str = "client_P8X5CMWmlaRO9gyO-KSqtg";
pub const ZAI_DEFAULT_AUTHORIZE_URL: &str = "https://chat.z.ai/api/oauth/authorize";
pub const ZAI_DEFAULT_TOKEN_URL: &str = "https://zcode.z.ai/api/v1/oauth/token";
pub const ZAI_DEFAULT_BUSINESS_LOGIN_URL: &str = "https://api.z.ai/api/auth/z/login";
pub const ZAI_DEFAULT_USERINFO_URL: &str = "https://chat.z.ai/api/oauth/userinfo";
pub const ZAI_REDIRECT_URI: &str =
    "https://zcode.z.ai/app/oauth/login?redirect=zcode%3A%2F%2Foauth%2Fcallback&app_version=3.14.0";

/// AutoClaw credential renewal.
///
/// The AutoClaw session access token is a JWT that lives 24h. The refresh grant
/// issued by `zai-oauth-login` lasts 30 days and comes back unchanged from a
/// successful renewal, so this endpoint is a steady-state loop, not a one-shot
/// recovery. `zai-oauth-url`/`zai-oauth-login` are the only other way in and
/// both need a fresh captcha, so without this call every account dies daily.
///
/// Mirrors `ProductAccountCredentialRefreshAdapter` in the official AutoClaw
/// client, which renews on the very rejection this provider used to swallow:
/// HTTP 200 carrying `code: 410000` ("Please log in to continue").
pub const ZAI_AUTOCLAW_REFRESH_URL: &str = "https://autoglm-api.autoglm.ai/userapi/v1/refresh";
const AUTOCLAW_APP_ID: &str = "100003";
const AUTOCLAW_APP_KEY: &str = "38d2391985e2369a5fb8227d8e6cd5e5";
const AUTOCLAW_APP_VERSION: &str = "2.0.3";
/// Device identity for the renewal call.
///
/// A property of this client installation rather than of the account: the
/// official app hardcodes it and every access token it issues carries the same
/// value in its `device_id` claim, so the login and the renewal agree by
/// construction. Reading it back per account would buy nothing.
const AUTOCLAW_DEVICE_ID: &str = "b1b01c10-91dc-4af5-9d32-d17b5f5bbd9b";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ZaiAccountMeta {
    pub autoclaw_access_token: Option<String>,
    pub autoclaw_refresh_token: Option<String>,
    pub zcode_jwt_token: Option<String>,
    pub zai_access_token: Option<String>,
    pub business_access_token: Option<String>,
    pub api_key: Option<String>,
    pub user_id: Option<String>,
    pub name: Option<String>,
    pub email: Option<String>,
    pub avatar: Option<String>,
}

#[derive(Clone, Default)]
pub struct ZaiOAuthProvider;

impl ZaiOAuthProvider {
    pub fn new() -> Self {
        Self
    }
}

impl OAuthProvider for ZaiOAuthProvider {
    fn name(&self) -> &'static str {
        "zai"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["zcode", "z.ai"]
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::AuthorizationCode
    }

    fn build_auth_url(
        &self,
        _redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<(String, String, String, String)>> + Send {
        let state = uuid::Uuid::new_v4().to_string();
        let effective_redirect = ZAI_REDIRECT_URI;

        let params = [
            ("response_type", "code"),
            ("client_id", ZAI_CLIENT_ID),
            ("redirect_uri", effective_redirect),
            ("state", &state),
        ];

        let query = crate::oauth::generic::urlencoded_string(&params);
        let auth_url = format!("{ZAI_DEFAULT_AUTHORIZE_URL}?{query}");

        std::future::ready(Ok((
            auth_url,
            state.clone(),
            effective_redirect.to_string(),
            state,
        )))
    }

    async fn exchange_code(
        &self,
        code: &str,
        code_verifier: &str,
        upstream_client: &Arc<UpstreamClient>,
        _redirect_uri: &str,
    ) -> Result<TokenResponse> {
        let (actual_code, extracted_state) = crate::oauth::util::parse_oauth_callback_input(code);

        // effective state, in order: from the callback URL/fragment, from
        // code_verifier, else a random UUID. Z.ai rejects an empty state with
        // code 3001.
        let effective_state = extracted_state
            .filter(|s| !s.trim().is_empty())
            .or_else(|| {
                let cv = code_verifier.trim();
                if !cv.is_empty() {
                    Some(cv.to_string())
                } else {
                    None
                }
            })
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let effective_redirect = ZAI_REDIRECT_URI;

        let mut zcode_jwt = None;
        let mut zai_access_token = None;
        let mut expires_in = Some(86400 * 30);
        let mut user_id = None;
        let mut name = None;
        let mut email = None;
        let mut avatar = None;
        let mut business_access_token = None;
        let mut api_key = None;

        // 1. Intentar intercambio tradicional con ZCode token URL
        let body = serde_json::json!({
            "provider": "zai",
            "code": actual_code.trim(),
            "redirect_uri": effective_redirect,
            "state": effective_state.trim()
        });

        if let Ok(body_bytes) = serde_json::to_vec(&body) {
            let mut req =
                UpstreamRequest::post_json(ZAI_DEFAULT_TOKEN_URL, bytes::Bytes::from(body_bytes))
                    .with_accept_json();
            req.headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            req.headers.insert(
                http::header::USER_AGENT,
                http::HeaderValue::from_static("ZCode/3.14.0"),
            );

            if let Ok(json) = crate::oauth::call_oauth_json::<serde_json::Value>(
                upstream_client,
                req,
                "zai",
                "zai exchange",
            )
            .await
            {
                let code_num = json
                    .get("code")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(-1);
                if code_num == 0
                    && let Some(data) = json.get("data")
                {
                    zcode_jwt = data
                        .get("token")
                        .and_then(serde_json::Value::as_str)
                        .map(|s| s.trim().to_string());

                    zai_access_token = data
                        .get("zai")
                        .and_then(|z| z.get("access_token"))
                        .and_then(serde_json::Value::as_str)
                        .map(|s| s.trim().to_string());

                    let raw_expires_in = data
                        .get("expires_in")
                        .or_else(|| data.get("expiresIn"))
                        .and_then(serde_json::Value::as_u64);
                    let raw_expires_at = data
                        .get("expires_at")
                        .or_else(|| data.get("expiresAt"))
                        .and_then(serde_json::Value::as_str);
                    expires_in =
                        crate::oauth::util::parse_expires_in(raw_expires_in, raw_expires_at)
                            .or(Some(86400 * 30));

                    let user_val = data.get("user");
                    user_id = user_val
                        .and_then(|u| u.get("id").or_else(|| u.get("user_id")))
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string);
                    name = user_val
                        .and_then(|u| u.get("name").or_else(|| u.get("username")))
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string);
                    email = user_val
                        .and_then(|u| u.get("email"))
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string);
                    avatar = user_val
                        .and_then(|u| u.get("avatar"))
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string);

                    business_access_token = match zai_access_token.as_deref() {
                        Some(tok) => Self::exchange_business_token(upstream_client, tok)
                            .await
                            .ok(),
                        None => None,
                    };

                    api_key = match business_access_token.as_deref() {
                        Some(tok) => {
                            openproxy_adapters::adapters::zai::resolve_zai_api_key(
                                upstream_client,
                                tok,
                            )
                            .await
                        }
                        None => None,
                    };
                }
            }
        }

        let effective_access_token = api_key
            .clone()
            .or_else(|| business_access_token.clone())
            .or_else(|| zcode_jwt.clone())
            .unwrap_or_default();

        let mut autoclaw_access_token = None;
        let mut autoclaw_refresh_token = None;

        let (actual_code_str, state_val_str) = (actual_code.trim(), effective_state.trim());
        let autoclaw_payload = serde_json::json!({
            "code": actual_code_str,
            "state": state_val_str,
            "navigate_uri": "http://localhost:18432/auth/callback-zai",
            "source_id": "autoclaw",
            "device_id": "b1b01c10-91dc-4a25-9d32-d17b5f5bbd9b"
        });

        if let Ok(body_bytes) = serde_json::to_vec(&autoclaw_payload) {
            let mut req_ac = UpstreamRequest::post_json(
                "https://autoglm-api.autoglm.ai/userapi/overseasv1/zai-oauth-login",
                bytes::Bytes::from(body_bytes),
            )
            .with_accept_json();
            req_ac.headers.insert(
                http::header::CONTENT_TYPE,
                http::HeaderValue::from_static("application/json"),
            );
            req_ac.headers.insert(
                http::header::USER_AGENT,
                http::HeaderValue::from_static("AutoClaw/2.0.3"),
            );
            apply_autoclaw_identity_headers(&mut req_ac);

            if let Ok((_status, ac_bytes)) =
                crate::oauth::call_oauth(upstream_client, req_ac, "autoclaw oauth login").await
                && let Ok(ac_json) = serde_json::from_slice::<serde_json::Value>(&ac_bytes)
                && let Some(ac_data) = ac_json.get("data")
                && ac_json.get("code").and_then(serde_json::Value::as_i64) == Some(0)
            {
                autoclaw_access_token = ac_data
                    .get("access_token")
                    .and_then(serde_json::Value::as_str)
                    .map(|s| s.trim().to_string());
                autoclaw_refresh_token = ac_data
                    .get("refresh_token")
                    .and_then(serde_json::Value::as_str)
                    .map(|s| s.trim().to_string());
                if user_id.is_none() {
                    user_id = ac_data
                        .get("user_id")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string);
                }
                if name.is_none() {
                    name = ac_data
                        .get("user_name")
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string);
                }
            }
        }

        if effective_access_token.is_empty() && autoclaw_access_token.is_none() {
            return Err(CoreError::Validation(
                "Failed to exchange OAuth token with both ZCode and AutoClaw".into(),
            ));
        }

        // The AutoClaw login response carries user_id and user_name but no
        // email. The JWT's `jti` claim is the account's address, and it is the
        // identifier the dashboard shows, so backfill it when absent.
        if email.is_none()
            && let Some(jwt) = autoclaw_access_token.as_deref()
        {
            email = openproxy_adapters::jwt::claim_str(jwt, "jti").filter(|e| e.contains('@'));
        }

        let meta = ZaiAccountMeta {
            autoclaw_access_token: autoclaw_access_token.clone(),
            autoclaw_refresh_token: autoclaw_refresh_token.clone(),
            zcode_jwt_token: zcode_jwt.clone(),
            zai_access_token,
            business_access_token,
            api_key,
            user_id,
            name,
            email,
            avatar,
        };

        let final_access_token = if effective_access_token.is_empty() {
            autoclaw_access_token.unwrap_or_default()
        } else {
            effective_access_token
        };

        let id_token = serde_json::to_string(&meta).ok();

        Ok(TokenResponse {
            access_token: final_access_token,
            token_type: "Bearer".into(),
            expires_in,
            refresh_token: autoclaw_refresh_token,
            scope: None,
            id_token,
        })
    }
    async fn post_exchange(
        &self,
        account_id: AccountId,
        db_pool: &Arc<openproxy_db::DbPool>,
        master_key: &MasterKey,
        _upstream: &Arc<UpstreamClient>,
    ) -> Result<()> {
        const WRITER_LOCK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        let pool = Arc::clone(db_pool);
        let read_master_key = master_key.clone();

        tokio::task::spawn_blocking(move || {
            let conn = pool.try_writer_for(WRITER_LOCK_TIMEOUT).ok_or_else(|| {
                CoreError::Internal(format!(
                    "zai post_exchange: writer lock not acquired within {WRITER_LOCK_TIMEOUT:?}"
                ))
            })?;

            let meta_raw: Option<String> = conn
                .query_row(
                    "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
                    rusqlite::params![account_id.0],
                    |row| row.get(0),
                )
                .ok()
                .flatten();

            // The blob is stored encrypted by store_oauth_tokens, so decrypt
            // before parsing. The fallback keeps blobs written by older paths
            // (plain JSON) working.
            let meta = meta_raw
                .as_deref()
                .and_then(|raw| {
                    openproxy_db::accounts::decrypt_oauth_provider_specific(
                        Some(raw),
                        &read_master_key,
                    )
                })
                .and_then(|plain| serde_json::from_str::<ZaiAccountMeta>(&plain).ok());

            if let Some(meta) = meta {
                // The email is the label users recognize. A credential blob may
                // only carry a numeric user id or a display name, so only treat
                // an email-looking value as one.
                let email = meta
                    .email
                    .clone()
                    .filter(|e| e.contains('@') && !e.trim().is_empty());

                if let Some(ident) = meta
                    .name
                    .or(meta.email)
                    .or(meta.user_id)
                    .filter(|s| !s.trim().is_empty())
                {
                    let label = format!("Z.ai ({ident})");
                    let _ = conn.execute(
                        "UPDATE accounts SET label = COALESCE(NULLIF(label, ''), ?1) WHERE id = ?2",
                        rusqlite::params![label, account_id.0],
                    );
                }

                if let Some(email) = email {
                    // Backfill the email column, and replace labels generated by
                    // older zai logins ("AutoClaw (...)" / "Z.ai (...)") with the
                    // address itself. Hand-written labels are left alone.
                    let _ = conn.execute(
                        "UPDATE accounts SET \
                             email = COALESCE(NULLIF(email, ''), ?1), \
                             label = CASE \
                                 WHEN label IS NULL OR TRIM(label) = '' \
                                      OR label LIKE 'AutoClaw (%' OR label LIKE 'Z.ai (%' \
                                 THEN ?1 ELSE label END \
                         WHERE id = ?2",
                        rusqlite::params![email, account_id.0],
                    );
                }
            }

            Ok::<(), CoreError>(())
        })
        .await
        .map_err(|e| CoreError::Internal(format!("zai post_exchange spawn join error: {e}")))??;

        Ok(())
    }
    fn request_device_code(
        &self,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> impl std::future::Future<Output = Result<DeviceAuthorizationResponse>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "Z.ai uses Authorization Code flow, not Device Code".into(),
        )))
    }

    fn poll_device_token(
        &self,
        _device_code: &str,
        _upstream_client: &Arc<UpstreamClient>,
    ) -> impl std::future::Future<Output = Result<Option<TokenResponse>>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "Z.ai uses Authorization Code flow, not Device Code".into(),
        )))
    }

    fn supports_refresh(&self) -> bool {
        true
    }

    fn access_tokens_expire(&self) -> bool {
        true
    }

    /// Renew the AutoClaw session against the identity gateway.
    ///
    /// The access token is a 24h JWT and the refresh grant survives 30 days
    /// without rotating, so this is the steady-state loop that keeps an account
    /// alive. Rejections come back as HTTP 200 with a business `code`, never as
    /// a transport error, so a non-zero envelope is reported as an auth failure
    /// rather than a parse one.
    ///
    /// The gateway also keeps the AutoClaw credential inside
    /// `oauth_provider_specific`, which the generic refresh path does not
    /// rebuild, so the renewed token is merged into that blob here and handed
    /// back through [`Self::refreshed_provider_specific`]. Unknown keys are
    /// preserved: the daily check-in bookkeeping lives next to the credential.
    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        _account_id: AccountId,
        _db: DbRef<'_>,
    ) -> Result<TokenResponse> {
        let raw_refresh = strip_bearer(refresh_token).ok_or_else(|| {
            CoreError::Validation("zai refresh: no AutoClaw refresh token stored".into())
        })?;

        let body = serde_json::json!({
            "refresh_token": raw_refresh,
            "source_id": "autoclaw",
            "device_id": AUTOCLAW_DEVICE_ID,
        });
        let body_bytes = serde_json::to_vec(&body)
            .map_err(|e| CoreError::Validation(format!("zai refresh encode: {e}")))?;

        let mut req =
            UpstreamRequest::post_json(ZAI_AUTOCLAW_REFRESH_URL, bytes::Bytes::from(body_bytes))
                .with_accept_json();
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("AutoClaw/2.0.3"),
        );
        apply_autoclaw_identity_headers(&mut req);

        let (_status, resp_bytes) =
            crate::oauth::call_oauth(upstream_client, req, "zai autoclaw refresh").await?;

        let json: serde_json::Value = serde_json::from_slice(&resp_bytes)
            .map_err(|e| CoreError::Parse(format!("zai refresh parse: {e}")))?;

        let code = json
            .get("code")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(-1);
        if code != 0 {
            let msg = json
                .get("msg")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("unknown");
            return Err(CoreError::Auth(format!(
                "zai refresh rejected by AutoClaw (code {code}): {msg}"
            )));
        }

        let data = json.get("data").ok_or_else(|| {
            CoreError::Parse("zai refresh: response carried no data envelope".into())
        })?;
        let new_access = data
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                CoreError::Parse("zai refresh: response carried no access_token".into())
            })?
            .to_string();
        // A successful renewal echoes the same grant back. Keep whatever came
        // back so a future rotation is picked up instead of silently ignored.
        let new_refresh = data
            .get("refresh_token")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string);

        // The response carries no `expires_in`; the JWT's own `exp` is the only
        // deadline, and the coordinator reads it straight off the token.
        let expires_in = openproxy_adapters::jwt::claim_i64(&new_access, "exp")
            .zip(openproxy_adapters::jwt::claim_i64(&new_access, "iat"))
            .and_then(|(exp, iat)| u64::try_from(exp - iat).ok());

        Ok(TokenResponse {
            access_token: new_access,
            token_type: "_fb30a070Bearer".into(),
            expires_in,
            refresh_token: new_refresh,
            scope: None,
            id_token: None,
        })
    }

    fn provider_specific_from_token(&self, token: &TokenResponse) -> Option<String> {
        token.id_token.clone()
    }

    /// Rewrite the AutoClaw credential inside `oauth_provider_specific`.
    ///
    /// The generic refresh path stores `access_token_encrypted` and leaves this
    /// blob alone, but the AutoClaw check-in reads `autoclaw_access_token` from
    /// it. Without this write the session would renew for proxying while the
    /// check-in kept using the dead token.
    ///
    /// The blob is read as a plain JSON value and only the two credential keys
    /// are touched, so everything else in it (including the daily check-in
    /// bookkeeping) survives untouched.
    async fn persist_refreshed_state(
        &self,
        account_id: AccountId,
        db: DbRef<'_>,
        master_key: &MasterKey,
        token: &TokenResponse,
    ) -> Result<()> {
        let new_access = token.access_token.clone();
        let raw_refresh = token
            .refresh_token
            .as_deref()
            .and_then(strip_bearer)
            .unwrap_or_default()
            .to_string();
        if new_access.trim().is_empty() || raw_refresh.is_empty() {
            return Ok(());
        }

        let id = account_id;
        let write_master_key = master_key.clone();
        let write_access = new_access.clone();
        let write_refresh = raw_refresh.clone();
        db.with_conn_async(move |conn| {
            let encrypted: Option<String> = conn
                .query_row(
                    "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
                    rusqlite::params![id.0],
                    |row| row.get(0),
                )
                .ok()
                .flatten();

            // Fail closed when the blob does not decrypt: the same column also
            // carries the OAuth credential, so a blind write could destroy it.
            let Some(existing) = openproxy_db::accounts::oauth::decrypt_oauth_provider_specific(
                encrypted.as_deref(),
                &write_master_key,
            ) else {
                return Ok(());
            };

            let mut value: serde_json::Value = serde_json::from_str(&existing)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));
            let Some(obj) = value.as_object_mut() else {
                return Ok(());
            };
            obj.insert(
                "autoclaw_access_token".to_string(),
                serde_json::Value::String(write_access),
            );
            obj.insert(
                "autoclaw_refresh_token".to_string(),
                serde_json::Value::String(write_refresh),
            );

            let serialized = serde_json::to_string(&value)
                .map_err(|e| CoreError::Parse(format!("zai persist encode: {e}")))?;
            let encrypted_new = openproxy_db::accounts::oauth::encrypt_oauth_provider_specific(
                &serialized,
                &write_master_key,
            )?;
            conn.execute(
                "UPDATE accounts SET oauth_provider_specific = ?1 WHERE id = ?2",
                rusqlite::params![encrypted_new, id.0],
            )
            .map_err(|e| CoreError::Internal(format!("zai persist write: {e}")))?;
            Ok(())
        })
        .await
    }

    fn email_from_token(&self, token: &TokenResponse) -> Option<String> {
        let from_id_token = token
            .id_token
            .as_deref()
            .and_then(|raw| serde_json::from_str::<ZaiAccountMeta>(raw).ok())
            .and_then(|m| m.email)
            .filter(|e| !e.trim().is_empty());
        // AutoClaw logins carry no id_token; the address rides in the access
        // token's `jti` claim instead.
        from_id_token.or_else(|| {
            openproxy_adapters::jwt::claim_str(&token.access_token, "jti")
                .filter(|e| e.contains('@'))
        })
    }
}

pub(crate) mod exchange;

#[cfg(test)]
#[path = "zai/tests.rs"]
mod tests;
