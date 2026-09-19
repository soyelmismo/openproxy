//! MiniMax Coding OAuth 2.0 provider (Device Authorization Grant + PKCE).
//!
//! Implements RFC 8628 with SHA256 PKCE against MiniMax's public OAuth endpoints:
//! - Device Authorization: `https://account.minimax.io/oauth2/device/code` (Global) / `.cn` (China)
//! - Token & Refresh: `https://account.minimax.io/oauth2/token`
//! - Matrix Client attribution: first-party signed headers (`x-signature`, `yy`, `x-timestamp`)
//! - Daily Check-in & Quota synchronization.

pub mod checkin;
pub mod identity;
pub mod matrix;
pub mod md5;

pub use identity::{resolve_membership_info, resolve_user_identity};

#[cfg(test)]
mod tests;

use dashmap::DashMap;
use rusqlite::OptionalExtension;
use std::sync::{Arc, LazyLock};
use std::time::Instant;

use self::matrix::MiniMaxRegion;
use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::generic::{code_challenge_s256, generate_code_verifier, urlencoded_body};
use crate::oauth::{
    DbRef, DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse,
    decode_jwt_payload, map_upstream_err,
};
use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use openproxy_db::secrets::MasterKey;
use serde::{Deserialize, Serialize};

pub const CLIENT_ID: &str = "mcode-public";
pub const SCOPE: &str = "agent.default";
pub const AUDIENCE: &str = "agent-backend";
pub const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";

struct PendingDeviceAuth {
    verifier: String,
    region: MiniMaxRegion,
    created_at: Instant,
    uses_user_code: bool,
    user_code: String,
}

