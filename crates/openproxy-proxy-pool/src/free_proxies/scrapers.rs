//! Upstream scrapers and parsers for free proxy providers.

use super::models::{SHARED_PROXY_CLIENT, ScrapedProxy};
use openproxy_adapters::upstream::{TimeoutProfile, UpstreamRequest};
use std::sync::Arc;

#[derive(serde::Deserialize)]
struct ProxiflyGeo {
    country: Option<String>,
}

#[derive(serde::Deserialize)]
struct ProxiflyItem {
    ip: String,
    port: u16,
    protocol: String,
    geolocation: Option<ProxiflyGeo>,
}

pub(crate) async fn fetch_upstream_json<T: serde::de::DeserializeOwned>(
    url: &str,
    name: &str,
) -> crate::error::Result<T> {
    let client = &*SHARED_PROXY_CLIENT;
    let req = UpstreamRequest::get(url);
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let res = client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("{name} HTTP error: {e:?}")))?;

    if res.status != 200 {
        return Err(crate::error::CoreError::Internal(format!(
            "{name} HTTP status: {}",
            res.status
        )));
    }

    let body_bytes = res
        .collect()
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("{name} body error: {e:?}")))?;
    serde_json::from_slice(&body_bytes)
        .map_err(|e| crate::error::CoreError::Internal(format!("{name} JSON error: {e}")))
}

pub async fn sync_proxifly(url: &str) -> crate::error::Result<Vec<ScrapedProxy>> {
    let items: Vec<ProxiflyItem> = fetch_upstream_json(url, "Proxifly").await?;

    let list = items
        .into_iter()
        .map(|item| {
            let country_code = item
                .geolocation
                .and_then(|g| g.country)
                .filter(|c| !c.is_empty());
            ScrapedProxy {
                source: "proxifly".to_string(),
                host: item.ip,
                port: item.port,
                r#type: item.protocol.to_lowercase(),
                country_code,
                username: None,
                password: None,
                priority: 0,
            }
        })
        .collect();
    Ok(list)
}

pub fn parse_proxy_host_port(proxy: &str) -> Option<(String, u16)> {
    let (host, port_str) = proxy.rsplit_once(':')?;
    let host = host.trim();
    let port = port_str.trim().parse::<u16>().ok()?;
    if host.is_empty() || port == 0 {
        None
    } else {
        Some((host.to_string(), port))
    }
}

pub fn parse_plain_proxy_lines(text: &str, src_name: &str, proto: &str) -> Vec<ScrapedProxy> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                return None;
            }
            let (host, port) = parse_proxy_host_port(trimmed)?;
            Some(ScrapedProxy {
                source: src_name.to_string(),
                host,
                port,
                r#type: proto.to_string(),
                country_code: None,
                username: None,
                password: None,
                priority: 0,
            })
        })
        .collect()
}

async fn fetch_github_proxy_file(
    client: &Arc<openproxy_adapters::upstream::UpstreamClient>,
    src_name: &str,
    proto: &str,
    url: &str,
) -> Vec<ScrapedProxy> {
    let req = openproxy_adapters::upstream::UpstreamRequest::get(url);
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let Ok(res) = client
        .call(
            req,
            openproxy_adapters::upstream::TimeoutProfile::ModelDiscovery,
            cancel,
        )
        .await
    else {
        return Vec::new();
    };
    if res.status != 200 {
        return Vec::new();
    }
    let Ok(body_bytes) = res.collect().await else {
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&body_bytes);
    parse_plain_proxy_lines(&text, src_name, proto)
}

pub async fn sync_github_lists(base_url: &str) -> crate::error::Result<Vec<ScrapedProxy>> {
    let client = &*SHARED_PROXY_CLIENT;
    let mut list = Vec::new();
    let sources: &[(&str, &str, &[&str])] = &[
        (
            "iplocate",
            "{base}/iplocate/free-proxy-list/main/protocols/{}.txt",
            &["http", "https", "socks4", "socks5"],
        ),
        (
            "hideip",
            "{base}/zloi-user/hideip.me/main/{}.txt",
            &["http", "socks4", "socks5"],
        ),
        (
            "r00tee",
            "{base}/r00tee/Proxy-List/main/Socks5.txt",
            &["socks5"],
        ),
        (
            "hookzof",
            "{base}/hookzof/socks5_list/master/proxy.txt",
            &["socks5"],
        ),
        (
            "anonymouswork",
            "{base}/Anonym0usWork1221/Free-Proxies/main/proxy_files/https_proxies.txt",
            &["https"],
        ),
        (
            "komutan234",
            "{base}/komutan234/Proxy-List-Free/main/proxies/http.txt",
            &["http"],
        ),
        (
            "yuceltoluyag",
            "{base}/yuceltoluyag/GoodProxy/main/raw.txt",
            &["http"],
        ),
    ];

    for &(src_name, url_template, protocols) in sources {
        for &proto in protocols {
            let url = url_template
                .replace("{base}", base_url)
                .replace("{}", proto);
            let mut proxies = fetch_github_proxy_file(client, src_name, proto, &url).await;
            list.append(&mut proxies);
        }
    }
    Ok(list)
}

