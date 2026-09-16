//! Domain models and URL constants for free proxies and sources.

use openproxy_adapters::upstream::UpstreamClient;
use std::sync::{Arc, LazyLock};

pub static SHARED_PROXY_CLIENT: LazyLock<Arc<UpstreamClient>> = LazyLock::new(UpstreamClient::new);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FreeProxy {
    pub id: String,
    pub source: String,
    pub host: String,
    pub port: u16,
    pub r#type: String,
    pub country_code: Option<String>,
    pub status: String,
    pub latency_ms: Option<i64>,
    pub last_validated: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub priority: i32,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct ScrapedProxy {
    pub source: String,
    pub host: String,
    pub port: u16,
    pub r#type: String,
    pub country_code: Option<String>,
    pub username: Option<String>,
    pub password: Option<String>,
    pub priority: i32,
}

// Module-scope upstream URL constants.
pub const PROXIFLY_URL: &str = "https://api.proxifly.dev/proxy?format=json&quantity=100";
pub const ONEPROXY_URL: &str = "https://1proxy-api.aitradepulse.com/api/v1/proxies/advanced";
pub const PROXYSCRAPE_CDN_URL: &str =
    "https://cdn.jsdelivr.net/gh/proxyscrape/free-proxy-list@main/proxies/all/data.json";
pub const GEONODE_URL: &str =
    "https://proxylist.geonode.com/api/proxy-list?limit=500&sort_by=lastChecked&sort_type=desc";
pub const CLEARPROXY_URL: &str =
    "https://raw.githubusercontent.com/ClearProxy/checked-proxy-list/main/http/json/all.json";
pub const VAKHOV_URL: &str = "https://vakhov.github.io/fresh-proxy-list/proxylist.json";
pub const GPROXYNET_URL: &str =
    "https://raw.githubusercontent.com/gproxynet/free-proxy-list/main/proxies.json";
pub const GITHUB_LISTS_BASE_URL: &str = "https://raw.githubusercontent.com";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProxySource {
    pub id: String,
    pub name: String,
    pub url: String,
    pub priority: i32,
    pub active: bool,
    pub is_builtin: bool,
    pub proxies_total: i64,
    pub proxies_alive: i64,
    pub proxies_dead: i64,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CreateProxySourceInput {
    pub name: String,
    pub url: String,
    pub priority: Option<i32>,
    pub active: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UpdateProxySourceInput {
    pub name: Option<String>,
    pub url: Option<String>,
    pub priority: Option<i32>,
    pub active: Option<bool>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SyncSummary {
    pub fetched: usize,
    pub added: usize,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProxySummary {
    pub total: usize,
    pub alive: usize,
    pub dead: usize,
    pub unknown: usize,
    pub avg_latency_ms: Option<u32>,
    pub sources: Vec<String>,
    pub protocols: Vec<String>,
}