static PENDING_AUTH: LazyLock<DashMap<String, PendingDeviceAuth>> = LazyLock::new(DashMap::new);

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MiniMaxAccountMeta {
    pub real_user_id: Option<String>,
    pub op_group_id: Option<String>,
    pub region: Option<String>,
    pub token_plan_tier: Option<String>,
    pub last_checkin_date: Option<String>,
    pub streak_days: Option<u8>,
    #[serde(default)]
    pub credit_balance: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

#[derive(Clone, Default)]
pub struct MiniMaxOAuthProvider;

impl MiniMaxOAuthProvider {
    pub fn new() -> Self {
        Self
    }
}

impl OAuthProvider for MiniMaxOAuthProvider {
    fn name(&self) -> &'static str {
        "minimax"
    }

    fn aliases(&self) -> &'static [&'static str] {
        &["minimax-coding", "minimax-managed", "minimax-cn"]
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::DeviceCode
    }

    fn build_auth_url(
        &self,
        _redirect_uri: &str,
    ) -> impl std::future::Future<Output = Result<(String, String, String, String)>> + Send {
        std::future::ready(Err(CoreError::Validation(
            "minimax uses device code flow, not PKCE authorization URL".into(),
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
            "minimax uses device code flow, not authorization code".into(),
        )))
    }

    async fn request_device_code(
        &self,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<DeviceAuthorizationResponse> {
        clean_expired_pending_auth();

        let region = resolve_default_region();
        let endpoint = format!("{}/oauth2/device/code", region.resolved_account_origin());

        let verifier = generate_code_verifier();
        let challenge = code_challenge_s256(&verifier);

        let params = [
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("audience", AUDIENCE),
            ("code_challenge", challenge.as_str()),
            ("code_challenge_method", "S256"),
        ];

        let body = urlencoded_body(&params);
        let mut req = UpstreamRequest::post_json(&endpoint, body);
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("MiniMaxCode"),
        );

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "minimax device code request"))?;

        let status = response.status;
        let body_bytes = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "minimax device code read"))?;

        if !status.is_success() {
            let err_str = String::from_utf8_lossy(&body_bytes).to_string();
            return Err(CoreError::upstream_error(
                status.as_u16(),
                "minimax",
                "<oauth_device_code>",
                err_str,
                false,
            ));
        }

        let json: serde_json::Value = serde_json::from_slice(&body_bytes)
            .map_err(|e| CoreError::Parse(format!("minimax device auth parse: {e}")))?;

        let user_code = json
            .get("user_code")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| CoreError::Parse("minimax device auth missing user_code".into()))?
            .to_string();

        let standard_device_code = json
            .get("device_code")
            .and_then(serde_json::Value::as_str)
            .map(std::string::ToString::to_string);

        let is_user_code_polling = standard_device_code.is_none();
        let device_code = standard_device_code.unwrap_or_else(|| user_code.clone());

        let verification_uri = json
            .get("verification_uri")
            .or_else(|| json.get("verification_url"))
            .and_then(serde_json::Value::as_str)
            .map_or_else(
                || region.resolved_account_origin(),
                std::string::ToString::to_string,
            );

        let verification_uri_complete = json
            .get("verification_uri_complete")
            .and_then(serde_json::Value::as_str)
            .map(|s| {
                if !s.contains("client_surface") {
                    let sep = if s.contains('?') { '&' } else { '?' };
                    format!("{s}{sep}client_surface=tui&download_source=mcode-internal")
                } else {
                    s.to_string()
                }
            })
            .or_else(|| {
                Some(build_complete_verification_uri(
                    &verification_uri,
                    &user_code,
                ))
            });

        let expires_in = json
            .get("expires_in")
            .or_else(|| json.get("expired_in"))
            .and_then(serde_json::Value::as_u64)
            .or(Some(300));

        let interval = json
            .get("interval")
            .and_then(serde_json::Value::as_u64)
            .or(Some(5));

        PENDING_AUTH.insert(
            device_code.clone(),
            PendingDeviceAuth {
                verifier: verifier.clone(),
                region,
                created_at: Instant::now(),
                uses_user_code: is_user_code_polling,
                user_code: user_code.clone(),
            },
        );

        // Also track user_code in case polling uses user_code
        PENDING_AUTH.insert(
            user_code.clone(),
            PendingDeviceAuth {
                verifier,
                region,
                created_at: Instant::now(),
                uses_user_code: true,
                user_code: user_code.clone(),
            },
        );

        Ok(DeviceAuthorizationResponse {
            device_code,
            user_code,
            verification_uri,
            verification_uri_complete,
            expires_in,
            interval,
        })
    }

    async fn poll_device_token(
        &self,
        device_code: &str,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<Option<TokenResponse>> {
        let (verifier, region, uses_user_code, user_code_val) =
            PENDING_AUTH.get(device_code).map_or_else(
                || {
                    let is_uc = device_code.contains('-') && device_code.len() <= 12;
                    (
                        String::new(),
                        resolve_default_region(),
                        is_uc,
                        device_code.to_string(),
                    )
                },
                |entry| {
                    (
                        entry.verifier.clone(),
                        entry.region,
                        entry.uses_user_code,
                        entry.user_code.clone(),
                    )
                },
            );

        let endpoint = format!("{}/oauth2/token", region.resolved_account_origin());
        let code_key = if uses_user_code {
            "user_code"
        } else {
            "device_code"
        };
        let code_val = if uses_user_code && !user_code_val.is_empty() {
            user_code_val.as_str()
        } else {
            device_code
        };

        let (status, body_bytes) =
            poll_token_request(upstream_client, &endpoint, code_key, code_val, &verifier).await?;

        // If 400 with invalid_request, try alternative parameter key (user_code <-> device_code)
        let (status, body_bytes) = if status.as_u16() == 400 {
            if let Ok(err_json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                let error = err_json.get("error").and_then(serde_json::Value::as_str);
                if error == Some("invalid_request") {
                    let alt_key = if code_key == "user_code" {
                        "device_code"
                    } else {
                        "user_code"
                    };
                    poll_token_request(upstream_client, &endpoint, alt_key, code_val, &verifier)
                        .await?
                } else {
                    (status, body_bytes)
                }
            } else {
                (status, body_bytes)
            }
        } else {
            (status, body_bytes)
        };

        // 400 or 428 standard pending responses
        if status.as_u16() == 400 || status.as_u16() == 428 {
            if let Ok(err_json) = serde_json::from_slice::<serde_json::Value>(&body_bytes) {
                let error = err_json.get("error").and_then(serde_json::Value::as_str);
                let poll_status = err_json.get("status").and_then(serde_json::Value::as_str);
                if matches!(error, Some("authorization_pending" | "slow_down"))
                    || matches!(poll_status, Some("pending" | "slow_down"))
                {
                    return Ok(None);
                }
                if matches!(error, Some("access_denied" | "expired_token"))
                    || matches!(
                        poll_status,
                        Some("denied" | "access_denied" | "expired" | "expired_token")
                    )
                {
                    return Err(CoreError::Validation(format!(
                        "MiniMax device authorization {}",
                        error.or(poll_status).unwrap_or("failed")
                    )));
                }
            }
            return Ok(None);
        }

        if !status.is_success() {
            let err_str = String::from_utf8_lossy(&body_bytes).to_string();
            return Err(CoreError::upstream_error(
                status.as_u16(),
                "minimax",
                "<oauth_device_poll>",
                err_str,
                false,
            ));
        }

        let json: serde_json::Value = serde_json::from_slice(&body_bytes)
            .map_err(|e| CoreError::Parse(format!("minimax token parse: {e}")))?;

        if let Some(error) = json.get("error").and_then(serde_json::Value::as_str) {
            if error == "authorization_pending" || error == "slow_down" {
                return Ok(None);
            }
            return Err(CoreError::Validation(format!(
                "MiniMax device authorization failed: {error}"
            )));
        }

        let token: TokenResponse = serde_json::from_value(json)
            .map_err(|e| CoreError::Parse(format!("minimax token response parse: {e}")))?;

        PENDING_AUTH.remove(device_code);
        Ok(Some(token))
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
        account_id: AccountId,
        db: DbRef<'_>,
    ) -> Result<TokenResponse> {
        let region = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
                    rusqlite::params![account_id.0],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map_err(openproxy_db::error::map_db_error_ctx(
                    "get provider_specific",
                ))
            })?
            .flatten()
            .and_then(|raw| serde_json::from_str::<MiniMaxAccountMeta>(&raw).ok())
            .and_then(|meta| meta.region)
            .map_or_else(resolve_default_region, |r| MiniMaxRegion::parse_str(&r));

        let endpoint = format!("{}/oauth2/token", region.resolved_account_origin());

        let params = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
            ("scope", SCOPE),
            ("audience", AUDIENCE),
        ];

        let body = urlencoded_body(&params);
        let mut req = UpstreamRequest::post_json(&endpoint, body);
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );
        req.headers.insert(
            http::header::ACCEPT,
            http::HeaderValue::from_static("application/json"),
        );
        req.headers.insert(
            http::header::USER_AGENT,
            http::HeaderValue::from_static("MiniMaxCode"),
        );

        let cancel = CancellationToken::new();
        let response = upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
            .map_err(|e| map_upstream_err(e, "minimax token refresh"))?;

        let status = response.status;
        let body_bytes = response
            .collect()
            .await
            .map_err(|e| map_upstream_err(e, "minimax token refresh read"))?;

        if !status.is_success() {
            let err_str = String::from_utf8_lossy(&body_bytes).to_string();
            return Err(CoreError::upstream_error(
                status.as_u16(),
                "minimax",
                "<oauth_refresh_token>",
                err_str,
                false,
            ));
        }

        serde_json::from_slice::<TokenResponse>(&body_bytes)
            .map_err(|e| CoreError::Parse(format!("minimax refresh token parse: {e}")))
    }

    fn email_from_token(&self, token: &TokenResponse) -> Option<String> {
        decode_jwt_payload(&token.access_token).and_then(|claims| {
            claims
                .get("email")
                .or_else(|| claims.get("user_email"))
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(std::string::ToString::to_string)
        })
    }

    fn provider_specific_from_token(&self, token: &TokenResponse) -> Option<String> {
        let region = resolve_default_region();
        let real_user_id = decode_jwt_payload(&token.access_token).and_then(|claims| {
            claims
                .get("sub")
                .or_else(|| claims.get("account_id"))
                .or_else(|| claims.get("user_id"))
                .and_then(serde_json::Value::as_str)
                .map(std::string::ToString::to_string)
        });
        let email = self.email_from_token(token);

        let meta = MiniMaxAccountMeta {
            real_user_id,
            op_group_id: None,
            region: Some(match region {
                MiniMaxRegion::Global => "en".into(),
                MiniMaxRegion::China => "cn".into(),
            }),
            token_plan_tier: None,
            last_checkin_date: None,
            streak_days: None,
            credit_balance: None,
            email,
        };

        serde_json::to_string(&meta).ok()
    }

    async fn post_exchange(
        &self,
        account_id: AccountId,
        db_pool: &Arc<openproxy_db::DbPool>,
        master_key: &MasterKey,
        upstream: &Arc<UpstreamClient>,
    ) -> Result<()> {
        let (access_token, mut current_meta) = {
            let pool = Arc::clone(db_pool);
            let key = master_key.clone();
            tokio::task::spawn_blocking(move || -> Result<(String, MiniMaxAccountMeta)> {
                let conn = pool.reader();
                let token = crate::accounts::decrypt_access_token(&conn, account_id, &key)?;
                let specific_raw: Option<String> = conn
                    .query_row(
                        "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
                        rusqlite::params![account_id.0],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(openproxy_db::error::map_db_error_ctx("read specific"))?
                    .flatten();

                let meta = specific_raw
                    .and_then(|raw| serde_json::from_str::<MiniMaxAccountMeta>(&raw).ok())
                    .unwrap_or_default();

                Ok((token, meta))
            })
            .await
            .map_err(|e| CoreError::Internal(format!("spawn failed: {e}")))??
        };

        let region = current_meta
            .region
            .as_deref()
            .map_or_else(resolve_default_region, MiniMaxRegion::parse_str);

        // 1. Resolve user identity (real_user_id + email + display_name)
        let identity = resolve_user_identity(upstream, &access_token, region).await;

        let real_user_id = current_meta
            .real_user_id
            .clone()
            .or(identity.real_user_id.clone())
            .unwrap_or_else(|| "0".into());
        current_meta.real_user_id = Some(real_user_id.clone());

        if let Some(ref em) = identity.email {
            current_meta.email = Some(em.clone());
        }

        // 2. Perform initial checkin right upon login (claims points if claimable)
        if let Ok(summary) =
            checkin::execute_daily_checkin(upstream, &access_token, &real_user_id, region).await
        {
            tracing::info!(
                account_id = account_id.0,
                streak = summary.streak_days,
                points = summary.points_claimed,
                "MiniMax post_exchange checkin: {}",
                summary.message
            );
            current_meta.streak_days = Some(summary.streak_days);
            current_meta.last_checkin_date =
                Some(chrono::Utc::now().format("%Y-%m-%d").to_string());
        }

        // 3. Resolve op_group_id, workspace tier & credits (after checkin so newly claimed points are included)
        if let Some((op_group_id, tier, credits)) =
            resolve_membership_info(upstream, &access_token, &real_user_id, region).await
        {
            current_meta.op_group_id = Some(op_group_id);
            if tier.is_some() {
                current_meta.token_plan_tier = tier;
            }
            if credits.is_some() {
                current_meta.credit_balance = credits;
            }
        }

        // 4. Save updated metadata, email, and display label to DB
        let meta_json = serde_json::to_string(&current_meta)
            .map_err(|e| CoreError::Parse(format!("serialize meta: {e}")))?;

        let final_email = current_meta.email.clone();
        let display_label = identity.display_label();
        let final_label = if display_label.is_empty() {
            format!("MiniMax User {real_user_id}")
        } else {
            display_label
        };

        let pool = Arc::clone(db_pool);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let conn = pool
                .try_writer_for(openproxy_db::conn::ADMIN_LOCK_TIMEOUT)
                .ok_or_else(|| CoreError::Internal("writer timeout".into()))?;
            conn.execute(
                "UPDATE accounts SET oauth_provider_specific = ?1, email = COALESCE(?2, email), \
                 label = COALESCE(NULLIF(label, ''), ?3) WHERE id = ?4",
                rusqlite::params![meta_json, final_email, final_label, account_id.0],
            )
            .map_err(openproxy_db::error::map_db_error_ctx(
                "update provider_specific + label",
            ))?;
            Ok(())
        })
        .await
        .map_err(|e| CoreError::Internal(format!("spawn failed: {e}")))??;

        Ok(())
    }
}

