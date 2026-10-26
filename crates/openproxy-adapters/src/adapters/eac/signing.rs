//! EAC Gateway HMAC-SHA256 request signing.
//!
//! Provides header signature generation:
//! - `x-ofm-timestamp`: unix timestamp in milliseconds.
//! - `x-ofm-signature`: hex(HMAC-SHA256(secret, `${timestamp}\n${METHOD}\n${pathname}\n${sha256(body)}`)).

use sha2::{Digest, Sha256};
use std::borrow::Cow;

pub const DEFAULT_EAC_SECRET: &str =
    "2b3a996145fd07a4973bdbc0dac4ee6eae59275f9c75f42487d66b5208ac6fbc";
pub const HEADER_OFM_TIMESTAMP: &str = "x-ofm-timestamp";
pub const HEADER_OFM_SIGNATURE: &str = "x-ofm-signature";
pub const HEADER_OFM_USER: &str = "x-ofm-user";
pub const DEFAULT_EAC_USER_AGENT: &str = "dsh-our-free-model";

/// Resolves the HMAC secret with precedence:
/// 1. Explicitly configured string (if non-empty)
/// 2. `OPENPROXY_EAC_SECRET` env var
/// 3. `EAC_HMAC_SECRET` env var
/// 4. `DEFAULT_EAC_SECRET`
pub fn resolve_eac_secret(configured: Option<&str>) -> Cow<'static, str> {
    if let Some(s) = configured.map(str::trim).filter(|s| !s.is_empty()) {
        return Cow::Owned(s.to_string());
    }
    if let Ok(env) = std::env::var("OPENPROXY_EAC_SECRET")
        && !env.trim().is_empty()
    {
        return Cow::Owned(env.trim().to_string());
    }
    if let Ok(env) = std::env::var("EAC_HMAC_SECRET")
        && !env.trim().is_empty()
    {
        return Cow::Owned(env.trim().to_string());
    }
    Cow::Borrowed(DEFAULT_EAC_SECRET)
}

pub use openproxy_types::to_hex_lower;

/// HMAC-SHA256 (RFC 2104 / RFC 4231) over `data` using `key`.
#[inline]
pub fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64; // SHA-256 block size
    let mut key_block = [0u8; BLOCK];
    if key.len() > BLOCK {
        key_block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        key_block[..key.len()].copy_from_slice(key);
    }
    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= key_block[i];
        opad[i] ^= key_block[i];
    }
    let inner = Sha256::new()
        .chain_update(ipad)
        .chain_update(data)
        .finalize();
    Sha256::new()
        .chain_update(opad)
        .chain_update(inner)
        .finalize()
        .into()
}

/// Extracts pathname from an absolute or relative URL.
///
/// Examples:
/// - `"https://ourfreemodel.beichenapi.com/eac/v1/chat/completions"` -> `"/eac/v1/chat/completions"`
/// - `"/eac/v1/chat/completions"` -> `"/eac/v1/chat/completions"`
/// - `"https://ourfreemodel.beichenapi.com"` -> `"/"`
#[inline]
pub fn extract_pathname(url: &str) -> &str {
    let after_scheme = if let Some(idx) = url.find("://") {
        &url[idx + 3..]
    } else {
        url
    };
    let path_start = after_scheme.find('/').unwrap_or(after_scheme.len());
    let path = &after_scheme[path_start..];
    let path = if path.is_empty() { "/" } else { path };
    let end = path.find(['?', '#']).unwrap_or(path.len());
    &path[..end]
}

/// Computes the EAC signature:
/// `hex(HMAC-SHA256(secret, "${timestamp}\n${METHOD}\n${pathname}\n${sha256(body)}"))`
pub fn compute_eac_signature(
    secret: &str,
    timestamp_ms: u64,
    method: &str,
    pathname: &str,
    body: &[u8],
) -> String {
    let body_hash = Sha256::digest(body);
    let body_hash_hex = to_hex_lower(&body_hash);
    let method_upper = method.to_ascii_uppercase();
    let payload = format!("{timestamp_ms}\n{method_upper}\n{pathname}\n{body_hash_hex}");
    let signature_bytes = hmac_sha256(secret.as_bytes(), payload.as_bytes());
    to_hex_lower(&signature_bytes)
}

/// Injects or updates `x-ofm-timestamp` and `x-ofm-signature` in outgoing request headers.
/// Also strips any transient `x-ofm-secret` or `x-eac-secret` header.
pub fn sign_request_headers(
    default_secret: &str,
    method: &str,
    url: &str,
    headers: &mut Vec<(String, String)>,
    body: &[u8],
) {
    let mut custom_secret = None;
    let mut extracted_token = None;
    let mut existing_user = None;
    let mut found_user_agent = false;

    for (k, v) in headers.iter() {
        if k.eq_ignore_ascii_case("authorization") {
            let s = v.trim();
            if let Some(t) = s
                .strip_prefix("Bearer ")
                .or_else(|| s.strip_prefix("bearer "))
            {
                let tok = t.trim();
                if !tok.is_empty() {
                    extracted_token = Some(tok.to_string());
                }
            } else if !s.is_empty() {
                extracted_token = Some(s.to_string());
            }
        } else if k.eq_ignore_ascii_case(HEADER_OFM_USER) {
            let s = v.trim();
            if !s.is_empty() {
                existing_user = Some(s.to_string());
            }
        } else if k.eq_ignore_ascii_case("user-agent") {
            found_user_agent = true;
        }
    }

    headers.retain(|(k, v)| {
        if k.eq_ignore_ascii_case("x-ofm-secret") || k.eq_ignore_ascii_case("x-eac-secret") {
            if custom_secret.is_none() && !v.trim().is_empty() {
                custom_secret = Some(v.trim().to_string());
            }
            false
        } else {
            true
        }
    });

    let effective_user = existing_user.or(extracted_token);
    if let Some(user_tok) = effective_user {
        let mut user_updated = false;
        for (k, v) in headers.iter_mut() {
            if k.eq_ignore_ascii_case(HEADER_OFM_USER) {
                *v = user_tok.clone();
                user_updated = true;
                break;
            }
        }
        if !user_updated {
            headers.push((HEADER_OFM_USER.to_string(), user_tok));
        }
    }

    if !found_user_agent {
        headers.push(("user-agent".to_string(), DEFAULT_EAC_USER_AGENT.to_string()));
    }

    let secret = resolve_eac_secret(custom_secret.as_deref().or(Some(default_secret)));
    let pathname = extract_pathname(url);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);

    let signature = compute_eac_signature(&secret, now_ms, method, pathname, body);
    let ts_str = now_ms.to_string();

    let mut found_ts = false;
    let mut found_sig = false;
    for (k, v) in headers.iter_mut() {
        if k.eq_ignore_ascii_case(HEADER_OFM_TIMESTAMP) {
            *v = ts_str.clone();
            found_ts = true;
        } else if k.eq_ignore_ascii_case(HEADER_OFM_SIGNATURE) {
            *v = signature.clone();
            found_sig = true;
        }
    }

    if !found_ts {
        headers.push((HEADER_OFM_TIMESTAMP.to_string(), ts_str));
    }
    if !found_sig {
        headers.push((HEADER_OFM_SIGNATURE.to_string(), signature));
    }
}
