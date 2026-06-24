//! Kiro AI (AWS SSO OIDC) OAuth provider.
//!
//! Uses the Device Authorization Grant (RFC 8628) against AWS's OIDC
//! endpoints. Client registration is dynamic via the client register endpoint.
//!
//! After a successful token exchange the provider calls
//! `ListAvailableProfiles` (the same `codewhisperer` endpoint the chat
//! executor uses) to bootstrap the per-account `profileArn` and stores
//! it in `accounts.oauth_provider_specific` as JSON:
//! `{"profileArn": "...", "clientId": "...", "clientSecret": "...",
//! "region": "us-east-1"}`. The chat executor reads this field and
//! embeds it in every upstream request.

use async_trait::async_trait;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};
use crate::ids::AccountId;
use crate::oauth::{DeviceAuthorizationResponse, OAuthFlow, OAuthProvider, TokenResponse};
use crate::secrets::MasterKey;
use crate::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamError, UpstreamRequest,
};
use std::sync::Arc;

/// AWS SSO OIDC endpoints.
const REGISTER_URL: &str = "https://oidc.us-east-1.amazonaws.com/client/register";
const DEVICE_AUTH_URL: &str = "https://oidc.us-east-1.amazonaws.com/device/authorization";
const TOKEN_URL: &str = "https://oidc.us-east-1.amazonaws.com/token";

/// AWS region Kiro is pinned to by default. Surfaced in
/// `oauth_provider_specific` so the chat executor can route the
/// eventual chat request to the same regional endpoint.
const DEFAULT_REGION: &str = "us-east-1";

/// CodeWhisperer `ListAvailableProfiles` endpoint. Same host the chat
/// executor will eventually call for the `generateAssistantResponse`
/// streaming call.
const LIST_PROFILES_URL: &str = "https://codewhisperer.us-east-1.amazonaws.com/";

/// Kiro-specific scopes.
const SCOPES: &[&str] = &["codewhisperer:completions", "codewhisperer:analysis"];

/// Client registration request body.
#[derive(Debug, Serialize)]
struct RegisterClientRequest {
    #[serde(rename = "clientName")]
    client_name: String,
    #[serde(rename = "clientType")]
    client_type: String,
    scopes: Vec<String>,
    #[serde(rename = "grantTypes")]
    grant_types: Vec<String>,
}

/// Client registration response.
#[derive(Debug, Deserialize)]
struct RegisterClientResponse {
    #[serde(rename = "clientId")]
    client_id: String,
    #[allow(dead_code)]
    #[serde(rename = "clientSecret")]
    client_secret: String,
}

/// Stored Kiro provider metadata in `accounts.oauth_provider_specific`.
///
/// `client_id` / `client_secret` are the OIDC credentials returned
/// by the dynamic client register call (RFC 7591). `profile_arn` is
/// the user's CodeWhisperer profile (added by `post_exchange`).
/// `region` is the AWS region the chat executor should target
/// (default: us-east-1).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KiroProviderMeta {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    #[serde(default = "default_region")]
    pub region: String,
}

fn default_region() -> String {
    DEFAULT_REGION.to_string()
}

/// Most-recent OIDC client registration. The admin handler reads
/// this back from `oauth_kiro::take_last_client` after
/// `request_device_code` returns and stashes the credentials on
/// the account row.
struct LastKiroClient {
    client_id: String,
    client_secret: String,
    stored_at: std::time::Instant,
}

thread_local! {
    static LAST_KIRO_CLIENT: std::cell::RefCell<Option<LastKiroClient>> = const { std::cell::RefCell::new(None) };
}

/// Staleness window for the OIDC-credentials cache. After this
/// many seconds a `take_last_client` call returns `None` so an
/// abandoned browser tab cannot drive a different user into
/// authentication. The default device-code `expires_in` is 1800s;
/// 60s is conservative enough to cover the round-trip from the
/// user completing the browser-side auth back to the device-poll
/// landing on the same process.
const LAST_KIRO_CLIENT_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// Read-and-clear the most recently registered Kiro OIDC client,
/// if it was registered within `LAST_KIRO_CLIENT_TTL`. Returns
/// `None` when no client was registered or the entry is stale.
pub fn take_last_client() -> Option<(String, String)> {
    LAST_KIRO_CLIENT.with(|cell| {
        let mut slot = cell.borrow_mut();
        let entry = slot.take()?;
        if entry.stored_at.elapsed() > LAST_KIRO_CLIENT_TTL {
            return None;
        }
        Some((entry.client_id, entry.client_secret))
    })
}