#[derive(serde::Deserialize)]
struct OneProxyApiProxy {
    ip: String,
    port: u16,
    protocol: Option<String>,
    country_code: Option<String>,
}

#[derive(serde::Deserialize)]
struct OneProxyApiResponse {
    proxies: Option<Vec<OneProxyApiProxy>>,
}

pub async fn sync_oneproxy(url: &str) -> crate::error::Result<Vec<ScrapedProxy>> {
    let body: OneProxyApiResponse = fetch_upstream_json(url, "1proxy").await?;

    let proxies = body.proxies.unwrap_or_default();
    let list = proxies
        .into_iter()
        .map(|p| ScrapedProxy {
            source: "1proxy".to_string(),
            host: p.ip,
            port: p.port,
            r#type: p
                .protocol
                .unwrap_or_else(|| "http".to_string())
                .to_lowercase(),
            country_code: p.country_code.filter(|c| !c.is_empty()),
            username: None,
            password: None,
            priority: 0,
        })
        .collect();
    Ok(list)
}

#[derive(serde::Deserialize)]
struct ProxyScrapeCdnItem {
    ip: String,
    port: u16,
    protocol: String,
    country_code: Option<String>,
}

pub async fn sync_proxyscrape_cdn(url: &str) -> crate::error::Result<Vec<ScrapedProxy>> {
    let items: Vec<ProxyScrapeCdnItem> = fetch_upstream_json(url, "ProxyScrape CDN").await?;

    let list = items
        .into_iter()
        .map(|item| ScrapedProxy {
            source: "proxyscrape_cdn".to_string(),
            host: item.ip,
            port: item.port,
            r#type: item.protocol.to_lowercase(),
            country_code: item.country_code.filter(|c| !c.is_empty()),
            username: None,
            password: None,
            priority: 0,
        })
        .collect();
    Ok(list)
}

#[derive(serde::Deserialize)]
struct GeonodeItem {
    ip: String,
    port: String,
    protocols: Vec<String>,
    country: Option<String>,
}

#[derive(serde::Deserialize)]
struct GeonodeResponse {
    data: Vec<GeonodeItem>,
}

pub async fn sync_geonode(url: &str) -> crate::error::Result<Vec<ScrapedProxy>> {
    let client = &*SHARED_PROXY_CLIENT;
    let mut req = UpstreamRequest::get(url);
    req.headers.insert(
        http::header::ACCEPT,
        http::HeaderValue::from_static("application/json"),
    );
    req.headers.insert(
        http::header::USER_AGENT,
        http::HeaderValue::from_static("Mozilla/5.0 (X11; Linux x86_64)"),
    );
    let cancel = openproxy_adapters::upstream::CancellationToken::new();
    let res = client
        .call(req, TimeoutProfile::ModelDiscovery, cancel)
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("Geonode HTTP error: {e:?}")))?;

    if res.status != 200 {
        return Err(crate::error::CoreError::Internal(format!(
            "Geonode HTTP status: {}",
            res.status
        )));
    }

    let body_bytes = res
        .collect()
        .await
        .map_err(|e| crate::error::CoreError::Internal(format!("Geonode body error: {e:?}")))?;
    let body: GeonodeResponse = serde_json::from_slice(&body_bytes)
        .map_err(|e| crate::error::CoreError::Internal(format!("Geonode JSON error: {e}")))?;

    let list = body
        .data
        .into_iter()
        .filter_map(|item| {
            let port = item.port.parse::<u16>().ok()?;
            let proto = item
                .protocols
                .into_iter()
                .next()
                .unwrap_or_else(|| "http".to_string());
            Some(ScrapedProxy {
                source: "geonode".to_string(),
                host: item.ip,
                port,
                r#type: proto.to_lowercase(),
                country_code: item.country.filter(|c| !c.is_empty()),
                username: None,
                password: None,
                priority: 0,
            })
        })
        .collect();
    Ok(list)
}

