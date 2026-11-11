//! Construction of authenticated CodeBuddy quota HTTP requests.

use super::apply_codebuddy_spoofing_headers;
use crate::adapters::UpstreamRequest;

use super::CODEBUDDY_ACCOUNTS_URL;

#[must_use]
pub fn build_codebuddy_resource_request(
    url: &str,
    token: &str,
    proxy_url: Option<&str>,
) -> UpstreamRequest {
    let mut req = UpstreamRequest::post_json(url, bytes::Bytes::from_static(b"{}"));
    req.proxy = proxy_url.map(ToString::to_string);

    let trimmed = token.trim();
    let auth_val = if trimmed.starts_with("Bearer ") {
        trimmed.to_string()
    } else {
        crate::upstream::format_bearer(trimmed)
    };
    if let Ok(hv) = http::HeaderValue::from_str(&auth_val) {
        req.headers.insert(http::header::AUTHORIZATION, hv);
    }
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json, text/plain, */*"),
    );
    if let Ok(uri) = url.parse::<http::Uri>()
        && let Some(host) = uri.host()
        && let Ok(val) = http::HeaderValue::from_str(host)
    {
        req.headers
            .insert(http::HeaderName::from_static("x-domain"), val);
    }

    apply_codebuddy_spoofing_headers(&mut req);
    req
}

#[must_use]
pub fn build_codebuddy_accounts_request_with_url(
    url: &str,
    token: &str,
    proxy_url: Option<&str>,
) -> UpstreamRequest {
    let mut req = UpstreamRequest::get_json(url);
    req.proxy = proxy_url.map(ToString::to_string);

    let trimmed = token.trim();
    let auth_val = if trimmed.starts_with("Bearer ") {
        trimmed.to_string()
    } else {
        crate::upstream::format_bearer(trimmed)
    };
    if let Ok(hv) = http::HeaderValue::from_str(&auth_val) {
        req.headers.insert(http::header::AUTHORIZATION, hv);
    }
    req.headers.insert(
        http::HeaderName::from_static("x-no-enterprise-id"),
        http::HeaderValue::from_static("true"),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-no-user-id"),
        http::HeaderValue::from_static("true"),
    );
    req.headers.insert(
        http::HeaderName::from_static("x-no-department-info"),
        http::HeaderValue::from_static("true"),
    );
    if let Ok(uri) = url.parse::<http::Uri>()
        && let Some(host) = uri.host()
        && let Ok(val) = http::HeaderValue::from_str(host)
    {
        req.headers
            .insert(http::HeaderName::from_static("x-domain"), val);
    }

    apply_codebuddy_spoofing_headers(&mut req);
    req
}

#[must_use]
pub fn build_codebuddy_accounts_request(token: &str, proxy_url: Option<&str>) -> UpstreamRequest {
    build_codebuddy_accounts_request_with_url(CODEBUDDY_ACCOUNTS_URL, token, proxy_url)
}
