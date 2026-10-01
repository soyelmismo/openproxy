//! Builtin proxy source definitions and registry.

use super::models::{
    CLEARPROXY_URL, GEONODE_URL, GITHUB_LISTS_BASE_URL, GPROXYNET_URL, ONEPROXY_URL, PROXIFLY_URL,
    PROXYSCRAPE_CDN_URL, ScrapedProxy, VAKHOV_URL,
};
use super::scrapers::{
    sync_clearproxy, sync_geonode, sync_github_lists, sync_gproxynet, sync_oneproxy, sync_proxifly,
    sync_proxyscrape_cdn, sync_vakhov,
};

pub struct BuiltinProxySourceDef {
    pub id: &'static str,
    pub name: &'static str,
    pub url: &'static str,
    pub scraped_sources: &'static [&'static str],
    pub sync_fn:
        fn(&str) -> futures::future::BoxFuture<'static, crate::error::Result<Vec<ScrapedProxy>>>,
}

impl BuiltinProxySourceDef {
    pub fn find_by_id(id: &str) -> Option<&'static BuiltinProxySourceDef> {
        BUILTIN_PROXY_SOURCES.iter().find(|s| s.id == id)
    }
}

pub static BUILTIN_PROXY_SOURCES: &[BuiltinProxySourceDef] = &[
    BuiltinProxySourceDef {
        id: "builtin_proxifly",
        name: "Proxifly (Built-in)",
        url: PROXIFLY_URL,
        scraped_sources: &["proxifly"],
        sync_fn: |url| {
            let u = url.to_string();
            Box::pin(async move { sync_proxifly(&u).await })
        },
    },
    BuiltinProxySourceDef {
        id: "builtin_github",
        name: "GitHub Lists (Built-in)",
        url: GITHUB_LISTS_BASE_URL,
        scraped_sources: &[
            "iplocate",
            "hideip",
            "r00tee",
            "hookzof",
            "anonymouswork",
            "komutan234",
            "yuceltoluyag",
        ],
        sync_fn: |url| {
            let u = url.to_string();
            Box::pin(async move { sync_github_lists(&u).await })
        },
    },
    BuiltinProxySourceDef {
        id: "builtin_oneproxy",
        name: "1proxy (Built-in)",
        url: ONEPROXY_URL,
        scraped_sources: &["1proxy"],
        sync_fn: |url| {
            let u = url.to_string();
            Box::pin(async move { sync_oneproxy(&u).await })
        },
    },
    BuiltinProxySourceDef {
        id: "builtin_proxyscrape",
        name: "ProxyScrape (Built-in)",
        url: PROXYSCRAPE_CDN_URL,
        scraped_sources: &["proxyscrape_cdn"],
        sync_fn: |url| {
            let u = url.to_string();
            Box::pin(async move { sync_proxyscrape_cdn(&u).await })
        },
    },
    BuiltinProxySourceDef {
        id: "builtin_geonode",
        name: "Geonode (Built-in)",
        url: GEONODE_URL,
        scraped_sources: &["geonode"],
        sync_fn: |url| {
            let u = url.to_string();
            Box::pin(async move { sync_geonode(&u).await })
        },
    },
    BuiltinProxySourceDef {
        id: "builtin_clearproxy",
        name: "ClearProxy (Built-in)",
        url: CLEARPROXY_URL,
        scraped_sources: &["clearproxy"],
        sync_fn: |url| {
            let u = url.to_string();
            Box::pin(async move { sync_clearproxy(&u).await })
        },
    },
    BuiltinProxySourceDef {
        id: "builtin_vakhov",
        name: "Vakhov (Built-in)",
        url: VAKHOV_URL,
        scraped_sources: &["vakhov"],
        sync_fn: |url| {
            let u = url.to_string();
            Box::pin(async move { sync_vakhov(&u).await })
        },
    },
    BuiltinProxySourceDef {
        id: "builtin_gproxynet",
        name: "GProxyNet (Built-in)",
        url: GPROXYNET_URL,
        scraped_sources: &["gproxynet"],
        sync_fn: |url| {
            let u = url.to_string();
            Box::pin(async move { sync_gproxynet(&u).await })
        },
    },
];