fn resolve_default_region() -> MiniMaxRegion {
    std::env::var("MINIMAX_REGION")
        .or_else(|_| std::env::var("MINIMAX_OAUTH_REGION"))
        .map_or(MiniMaxRegion::Global, |s| MiniMaxRegion::parse_str(&s))
}

fn clean_expired_pending_auth() {
    PENDING_AUTH.retain(|_, val| val.created_at.elapsed().as_secs() < 600);
}

pub fn build_complete_verification_uri(base_uri: &str, user_code: &str) -> String {
    let sep = if base_uri.contains('?') { '&' } else { '?' };
    format!(
        "{base_uri}{sep}user_code={user_code}&client_surface=tui&download_source=mcode-internal"
    )
}

async fn poll_token_request(
    upstream_client: &Arc<UpstreamClient>,
    endpoint: &str,
    code_key: &str,
    code_val: &str,
    verifier: &str,
) -> Result<(http::StatusCode, Vec<u8>)> {
    let params = [
        ("grant_type", DEVICE_GRANT_TYPE),
        (code_key, code_val),
        ("client_id", CLIENT_ID),
        ("code_verifier", verifier),
    ];
    let body = urlencoded_body(&params);
    let mut req = UpstreamRequest::post_json(endpoint, body);
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/x-www-form-urlencoded"),
    );
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("MiniMaxCode"),
    );

    let cancel = CancellationToken::new();
    let response = upstream_client
        .call(req, TimeoutProfile::OAuth, cancel)
        .await
        .map_err(|e| map_upstream_err(e, "minimax device poll"))?;

    let status = response.status;
    let body_bytes = response
        .collect()
        .await
        .map_err(|e| map_upstream_err(e, "minimax device poll read"))?
        .to_vec();

    Ok((status, body_bytes))
}