#[derive(serde::Deserialize)]
struct ClearProxyItem {
    ip: String,
    port: u16,
    protocol: String,
    country_code: Option<String>,
}

pub async fn sync_clearproxy(url: &str) -> crate::error::Result<Vec<ScrapedProxy>> {
    let items: Vec<ClearProxyItem> = fetch_upstream_json(url, "ClearProxy").await?;

    let list = items
        .into_iter()
        .map(|item| ScrapedProxy {
            source: "clearproxy".to_string(),
            host: item.ip,
            port: item.port,
            r#type: item.protocol.to_lowercase(),
            country_code: item.country_code.filter(|c| !c.is_empty()),
            username: None,
            password: None,
            priority: 0,
        })
        .collect();
    Ok(list)
}

#[derive(serde::Deserialize)]
struct VakhovItem {
    ip: String,
    port: serde_json::Value,
    country_code: Option<String>,
}

pub fn parse_vakhov_port(port: &serde_json::Value) -> Option<u16> {
    match port {
        serde_json::Value::Number(n) => n.as_u64().map(|v| v as u16),
        serde_json::Value::String(s) => s.parse::<u16>().ok(),
        _ => None,
    }
}

pub async fn sync_vakhov(url: &str) -> crate::error::Result<Vec<ScrapedProxy>> {
    let items: Vec<VakhovItem> = fetch_upstream_json(url, "Vakhov").await?;

    let list = items
        .into_iter()
        .filter_map(|item| {
            let port = parse_vakhov_port(&item.port)?;
            Some(ScrapedProxy {
                source: "vakhov".to_string(),
                host: item.ip,
                port,
                r#type: "http".to_string(),
                country_code: item.country_code.filter(|c| !c.is_empty()),
                username: None,
                password: None,
                priority: 0,
            })
        })
        .collect();
    Ok(list)
}

#[derive(serde::Deserialize)]
struct GProxyNetItem {
    proxy: String,
    protocol: Option<String>,
    country: Option<String>,
}

pub async fn sync_gproxynet(url: &str) -> crate::error::Result<Vec<ScrapedProxy>> {
    let items: Vec<GProxyNetItem> = fetch_upstream_json(url, "GProxyNet").await?;

    let list = items
        .into_iter()
        .filter_map(|item| {
            let (host, port) = parse_proxy_host_port(&item.proxy)?;
            let proto = item.protocol.unwrap_or_else(|| "http".to_string());
            Some(ScrapedProxy {
                source: "gproxynet".to_string(),
                host,
                port,
                r#type: proto.to_lowercase(),
                country_code: item.country.filter(|c| !c.is_empty()),
                username: None,
                password: None,
                priority: 0,
            })
        })
        .collect();
    Ok(list)
}

pub fn parse_custom_proxy_auth(auth_part: Option<&str>) -> (Option<String>, Option<String>) {
    match auth_part {
        Some(auth) => match auth.split_once(':') {
            Some((u, p)) => (Some(u.trim().to_string()), Some(p.trim().to_string())),
            None => (Some(auth.trim().to_string()), None),
        },
        None => (None, None),
    }
}

pub fn parse_custom_proxy_line(
    line: &str,
    source_name: &str,
    priority: i32,
) -> Option<ScrapedProxy> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }

    let (proto, host_port) = trimmed.split_once("://").unwrap_or(("http", trimmed));
    let (raw_host, rest) = host_port.split_once(':')?;
    let (raw_port, auth_part) = rest
        .split_once(':')
        .map_or((rest, None), |(p, a)| (p, Some(a)));

    let host = raw_host.trim().to_string();
    let port = raw_port.trim().parse::<u16>().ok()?;
    if host.is_empty() || port == 0 {
        return None;
    }

    let (username, password) = parse_custom_proxy_auth(auth_part);
    Some(ScrapedProxy {
        source: source_name.to_string(),
        host,
        port,
        r#type: proto.to_lowercase(),
        country_code: None,
        username,
        password,
        priority,
    })
}