/// Build an `application/x-www-form-urlencoded` body from a list of
/// `(name, value)` pairs. Mirrors the helper in `oauth_antigravity.rs`
/// (kept here to avoid a cross-module dep — the two providers have
/// no shared helper module today).
fn urlencoded_body(params: &[(&str, &str)]) -> bytes::Bytes {
    let mut s = String::new();
    for (i, (k, v)) in params.iter().enumerate() {
        if i > 0 {
            s.push('&');
        }
        s.push_str(&urlencoding::encode(k));
        s.push('=');
        s.push_str(&urlencoding::encode(v));
    }
    bytes::Bytes::from(s)
}

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

#[async_trait]
impl OAuthProvider for KiroOAuthProvider {
    fn name(&self) -> &str {
        "kiro"
    }

    fn flow(&self) -> OAuthFlow {
        OAuthFlow::DeviceCode
    }

    async fn build_auth_url(&self, _redirect_uri: &str) -> Result<(String, String, String)> {
        Err(CoreError::Validation(
            "kiro uses device code flow, not PKCE".into(),
        ))
    }

    async fn exchange_code(
        &self,
        _code: &str,
        _code_verifier: &str,
        _upstream_client: &Arc<UpstreamClient>,
        _redirect_uri: &str,
    ) -> Result<TokenResponse> {
        Err(CoreError::Validation(
            "kiro uses device code flow, not authorization code".into(),
        ))
    }

