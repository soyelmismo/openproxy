use openproxy_types::error::{CoreError, Result};

/// Trait for input payloads that can validate their fields before processing.
pub trait Validatable {
    fn validate(&self) -> Result<()>;
}

pub fn validate_base_url(url: &str) -> Result<()> {
    let Some((scheme, remainder)) = url.split_once("://") else {
        return Err(CoreError::Validation(format!(
            "base_url must start with http:// or https://, got: {url}"
        )));
    };
    if scheme != "http" && scheme != "https" {
        return Err(CoreError::Validation(format!(
            "base_url must start with http:// or https://, got: {url}"
        )));
    }
    let host_part = remainder.split('/').next().unwrap_or(remainder);
    let host = host_part.split(':').next().unwrap_or(host_part);
    if host.is_empty() {
        return Err(CoreError::Validation(format!(
            "base_url must have a non-empty host, got: {url}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_base_url_valid() {
        assert!(validate_base_url("http://example.com").is_ok());
        assert!(validate_base_url("https://example.com").is_ok());
        assert!(validate_base_url("https://example.com:8080").is_ok());
        assert!(validate_base_url("https://example.com/path").is_ok());
        assert!(validate_base_url("https://example.com:8080/path").is_ok());
    }

    #[test]
    fn test_validate_base_url_invalid_scheme() {
        assert!(validate_base_url("ftp://example.com").is_err());
        assert!(validate_base_url("example.com").is_err());
        assert!(validate_base_url("").is_err());
    }

    #[test]
    fn test_validate_base_url_empty_host() {
        assert!(validate_base_url("https://").is_err());
        assert!(validate_base_url("http://").is_err());
        assert!(validate_base_url("https:///path").is_err());
        assert!(validate_base_url("https://:8080").is_err());
    }
}
