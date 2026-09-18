//! MiniMax Matrix / Public Gateway client attribution and request signing.
//!
//! Replicates first-party signatures (`x-signature`, `yy`, `x-timestamp`)
//! and URL query parameters used by MiniMax Code.

use super::md5::md5_hex;
use openproxy_adapters::upstream::UpstreamRequest;
use serde::{Deserialize, Serialize};

/// Supported MiniMax geographic regions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MiniMaxRegion {
    #[default]
    Global,
    China,
}

impl MiniMaxRegion {
    pub fn parse_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "cn" | "china" | "minimax-cn" => Self::China,
            _ => Self::Global,
        }
    }

    pub fn account_origin(self) -> &'static str {
        match self {
            Self::Global => "https://account.minimax.io",
            Self::China => "https://account.minimax.cn",
        }
    }

    pub fn gateway_origin(self) -> &'static str {
        match self {
            Self::Global => "https://agent.minimax.io",
            Self::China => "https://agent.minimax.cn",
        }
    }

    pub fn platform_origin(self) -> &'static str {
        match self {
            Self::Global => "https://platform.minimax.io",
            Self::China => "https://platform.minimaxi.com",
        }
    }

    pub fn llm_base_url(self) -> &'static str {
        match self {
            Self::Global => "https://agent.minimax.io/mavis/api/v1/llm/v1",
            Self::China => "https://agent.minimax.cn/mavis/api/v1/llm/v1",
        }
    }

    pub fn language_code(self) -> &'static str {
        match self {
            Self::Global => "en",
            Self::China => "zh",
        }
    }
}

/// Constructs the Matrix Public Gateway URL with required client tracking query parameters.
pub fn build_gateway_url(
    region: MiniMaxRegion,
    pathname: &str,
    user_id: &str,
    now_ms: u64,
) -> (String, String) {
    let origin = region.gateway_origin();
    let lang = region.language_code();
    let clean_path = if pathname.starts_with('/') {
        pathname
    } else {
        &format!("/{pathname}")
    };

    let user_id_clean = if user_id.trim().is_empty() {
        "0"
    } else {
        user_id.trim()
    };

    let query = format!(
        "device_platform=mcode&biz_id=3&app_id=3001&version_code=22201&unix={now_ms}\
         &timezone_offset=0&sys_language={lang}&lang={lang}&device_id=0&os_name=linux\
         &browser_name=mcode&user_id={user_id_clean}&client=mcode"
    );

    let path_with_query = format!("{clean_path}?{query}");
    let full_url = format!("{origin}{path_with_query}");
    (full_url, path_with_query)
}

/// Signs a Matrix gateway request and attaches headers.
pub fn apply_matrix_headers(
    req: &mut UpstreamRequest,
    path_with_query: &str,
    body: Option<&str>,
    token: &str,
    now_ms: u64,
) {
    let second = now_ms / 1000;
    let sig_body = body.unwrap_or("");
    let yy_body = if sig_body.is_empty() { "{}" } else { sig_body };

    // x-signature: MD5("${second}I*7Cf%WZ#S&%1RlZJ&C2${signatureBody}")
    let sig_input = format!("{second}I*7Cf%WZ#S&%1RlZJ&C2{sig_body}");
    let x_signature = md5_hex(sig_input.as_bytes());

    // yy: MD5("${url_encode(pathWithSearch)}_${yyBody}${MD5(String(nowMs))}ooui")
    let encoded_path = urlencoding::encode(path_with_query);
    let ms_md5 = md5_hex(now_ms.to_string().as_bytes());
    let yy_input = format!("{encoded_path}_{yy_body}{ms_md5}ooui");
    let yy = md5_hex(yy_input.as_bytes());

    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("MiniMaxCode"),
    );

    if let Ok(val) = http::HeaderValue::from_str(&format!("Bearer {token}")) {
        req.headers.insert(http::header::AUTHORIZATION, val);
    }
    if let Ok(val) = http::HeaderValue::from_str(&second.to_string()) {
        req.headers
            .insert(http::HeaderName::from_static("x-timestamp"), val);
    }
    if let Ok(val) = http::HeaderValue::from_str(&x_signature) {
        req.headers
            .insert(http::HeaderName::from_static("x-signature"), val);
    }
    if let Ok(val) = http::HeaderValue::from_str(&yy) {
        req.headers
            .insert(http::HeaderName::from_static("yy"), val);
    }
}

/// Builds a signed Matrix GET request.
pub fn build_matrix_get_request(
    region: MiniMaxRegion,
    path: &str,
    token: &str,
    user_id: &str,
    now_ms: u64,
) -> UpstreamRequest {
    let (url, path_with_query) = build_gateway_url(region, path, user_id, now_ms);
    let mut req = UpstreamRequest::get(&url);
    apply_matrix_headers(&mut req, &path_with_query, None, token, now_ms);
    req
}

/// Builds a signed Matrix POST request.
pub fn build_matrix_post_request(
    region: MiniMaxRegion,
    path: &str,
    token: &str,
    user_id: &str,
    body: &str,
    now_ms: u64,
) -> UpstreamRequest {
    let (url, path_with_query) = build_gateway_url(region, path, user_id, now_ms);
    let mut req = UpstreamRequest::post_json(&url, bytes::Bytes::copy_from_slice(body.as_bytes()));
    apply_matrix_headers(&mut req, &path_with_query, Some(body), token, now_ms);
    req
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_matrix_signing_format() {
        let (full_url, path_with_query) =
            build_gateway_url(MiniMaxRegion::Global, "/api/v1/test", "12345", 1700000000000);
        assert!(full_url.starts_with("https://agent.minimax.io/api/v1/test?"));
        assert!(full_url.contains("user_id=12345"));
        assert!(full_url.contains("client=mcode"));

        let mut req = UpstreamRequest::get(&full_url);
        apply_matrix_headers(&mut req, &path_with_query, None, "test-token", 1700000000000);

        assert!(req.headers.contains_key("x-timestamp"));
        assert!(req.headers.contains_key("x-signature"));
        assert!(req.headers.contains_key("yy"));
        assert_eq!(
            req.headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer test-token")
        );
    }
}