    async fn request_device_code(
        &self,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<DeviceAuthorizationResponse> {
        // Step 1: Register a dynamic OIDC client.
        let register_body = serde_json::to_vec(&RegisterClientRequest {
            client_name: "openproxy-kiro".into(),
            client_type: "public".into(),
            scopes: SCOPES.iter().map(|s| s.to_string()).collect(),
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
            .call(register_req, TimeoutProfile::OAuth, cancel.clone())
            .await;
        let register_response = match register_response {
            Ok(r) => r,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => {
                        CoreError::UpstreamConnection(format!("kiro client register: {other}"))
                    }
                });
            }
        };

        let register_status = register_response.status;
        let register_body = match register_response.collect().await {
            Ok(b) => b,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => {
                        CoreError::UpstreamConnection(format!("kiro register body read: {other}"))
                    }
                });
            }
        };
        if !register_status.is_success() {
            let body_str = String::from_utf8_lossy(&register_body).to_string();
            return Err(CoreError::UpstreamError {
                status: register_status.as_u16(),
                provider: "kiro".into(),
                model: "<oauth>".into(),
                body: body_str,
            });
        }

        let client: RegisterClientResponse = serde_json::from_slice(&register_body)
            .map_err(|e| CoreError::Parse(format!("kiro register response parse: {e}")))?;

        // Step 2: Request device authorization.
        // AWS SSO OIDC expects JSON with clientId, clientSecret, and startUrl.
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
            .call(device_auth_req, TimeoutProfile::OAuth, cancel)
            .await;
        let device_auth_response = match device_auth_response {
            Ok(r) => r,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => {
                        CoreError::UpstreamConnection(format!("kiro device authorization: {other}"))
                    }
                });
            }
        };

        let device_auth_status = device_auth_response.status;
        let device_auth_body = match device_auth_response.collect().await {
            Ok(b) => b,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => CoreError::UpstreamConnection(format!(
                        "kiro device auth body read: {other}"
                    )),
                });
            }
        };
        if !device_auth_status.is_success() {
            let body_str = String::from_utf8_lossy(&device_auth_body).to_string();
            return Err(CoreError::UpstreamError {
                status: device_auth_status.as_u16(),
                provider: "kiro".into(),
                model: "<oauth>".into(),
                body: body_str,
            });
        }

        let dar: DeviceAuthorizationResponse = serde_json::from_slice(&device_auth_body)
            .map_err(|e| CoreError::Parse(format!("kiro device auth response parse: {e}")))?;

        // Stash the freshly-registered OIDC credentials on a
        // thread-local cell so the admin handler can persist
        // them on the account row before the user finishes the
        // device verification. The chat executor will read them
        // back from `oauth_provider_specific` later. A short
        // 60-second TTL means a stale entry from an abandoned
        // browser tab cannot be picked up by a different user's
        // poll.
        LAST_KIRO_CLIENT.with(|cell| {
            *cell.borrow_mut() = Some(LastKiroClient {
                client_id: client.client_id,
                client_secret: client.client_secret,
                stored_at: std::time::Instant::now(),
            });
        });

        Ok(dar)
    }

    async fn poll_device_token(
        &self,
        device_code: &str,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<Option<TokenResponse>> {
        // Read OIDC client credentials from the thread-local cache
        // (stashed by request_device_code). AWS SSO OIDC requires them.
        let (cid, csec) = crate::oauth_kiro::take_last_client().unwrap_or_default();
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
        let response = match upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => CoreError::UpstreamConnection(format!("kiro device poll: {other}")),
                });
            }
        };

        let status = response.status;
        let body = match response.collect().await {
            Ok(b) => b,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => CoreError::UpstreamConnection(format!(
                        "kiro device poll body read: {other}"
                    )),
                });
            }
        };

        if status.as_u16() == 400 || status.as_u16() == 428 {
            // Authorization_pending or similar — caller should retry.
            return Ok(None);
        }

        if !status.is_success() {
            let body_str = String::from_utf8_lossy(&body).to_string();
            return Err(CoreError::UpstreamError {
                status: status.as_u16(),
                provider: "kiro".into(),
                model: "<oauth>".into(),
                body: body_str,
            });
        }

        serde_json::from_slice::<TokenResponse>(&body)
            .map(Some)
            .map_err(|e| CoreError::Parse(format!("kiro token parse: {e}")))
    }

    async fn refresh_token(
        &self,
        refresh_token: &str,
        upstream_client: &Arc<UpstreamClient>,
    ) -> Result<TokenResponse> {
        let body = urlencoded_body(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
        ]);
        let mut req = UpstreamRequest::post_json(TOKEN_URL, body);
        req.headers.insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/x-www-form-urlencoded"),
        );

        let cancel = CancellationToken::new();
        let response = match upstream_client
            .call(req, TimeoutProfile::OAuth, cancel)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => CoreError::UpstreamConnection(format!("kiro token refresh: {other}")),
                });
            }
        };

        let status = response.status;
        let body = match response.collect().await {
            Ok(b) => b,
            Err(e) => {
                return Err(match e {
                    UpstreamError::Cancel => CoreError::ClientDisconnected,
                    other => CoreError::UpstreamConnection(format!(
                        "kiro token refresh body read: {other}"
                    )),
                });
            }
        };

        if !status.is_success() {
            let body_str = String::from_utf8_lossy(&body).to_string();
            return Err(CoreError::UpstreamError {
                status: status.as_u16(),
                provider: "kiro".into(),
                model: "<oauth>".into(),
                body: body_str,
            });
        }

        serde_json::from_slice::<TokenResponse>(&body)
            .map_err(|e| CoreError::Parse(format!("kiro token refresh parse: {e}")))
    }

    async fn post_exchange(
        &self,
        account_id: AccountId,
        db_pool: &std::sync::Arc<crate::db::DbPool>,
        master_key: &MasterKey,
    ) -> Result<()> {
        // 1. Decrypt the access token we just stored + read the
        //    existing OIDC meta in a single critical section. The
        //    writer guard is dropped at the end of the block so the
        //    next `.await` (the listAvailableProfiles HTTP call)
        //    is `Send`.
        let (access_token, mut meta) = {
            let conn = db_pool.writer();
            let access_token =
                crate::accounts::decrypt_access_token(&conn, account_id, master_key)?;

            // Read the OIDC credentials that the device-code flow
            // stashed in `oauth_provider_specific`. They are
            // required for the `x-amz-user-agent` header and the
            // chat executor's eventual request envelope.
            let raw: Option<String> = conn
                .query_row(
                    "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
                    params![account_id.0],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| CoreError::Database {
                    message: format!(
                        "kiro post_exchange read meta for account {}: {}",
                        account_id.0, e
                    ),
                    source: Some(Box::new(e)),
                })?;

            let meta: KiroProviderMeta = match raw {
                Some(s) => serde_json::from_str(&s)
                    .map_err(|e| CoreError::Parse(format!("kiro meta parse: {e}")))?,
                None => {
                    // The device-code flow normally writes the meta first
                    // (with `client_id` / `client_secret` but no
                    // `profile_arn`). Defensive: if the column is NULL
                    // we treat it as an empty meta so the post-exchange
                    // surface stays well-defined.
                    KiroProviderMeta::default()
                }
            };

            (access_token, meta)
        };

        // 2. Hit `ListAvailableProfiles` and pick the first profile
        //    (the user may own several; Kiro CLI defaults to the
        //    first one in the array). If the list is empty we keep
        //    the row as-is — the user can re-link later from the
        //    dashboard.
        match list_available_profiles(&access_token).await? {
            Some(arn) => {
                meta.profile_arn = Some(arn);
            }
            None => {
                tracing::info!(
                    account = account_id.0,
                    "kiro post_exchange: no profiles available; profileArn left empty"
                );
            }
        }

        // 3. Persist the updated meta. The `client_id` /
        //    `client_secret` survive the round-trip so the chat
        //    executor can read them later.
        let meta_json = serde_json::to_string(&meta)
            .map_err(|e| CoreError::Internal(format!("kiro meta serialize: {e}")))?;
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
            source: Some(Box::new(e)),
        })?;

        Ok(())
    }
}

