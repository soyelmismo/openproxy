use openproxy_adapters::upstream::{
    CancellationToken, TimeoutProfile, UpstreamClient, UpstreamRequest,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::error::{CoreError, Result};
use crate::ids::AccountId;

pub const DEFAULT_REGION: &str = "us-east-1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KiroProviderMeta {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_arn: Option<String>,
    #[serde(default = "default_region")]
    pub region: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
}

impl Default for KiroProviderMeta {
    fn default() -> Self {
        Self {
            client_id: String::new(),
            client_secret: String::new(),
            profile_arn: None,
            region: default_region(),
            auth_method: None,
        }
    }
}

pub(crate) fn default_region() -> String {
    DEFAULT_REGION.to_string()
}

pub(crate) async fn list_available_profiles(
    upstream: &Arc<UpstreamClient>,
    access_token: &str,
    region: &str,
) -> Result<Option<String>> {
    let region = if region.is_empty() {
        "us-east-1"
    } else {
        region
    };
    let host = if region == "us-east-1" {
        "https://codewhisperer.us-east-1.amazonaws.com".to_string()
    } else {
        format!("https://q.{region}.amazonaws.com")
    };
    let url = format!("{host}/");

    let body = serde_json::json!({ "maxResults": 10 });
    let body_bytes = serde_json::to_vec(&body)
        .map_err(|e| CoreError::Parse(format!("kiro listAvailableProfiles serialize: {e}")))?;

    let mut req = UpstreamRequest::post_json(&url, bytes::Bytes::from(body_bytes));
    if let Ok(v) = http::HeaderValue::from_str(&format!("Bearer {access_token}")) {
        req.headers.insert(http::header::AUTHORIZATION, v);
    }
    req.headers.insert(
        http::header::HeaderName::from_static("content-type"),
        http::HeaderValue::from_static("application/x-amz-json-1.0"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("accept"),
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-amz-target"),
        http::HeaderValue::from_static("AmazonCodeWhispererService.ListAvailableProfiles"),
    );
    req.headers.insert(
        http::header::HeaderName::from_static("x-amz-user-agent"),
        http::HeaderValue::from_static("aws-sdk-js/3.0.0 kiro/0.1"),
    );
    req.is_streaming = false;

    let cancel = CancellationToken::new();
    let resp = upstream
        .call(req, TimeoutProfile::OAuth, cancel)
        .await
        .map_err(|e| CoreError::UpstreamConnection(format!("kiro listAvailableProfiles: {e}")))?;

    if !resp.status.is_success() {
        let status = resp.status.as_u16();
        let body_str =
            String::from_utf8_lossy(&resp.collect().await.unwrap_or_default()).to_string();
        if status == 403
            || body_str.contains("Builder ID")
            || body_str.contains("AccessDeniedException")
        {
            tracing::info!(
                "Kiro profile ARN discovery returned AccessDenied (likely Builder ID account); proceeding without profile ARN"
            );
            return Ok(None);
        }
        return Err(CoreError::upstream_error(
            status,
            "kiro",
            "<post_exchange>",
            body_str,
            false,
        ));
    }

    let body_bytes = resp.collect().await.map_err(|e| {
        CoreError::UpstreamConnection(format!("kiro listAvailableProfiles read: {e}"))
    })?;

    let value: serde_json::Value = serde_json::from_slice(&body_bytes)
        .map_err(|e| CoreError::Parse(format!("kiro listAvailableProfiles parse: {e}")))?;

    let arn = value
        .get("profiles")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|p| {
                    p.get("arn")
                        .or_else(|| p.get("profileArn"))
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| s.contains(&format!(":{region}:")))
                })
                .or_else(|| arr.first())
        })
        .and_then(|p| {
            p.get("arn")
                .or_else(|| p.get("profileArn"))
                .and_then(|v| v.as_str())
        })
        .map(std::string::ToString::to_string);

    Ok(arn)
}

pub fn read_profile_meta(
    conn: &Connection,
    account_id: AccountId,
) -> Result<Option<KiroProviderMeta>> {
    let raw: Option<Option<String>> = conn
        .query_row(
            "SELECT oauth_provider_specific FROM accounts WHERE id = ?1",
            params![account_id.0],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|e| CoreError::Database {
            message: format!("read kiro meta for account {}: {}", account_id.0, e),
            source: Some(std::sync::Arc::new(e)),
        })?;
    let Some(Some(raw)) = raw else {
        return Ok(Some(KiroProviderMeta::default()));
    };
    if raw.is_empty() {
        return Ok(Some(KiroProviderMeta::default()));
    }
    let mut meta: KiroProviderMeta = serde_json::from_str(&raw)
        .map_err(|e| CoreError::Parse(format!("kiro meta parse: {e}")))?;
    if meta.region.is_empty() {
        meta.region = DEFAULT_REGION.to_string();
    }
    Ok(Some(meta))
}
