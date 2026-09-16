//! Provider registry CRUD and domain utilities.

use crate::error::Result;
use crate::ids::ProviderId;
pub use openproxy_db::providers::{
    NewProvider, UpdateProviderParams, create, delete, get, get_auth_types, list, list_active,
    set_active, set_favicon, update, update_current_proxy,
};
pub use openproxy_types::providers::*;

pub use crate::seed::{builtin_provider_ids, is_builtin};

/// Extract clean domain/host from a base_url string.
pub fn extract_domain(base_url: &str) -> Option<String> {
    let trimmed = base_url.trim();
    let stripped = trimmed
        .strip_prefix("https://")
        .or_else(|| trimmed.strip_prefix("http://"))
        .unwrap_or(trimmed);
    let host_part = stripped.split_once('/').map_or(stripped, |(h, _)| h);
    let host = host_part
        .split_once(':')
        .map_or(host_part, |(h, _)| h)
        .trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

fn is_compound_tld(second_to_last: &str, last: &str) -> bool {
    matches!(second_to_last, "co" | "com" | "org" | "net" | "gov" | "edu") && last.len() == 2
}

/// Extract apex/root domain from host (e.g. "api.fireworks.ai" -> "fireworks.ai").
pub fn extract_apex_domain(host: &str) -> String {
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() <= 2 {
        return host.to_string();
    }
    let second_to_last = parts[parts.len() - 2];
    let last = parts[parts.len() - 1];
    if is_compound_tld(second_to_last, last) && parts.len() >= 3 {
        parts[parts.len() - 3..].join(".")
    } else {
        parts[parts.len() - 2..].join(".")
    }
}

/// Fetch the favicon for a provider from multiple fallback sources
/// as a data URI (`data:image/png;base64,...`).
pub async fn fetch_favicon_data_uri(
    base_url: &str,
    upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
) -> Option<String> {
    let host = extract_domain(base_url)?;
    let apex = extract_apex_domain(&host);

    let mut domains = vec![host.clone()];
    if apex != host {
        domains.push(apex);
    }

    async fn try_fetch_b64(
        upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
        url: &str,
        mime: &str,
    ) -> Option<String> {
        let bytes = openproxy_adapters::adapters::upstream_get_bytes(upstream_client, url, &[])
            .await
            .ok()?;
        if bytes.len() > 100 {
            use base64::Engine;
            let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
            Some(format!("data:{mime};base64,{b64}"))
        } else {
            None
        }
    }

    for domain in domains {
        if let Some(data) = try_fetch_b64(
            upstream_client,
            &format!("https://www.google.com/s2/favicons?domain={domain}&sz=64"),
            "image/png",
        )
        .await
        {
            return Some(data);
        }

        if let Some(data) = try_fetch_b64(
            upstream_client,
            &format!("https://icons.duckduckgo.com/ip3/{domain}.ico"),
            "image/x-icon",
        )
        .await
        {
            return Some(data);
        }

        if let Some(data) = try_fetch_b64(
            upstream_client,
            &format!("https://{domain}/favicon.ico"),
            "image/x-icon",
        )
        .await
        {
            return Some(data);
        }
    }

    None
}

/// Fetch the favicon for a provider and store it in the database.
pub async fn fetch_and_cache_favicon(
    db_pool: &std::sync::Arc<openproxy_db::conn::DbPool>,
    id: &ProviderId,
    base_url: &str,
    upstream_client: &std::sync::Arc<openproxy_adapters::upstream::UpstreamClient>,
) -> Result<()> {
    if let Some(data_uri) = fetch_favicon_data_uri(base_url, upstream_client).await {
        let pool = std::sync::Arc::clone(db_pool);
        let id_clone = id.clone();
        tokio::task::spawn_blocking(move || {
            let conn = pool.writer();
            set_favicon(&conn, &id_clone, &data_uri)
        })
        .await
        .map_err(|e| openproxy_types::error::CoreError::Internal(format!("join error: {e}")))??;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