/// Call `ListAvailableProfiles` and return the first `arn` (or `None`
/// when the user owns zero profiles).
async fn list_available_profiles(access_token: &str) -> Result<Option<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| CoreError::UpstreamConnection(format!("kiro profiles client: {e}")))?;

    let body = serde_json::json!({ "origin": "AI_EDITOR" });

    let resp = client
        .post(LIST_PROFILES_URL)
        .bearer_auth(access_token)
        .header("Content-Type", "application/json")
        // The `x-amz-user-agent` header is the same one Kiro's CLI
        // sends; the executor will use the same string later.
        .header("x-amz-user-agent", "aws-sdk-js/3.0.0 kiro/0.1")
        .json(&body)
        .send()
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("kiro listAvailableProfiles: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let let_body = resp.text().await.unwrap_or_default();
        return Err(CoreError::UpstreamError {
            status,
            provider: "kiro".into(),
            model: "<post_exchange>".into(),
            body: let_body,
        });
    }

    let value: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| CoreError::Parse(format!("kiro listAvailableProfiles parse: {e}")))?;

    // Upstream returns `{"profiles": [{"arn": "...", ...}, ...]}`.
    // Some older versions use `profileArn`; accept both.
    let arn = value
        .get("profiles")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|p| {
            p.get("arn")
                .or_else(|| p.get("profileArn"))
                .and_then(|v| v.as_str())
        })
        .map(|s| s.to_string());

    Ok(arn)
}

/// Read the `profileArn` stored on the account row by `post_exchange`.
pub fn read_profile_meta(
    conn: &Connection,
    account_id: AccountId,
) -> Result<Option<KiroProviderMeta>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
            params![account_id.0],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| CoreError::Database {
            message: format!("kiro read meta for account {}: {}", account_id.0, e),
            source: Some(Box::new(e)),
        })?;

    let Some(raw) = raw else { return Ok(None) };
    let meta: KiroProviderMeta = serde_json::from_str(&raw)
        .map_err(|e| CoreError::Parse(format!("kiro meta parse: {e}")))?;
    Ok(Some(meta))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_and_flow() {
        let p = KiroOAuthProvider::new();
        assert_eq!(p.name(), "kiro");
        assert_eq!(p.flow(), OAuthFlow::DeviceCode);
    }

    #[test]
    fn kiro_provider_meta_serde_roundtrip() {
        let meta = KiroProviderMeta {
            client_id: "test-client-id".into(),
            client_secret: "test-client-secret".into(),
            profile_arn: Some("arn:aws:codewhisperer:us-east-1:123:profile/abc".into()),
            region: "us-east-1".into(),
        };
        let json = serde_json::to_string(&meta).unwrap();
        let back: KiroProviderMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back.client_id, "test-client-id");
        assert_eq!(back.client_secret, "test-client-secret");
        assert_eq!(
            back.profile_arn.as_deref(),
            Some("arn:aws:codewhisperer:us-east-1:123:profile/abc")
        );
        assert_eq!(back.region, "us-east-1");
    }

    #[test]
    fn kiro_provider_meta_default_region() {
        // When the on-disk JSON omits the `region` field, the
        // deserializer must default to us-east-1 (the only region
        // Kiro currently ships with).
        let raw = r#"{"client_id":"c","client_secret":"s"}"#;
        let meta: KiroProviderMeta = serde_json::from_str(raw).unwrap();
        assert_eq!(meta.region, "us-east-1");
        assert!(meta.profile_arn.is_none());
    }
}
