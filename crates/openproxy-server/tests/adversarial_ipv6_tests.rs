use axum::extract::{Json, State};
use openproxy_adapters::upstream::is_private_or_reserved;
use openproxy_server::handlers::admin::proxies::{
    CreateCustomProxyInput, UpdateProxyTestUrlInput, create_custom_proxy, update_proxy_test_url,
};
use openproxy_server::state::AppState;
use std::net::IpAddr;
use std::sync::Arc;

async fn create_test_state() -> (AppState, openproxy_db::testing::TempDir) {
    let temp_dir =
        openproxy_db::testing::TempDir::new("openproxy-ipv6-adversarial-test").expect("mkdir");
    let pool =
        Arc::new(openproxy_db::DbPool::open(&temp_dir.path().join("test.db")).expect("open pool"));
    {
        let mut w = pool.writer();
        openproxy_db::migrations::run(&mut w).expect("migrations");
    }
    let mk = openproxy_db::secrets::MasterKey::generate().expect("master key");
    let adapters = Arc::new(parking_lot::RwLock::new(Arc::new(
        openproxy_adapters::adapters::builtin_adapters(),
    )));
    let state = AppState::for_test(
        openproxy_core::AppConfig::default(),
        pool,
        Arc::new(mk),
        adapters,
    );
    (state, temp_dir)
}

#[tokio::test]
async fn test_update_proxy_test_url_public_ipv6_brackets() {
    let (state, _temp) = create_test_state().await;

    let valid_ipv6_urls = [
        "https://[2606:4700:4700::1111]/generate_204",
        "http://[2001:4860:4860::8888]:80/generate_204",
        "https://[2600:1406:3a::1]:443/test",
    ];

    for url in valid_ipv6_urls {
        let input = UpdateProxyTestUrlInput {
            proxy_test_url: url.to_string(),
        };
        let res = update_proxy_test_url(State(state.clone()), Json(input)).await;
        assert!(
            res.is_ok(),
            "Public IPv6 bracket URL '{url}' should be ACCEPTED, got: {:?}",
            res.err()
        );
    }
}

#[tokio::test]
async fn test_update_proxy_test_url_standard_private_ssrf_ipv6_brackets() {
    let (state, _temp) = create_test_state().await;

    let blocked_ipv6_urls = [
        "http://[::1]:8080/test",
        "https://[::1]/test",
        "http://[fc00::1]:8080/test",
        "http://[fd00::1]:8080/test",
        "http://[fd12:3456:789a::1]:80/test",
        "http://[fe80::1]:8080/test",
        "http://[::ffff:127.0.0.1]:8080/test",
        "http://[::ffff:10.0.0.1]:8080/test",
        "http://[::ffff:192.168.1.1]:8080/test",
        "http://[::ffff:169.254.169.254]:8080/test",
    ];

    for url in blocked_ipv6_urls {
        let input = UpdateProxyTestUrlInput {
            proxy_test_url: url.to_string(),
        };
        let res = update_proxy_test_url(State(state.clone()), Json(input)).await;
        assert!(
            res.is_err(),
            "Private/SSRF IPv6 bracket URL '{url}' MUST BE BLOCKED, but was accepted!"
        );
    }
}

#[tokio::test]
async fn test_update_proxy_test_url_malformed_bracket_urls() {
    let (state, _temp) = create_test_state().await;

    let malformed_urls = [
        "http://[::1/test",
        "http://::1]:80/test",
        "http://[]/test",
        "http://[[::1]]/test",
        "http://[invalid:ipv6:literal]:80/test",
    ];

    for url in malformed_urls {
        let input = UpdateProxyTestUrlInput {
            proxy_test_url: url.to_string(),
        };
        let res = update_proxy_test_url(State(state.clone()), Json(input)).await;
        assert!(
            res.is_err(),
            "Malformed URL '{url}' MUST be rejected, but was accepted!"
        );
    }
}

