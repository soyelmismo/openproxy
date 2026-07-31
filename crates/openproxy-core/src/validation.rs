use openproxy_types::error::{CoreError, Result};

/// Trait for input payloads that can validate their fields before processing.
pub trait Validatable {
    fn validate(&self) -> Result<()>;
}

/// Validate that a `base_url` is a well-formed HTTP(S) URL with a non-empty host.
pub fn validate_base_url(url: &str) -> Result<()> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(CoreError::Validation(format!(
            "base_url must start with http:// or https://, got: {url}"
        )));
    }
    let remainder = &url[url.find("://").unwrap() + 3..];
    let host_end = remainder.find('/').unwrap_or(remainder.len());
    let host_part = &remainder[..host_end];
    let host = if let Some(colon_pos) = host_part.rfind(':') {
        &host_part[..colon_pos]
    } else {
        host_part
    };
    if host.is_empty() {
        return Err(CoreError::Validation(format!(
            "base_url must have a non-empty host, got: {url}"
        )));
    }
    Ok(())
}