#[tokio::test]
async fn test_create_custom_proxy_public_ipv6_bracketed_and_raw() {
    let (state, _temp) = create_test_state().await;

    // Bracketed public IPv6
    let bracketed = CreateCustomProxyInput {
        host: "[2606:4700:4700::1111]".to_string(),
        port: 8080,
        r#type: "http".to_string(),
        country_code: None,
        username: None,
        password: None,
    };
    let res_bracketed = create_custom_proxy(State(state.clone()), Json(bracketed)).await;
    assert!(
        res_bracketed.is_ok(),
        "Public bracketed IPv6 proxy '[2606:4700:4700::1111]' should be accepted, got: {:?}",
        res_bracketed.err()
    );

    // Raw/unbracketed public IPv6
    let raw = CreateCustomProxyInput {
        host: "2606:4700:4700::1111".to_string(),
        port: 8080,
        r#type: "http".to_string(),
        country_code: None,
        username: None,
        password: None,
    };
    let res_raw = create_custom_proxy(State(state.clone()), Json(raw)).await;
    assert!(
        res_raw.is_ok(),
        "Public raw IPv6 proxy '2606:4700:4700::1111' should be accepted, got: {:?}",
        res_raw.err()
    );
}

#[tokio::test]
async fn test_create_custom_proxy_standard_private_ssrf_ipv6_variations() {
    let (state, _temp) = create_test_state().await;

    let blocked_hosts = [
        "[::1]",
        "::1",
        "[fc00::1]",
        "fc00::1",
        "[fd00::1]",
        "fd00::1",
        "[fe80::1]",
        "fe80::1",
        "[::ffff:127.0.0.1]",
        "::ffff:127.0.0.1",
        "[::ffff:10.0.0.1]",
        "::ffff:10.0.0.1",
        "[::ffff:192.168.1.1]",
        "::ffff:192.168.1.1",
        "[::ffff:169.254.169.254]",
        "::ffff:169.254.169.254",
    ];

    for host in blocked_hosts {
        let input = CreateCustomProxyInput {
            host: host.to_string(),
            port: 8080,
            r#type: "http".to_string(),
            country_code: None,
            username: None,
            password: None,
        };
        let res = create_custom_proxy(State(state.clone()), Json(input)).await;
        assert!(
            res.is_err(),
            "Host '{host}' MUST BE BLOCKED by SSRF protection, but was accepted!"
        );
    }
}

#[tokio::test]
async fn test_create_custom_proxy_malformed_bracket_inputs() {
    let (state, _temp) = create_test_state().await;

    let malformed_hosts = [
        "[::1",                  // unclosed bracket
        "::1]",                  // unopened bracket
        "[]",                    // empty bracket
        "   ",                   // whitespace
        "[2606:4700:4700::1111", // unclosed public
    ];

    for host in malformed_hosts {
        let input = CreateCustomProxyInput {
            host: host.to_string(),
            port: 8080,
            r#type: "http".to_string(),
            country_code: None,
            username: None,
            password: None,
        };
        let res = create_custom_proxy(State(state.clone()), Json(input)).await;
        assert!(
            res.is_err(),
            "Malformed host '{host}' MUST be rejected, but was accepted!"
        );
    }
}

#[tokio::test]
async fn test_ssrf_unspecified_ipv6_bypass_demonstration() {
    let (state, _temp) = create_test_state().await;

    // 1. Verify that is_private_or_reserved returns true for :: (UNSPECIFIED)
    let unspecified_ip = "::".parse::<IpAddr>().expect("valid ip");
    let is_blocked = is_private_or_reserved(&unspecified_ip);
    assert!(
        is_blocked,
        "is_private_or_reserved must return true for IPv6 UNSPECIFIED (::)"
    );

    // 2. Verify that update_proxy_test_url rejects http://[::]:8080/test
    let url_input = UpdateProxyTestUrlInput {
        proxy_test_url: "http://[::]:8080/test".to_string(),
    };
    let res_url = update_proxy_test_url(State(state.clone()), Json(url_input)).await;
    assert!(
        res_url.is_err(),
        "update_proxy_test_url must reject http://[::]:8080/test (SSRF block)!"
    );

    // 3. Verify that create_custom_proxy rejects host '[::]'
    let custom_proxy_input = CreateCustomProxyInput {
        host: "[::]".to_string(),
        port: 8080,
        r#type: "http".to_string(),
        country_code: None,
        username: None,
        password: None,
    };
    let res_proxy = create_custom_proxy(State(state.clone()), Json(custom_proxy_input)).await;
    assert!(
        res_proxy.is_err(),
        "create_custom_proxy must reject host '[::]' (SSRF block)!"
    );
}
